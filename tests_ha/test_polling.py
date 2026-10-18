"""The coordinator follows real cursor pages and rejects partial snapshots."""

import json

import pytest
from aiohttp import web
from homeassistant.core import HomeAssistant
from pytest_aiohttp.plugin import AiohttpServer
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import (
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
    MAX_RESPONSE_BYTES,
)

from .conftest import (
    NEXT_OCCURRENCE_ID,
    SUMMARY,
    Backend,
    entity_items,
    item_record,
)


async def test_native_entities_follow_the_entire_inventory(
    hass: HomeAssistant, aiohttp_server: AiohttpServer, socket_enabled: None
) -> None:
    requests: list[dict[str, str]] = []

    async def handle(request: web.Request) -> web.Response:
        assert request.headers["Authorization"] == "Bearer test-token"
        if request.path == "/v1/summary":
            return web.json_response(SUMMARY)
        requests.append(dict(request.query))
        if request.query.get("page_token") == "next+page":
            return web.json_response(
                {
                    "items": [
                        item_record(
                            item_id="second-filter",
                            occurrence_id=NEXT_OCCURRENCE_ID,
                            summary="Example second filter",
                        )
                    ],
                    "next_page": None,
                }
            )
        return web.json_response({"items": [item_record()], "next_page": "next+page"})

    app = web.Application()
    app.router.add_get("/{path:.*}", handle)
    server = await aiohttp_server(app)
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: str(server.make_url("")).rstrip("/"),
            CONF_TOKEN: "test-token",
        },
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    assert requests == [{"limit": "200"}, {"limit": "200", "page_token": "next+page"}]
    assert {item.id for item in entry.runtime_data.data.items} == {
        "replace-filter",
        "second-filter",
    }
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "2"
    calendar = await hass.services.async_call(
        "calendar",
        "get_events",
        {
            "entity_id": "calendar.maintenance",
            "start_date_time": "2026-10-15T00:00:00Z",
            "end_date_time": "2026-10-17T00:00:00Z",
        },
        blocking=True,
        return_response=True,
    )
    assert {
        event["summary"]
        for event in entity_items(calendar, "calendar.maintenance", "events")
    } == {
        "Replace filter",
        "Example second filter",
    }
    assert len(requests) == 2


@pytest.mark.parametrize(
    "page",
    [
        {"items": [item_record(), item_record()], "next_page": None},
        {"items": [{**item_record(), "revision": True}], "next_page": None},
        {"items": [{**item_record(), "status": "unknown"}], "next_page": None},
        {
            "items": [{**item_record(), "schedule": {"kind": "unknown"}}],
            "next_page": None,
        },
        {
            "items": [
                {
                    **item_record(),
                    "last_completion": {"completed_at": "2025-02-03T04:05:06"},
                }
            ],
            "next_page": None,
        },
        {
            "items": [{**item_record(), "occurrence": {"id": NEXT_OCCURRENCE_ID}}],
            "next_page": None,
        },
        {"items": [item_record(due="9999-12-31")], "next_page": None},
        {"items": [item_record()], "next_page": "repeated-cursor"},
        {"items": [], "next_page": False},
        {
            "items": [item_record(), item_record(item_id="second-filter")],
            "next_page": None,
        },
    ],
    ids=[
        "duplicate-id",
        "invalid-revision",
        "invalid-status",
        "invalid-schedule",
        "naive-completion-time",
        "missing-nullable-due",
        "unrepresentable-calendar-end",
        "cursor-loop",
        "invalid-cursor",
        "duplicate-uid",
    ],
)
async def test_invalid_page_preserves_previous_snapshot_and_marks_unavailable(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    page: dict[str, object],
) -> None:
    previous = entry.runtime_data.data
    backend.responses[("GET", "/v1/items")] = (200, json.dumps(page).encode(), {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert not entry.runtime_data.last_update_success
    assert entry.runtime_data.data == previous
    for entity_id in ("todo.maintenance", "calendar.maintenance"):
        state = hass.states.get(entity_id)
        assert state is not None
        assert state.state == "unavailable"


async def test_partial_inventory_auth_failure_requests_reauthentication(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    previous = entry.runtime_data.data
    backend.responses[("GET", "/v1/items")] = (401, b"{}", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert not entry.runtime_data.last_update_success
    assert entry.runtime_data.data == previous
    assert any(
        flow["context"].get("entry_id") == entry.entry_id
        for flow in hass.config_entries.flow.async_progress()
    )


async def test_single_oversized_item_preserves_snapshot_and_stops_retrying(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    previous = entry.runtime_data.data
    backend.responses[("GET", "/v1/items")] = (
        200,
        b"x" * (MAX_RESPONSE_BYTES + 1),
        {},
    )
    before = len(backend.requests)
    await entry.runtime_data.async_refresh()
    assert not entry.runtime_data.last_update_success
    assert entry.runtime_data.data == previous
    pages = [
        request for request in backend.requests[before:] if request.path == "/v1/items"
    ]
    assert [request.query["limit"] for request in pages] == [
        "200",
        "100",
        "50",
        "25",
        "12",
        "6",
        "3",
        "1",
    ]
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "unavailable"
