"""Real Home Assistant and HTTP transport fixtures for the thin adapter."""

import asyncio
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from typing import Literal

import pytest
from aiohttp import web
from homeassistant.auth.const import GROUP_ID_USER
from homeassistant.auth.providers import AuthProvider
from homeassistant.core import HomeAssistant, ServiceResponse
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import entity_registry as er
from homeassistant.util.json import JsonObjectType
from pytest_aiohttp.plugin import AiohttpServer
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser

from custom_components.home_maintenance import MaintenanceConfigEntry
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


@dataclass(frozen=True)
class RegisteredObjects:
    """Stable keys for fictional objects created through HA's real registries."""

    device_id: str
    registry_id: str
    entity_id: str


@pytest.fixture
def registered_objects(
    hass: HomeAssistant,
    device_registry: dr.DeviceRegistry,
    entity_registry: er.EntityRegistry,
) -> RegisteredObjects:
    source = MockConfigEntry(domain="test")
    source.add_to_hass(hass)
    device = device_registry.async_get_or_create(
        config_entry_id=source.entry_id,
        identifiers={("test", "example-appliance")},
        name="Example appliance",
        serial_number="excluded-example-serial",
        model="excluded-example-model",
    )
    entity = entity_registry.async_get_or_create(
        "sensor",
        "test",
        "example-temperature",
        config_entry=source,
        device_id=device.id,
        suggested_object_id="example_temperature",
        original_name="Example temperature",
    )
    hass.states.async_set(
        entity.entity_id,
        "excluded-example-state",
        {
            "friendly_name": "Example temperature",
            "private_attribute": "excluded-example-attribute",
        },
    )
    return RegisteredObjects(device.id, entity.id, entity.entity_id)


def entity_items(
    response: ServiceResponse, entity_id: str, collection: str = "items"
) -> list[JsonObjectType]:
    """Assert the shape of a native entity service's returned collection."""
    assert response is not None
    entity = response[entity_id]
    assert isinstance(entity, dict)
    items = entity[collection]
    assert isinstance(items, list)
    records: list[JsonObjectType] = []
    for item in items:
        assert isinstance(item, dict)
        records.append(item)
    return records


def item_record(
    *,
    item_id: str = ITEM_ID,
    occurrence_id: str = OCCURRENCE_ID,
    revision: int = 1,
    summary: str = "Replace filter",
    description: str | None = None,
    location: str | None = None,
    due: str | None = "2026-10-15",
    status: Literal["needs_action", "completed"] = "needs_action",
    schedule_kind: Literal["once", "standing", "recurring", "instruction"] = (
        "recurring"
    ),
    archived: bool = False,
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
        "location": location,
        "group": None,
        "schedule": (
            {
                "kind": "recurring",
                "interval": {"days": 0, "months": 3, "years": 0},
            }
            if schedule_kind == "recurring"
            else {"kind": schedule_kind}
        ),
        "instructions": ["Turn off the appliance", "Replace the filter"],
        "specs": [],
        "resources": [],
        "reorder_url": None,
        "revision": revision,
        "archived": archived,
        "status": status,
        "occurrence": {
            "id": occurrence_id,
            "due": due,
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
        if request.path == "/v1/integration/registry" and request.method == "PUT":
            return web.Response(status=204)
        if request.path == "/v1/system":
            return web.json_response({"credential_role": self.role})
        if request.path == "/v1/summary":
            return web.json_response(SUMMARY)
        if request.path == "/v1/items":
            return web.json_response({"items": [item_record()], "next_page": None})
        return web.json_response(item_record(), headers={"ETag": '"1"'})


@pytest.fixture(autouse=True)
def custom_integrations(enable_custom_integrations: None) -> None:
    """Enable the integration from this checkout."""


@pytest.fixture
async def hass_user(hass: HomeAssistant, local_auth: AuthProvider) -> MockUser:
    """An authenticated user without administrator privileges."""
    group = await hass.auth.async_get_group(GROUP_ID_USER)
    assert group is not None
    return MockUser(groups=[group]).add_to_hass(hass)


@pytest.fixture
async def hass_user_token(hass: HomeAssistant, hass_user: MockUser) -> str:
    refresh_token = await hass.auth.async_create_refresh_token(
        hass_user, client_id="https://example.invalid"
    )
    return hass.auth.async_create_access_token(refresh_token)


@pytest.fixture
async def backend(
    aiohttp_server: AiohttpServer, socket_enabled: None
) -> AsyncIterator[Backend]:
    upstream = Backend()
    app = web.Application()
    app.router.add_route("*", "/{path:.*}", upstream.handle)
    server = await aiohttp_server(app)
    upstream.url = str(server.make_url("")).rstrip("/")
    yield upstream


@pytest.fixture
async def entry(hass: HomeAssistant, backend: Backend) -> MaintenanceConfigEntry:
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "test-integration-token"},
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    return entry
