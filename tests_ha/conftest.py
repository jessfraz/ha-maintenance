"""Real Home Assistant and HTTP transport fixtures for the thin adapter."""

import asyncio
from collections.abc import AsyncIterator
from dataclasses import dataclass, field

import pytest
from aiohttp import web
from homeassistant.auth.const import GROUP_ID_USER
from homeassistant.core import HomeAssistant
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser

from custom_components.home_maintenance.const import (
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)

SUMMARY = {"active": 4, "overdue": 1, "due_soon": 2, "completed": 3}
ITEM_ID = "replace-filter"
OCCURRENCE_ID = "0193c69f-e5c4-7e93-b6cb-8172dacb617e"
NEXT_OCCURRENCE_ID = "0193c69f-e5c4-7e93-b6cb-8172dacb617f"
STEP_ID = "0193c69f-e5c4-7e93-b6cb-8172dacb6180"
SECOND_STEP_ID = "0193c69f-e5c4-7e93-b6cb-8172dacb6181"


def item_record(
    *,
    item_id: str = ITEM_ID,
    occurrence_id: str = OCCURRENCE_ID,
    revision: int = 1,
    summary: str = "Replace filter",
    description: str | None = None,
    checklist: list[dict[str, object]] | None = None,
) -> dict[str, object]:
    """A synthetic public-contract task unrelated to any household data."""
    return {
        "id": item_id,
        "name": "Replace filter",
        "summary": summary,
        "description": description,
        "checklist": [
            {"id": step["id"], "summary": step["summary"]} for step in checklist or []
        ],
        "timezone": "UTC",
        "location": None,
        "group": None,
        "schedule": {"days": 0, "months": 3, "years": 0},
        "instructions": ["Turn off the appliance", "Replace the filter"],
        "specs": [],
        "resources": [],
        "reorder_url": None,
        "revision": revision,
        "archived": False,
        "occurrence": {
            "id": occurrence_id,
            "due": "2026-10-15",
            "checklist": checklist or [],
        },
        "last_completion": None,
    }


@dataclass
class ReceivedRequest:
    """A request actually received by the upstream HTTP server."""

    method: str
    path: str
    query: dict[str, str]
    headers: dict[str, str]
    body: bytes


@dataclass
class Backend:
    """A controllable HTTP server without replacing adapter transport."""

    url: str = ""
    requests: list[ReceivedRequest] = field(default_factory=list)
    responses: dict[tuple[str, str], tuple[int, bytes, dict[str, str]]] = field(
        default_factory=dict
    )
    role: str = "integration"
    delay_seconds: float = 0

    async def handle(self, request: web.Request) -> web.Response:
        self.requests.append(
            ReceivedRequest(
                request.method,
                request.path,
                dict(request.query),
                dict(request.headers),
                await request.read(),
            )
        )
        if self.delay_seconds:
            await asyncio.sleep(self.delay_seconds)
        if response := self.responses.get((request.method, request.path)):
            status, body, headers = response
            return web.Response(status=status, body=body, headers=headers)
        if request.path == "/v1/system":
            return web.json_response({"credential_role": self.role})
        if request.path == "/v1/summary":
            return web.json_response(SUMMARY)
        if request.path == "/v1/items":
            return web.json_response({"items": [item_record()], "next_page": None})
        return web.json_response(item_record(), headers={"ETag": '"1"'})


@pytest.fixture(autouse=True)
def custom_integrations(enable_custom_integrations) -> None:
    """Enable the integration from this checkout."""


@pytest.fixture
async def hass_user(hass: HomeAssistant, local_auth) -> MockUser:
    """An authenticated user without administrator privileges."""
    group = await hass.auth.async_get_group(GROUP_ID_USER)
    return MockUser(groups=[group]).add_to_hass(hass)


@pytest.fixture
async def hass_user_token(hass: HomeAssistant, hass_user: MockUser) -> str:
    refresh_token = await hass.auth.async_create_refresh_token(
        hass_user, client_id="https://example.invalid"
    )
    return hass.auth.async_create_access_token(refresh_token)


@pytest.fixture
async def backend(aiohttp_server, socket_enabled) -> AsyncIterator[Backend]:
    upstream = Backend()
    app = web.Application()
    app.router.add_route("*", "/{path:.*}", upstream.handle)
    server = await aiohttp_server(app)
    upstream.url = str(server.make_url("")).rstrip("/")
    yield upstream


@pytest.fixture
async def entry(hass: HomeAssistant, backend: Backend) -> MockConfigEntry:
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "test-integration-token"},
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    return entry
