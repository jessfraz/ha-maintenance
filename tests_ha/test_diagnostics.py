"""Native diagnostics downloads contain operational aggregates only."""

import asyncio
import json
from uuid import NAMESPACE_URL, uuid5

import pytest
from homeassistant.components.homeassistant.exposed_entities import async_expose_entity
from homeassistant.config_entries import ConfigEntryState
from homeassistant.const import STATE_UNAVAILABLE, STATE_UNKNOWN
from homeassistant.core import HomeAssistant
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.update_coordinator import UpdateFailed
from homeassistant.setup import async_setup_component
from homeassistant.util.json import JsonObjectType
from pytest_homeassistant_custom_component.common import MockConfigEntry
from pytest_homeassistant_custom_component.typing import (
    ClientSessionGenerator,
    WebSocketGenerator,
)

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)

from .conftest import STEP_ID, SUMMARY, Backend, item_record


async def download(
    hass_client: ClientSessionGenerator, entry: MaintenanceConfigEntry
) -> JsonObjectType:
    client = await hass_client()
    response = await client.get(f"/api/diagnostics/config_entry/{entry.entry_id}")
    assert response.status == 200
    result = await response.json()
    assert isinstance(result, dict)
    data = result["data"]
    assert isinstance(data, dict)
    return data


async def test_native_diagnostics_download_and_discovery(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    hass_ws_client: WebSocketGenerator,
) -> None:
    """Core finds the diagnostics platform and serves it without backend IO."""
    client = await hass_client()
    before = len(backend.requests)
    response = await client.get(f"/api/diagnostics/config_entry/{entry.entry_id}")
    assert response.status == 200
    payload = await response.json()
    assert payload["integration_manifest"]["domain"] == DOMAIN
    assert "home_assistant" in payload
    assert payload["data"] == {
        "connection_mode": "standalone",
        "entry_state": "loaded",
        "last_update_success": True,
        "last_exception_type": None,
        "update_interval_seconds": 60,
        "snapshot_available": True,
        "assist_exposed": None,
        "summary": SUMMARY,
        "inventory": {
            "total": 1,
            "archived": 0,
            "needs_action": 1,
            "completed": 0,
            "dated": 1,
            "undated": 0,
            "schedule_kinds": {
                "once": 0,
                "standing": 0,
                "recurring": 1,
                "instruction": 0,
            },
        },
        "entities": {
            "registered": 6,
            "available": 6,
            "unavailable": 0,
            "unknown": 0,
            "not_loaded": 0,
        },
    }
    ws = await hass_ws_client(hass)
    await ws.send_json({"id": 1, "type": "diagnostics/get", "domain": DOMAIN})
    discovery = await ws.receive_json()
    assert discovery["success"] is True
    assert discovery["result"] == {
        "domain": DOMAIN,
        "handlers": {"config_entry": True, "device": False},
    }
    assert len(backend.requests) == before


async def test_native_diagnostics_requires_administrator(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    hass_user_token: str,
    hass_client: ClientSessionGenerator,
    hass_ws_client: WebSocketGenerator,
) -> None:
    client = await hass_client(hass_user_token)
    before = len(backend.requests)
    response = await client.get(f"/api/diagnostics/config_entry/{entry.entry_id}")
    assert response.status == 401
    ws = await hass_ws_client(hass, hass_user_token)
    await ws.send_json({"id": 1, "type": "diagnostics/get", "domain": DOMAIN})
    discovery = await ws.receive_json()
    assert discovery["success"] is False
    assert discovery["error"]["code"] == "unauthorized"
    assert len(backend.requests) == before


async def test_download_omits_private_data_including_exception_chains(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    entity_registry: er.EntityRegistry,
) -> None:
    """Both the allowlist and Core's actual envelope omit synthetic secrets."""
    record = item_record(
        item_id="excluded-example-task",
        summary="excluded-example-summary",
        description="excluded-example-description",
        location="excluded-example-location",
        checklist=[
            {
                "id": STEP_ID,
                "summary": "excluded-example-checklist",
                "completion": {
                    "completed_at": "2026-10-16T12:00:00Z",
                    "actor": {
                        "kind": "user",
                        "id": "excluded-example-user-id",
                        "name": "excluded-example-user-name",
                    },
                },
            }
        ],
    )
    record.update(
        {
            "name": "excluded-example-name",
            "instructions": ["excluded-example-instruction"],
            "specs": [
                {
                    "label": "excluded-example-spec",
                    "value": "excluded-example-spec-value",
                }
            ],
            "resources": [
                {
                    "label": "excluded-example-resource",
                    "url": "https://example.invalid/excluded-example-resource",
                }
            ],
            "reorder_url": "https://example.invalid/excluded-example-reorder",
            "home_assistant": [
                {
                    "kind": "entity",
                    "instance_id": "excluded-example-instance",
                    "id": "excluded-example-registry-id",
                }
            ],
        }
    )
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": [record], "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    entry.runtime_data.data.summary["excluded-example-extra-summary"] = 99
    hass.config_entries.async_update_entry(
        entry,
        title="excluded-example-entry-title",
        unique_id="excluded-example-unique-id",
        data={
            **entry.data,
            CONF_BACKEND_URL: "https://example.invalid/excluded-example-backend",
            CONF_TOKEN: "excluded-example-credential",
            CONF_ADDON_SLUG: "excluded-example-app-slug",
        },
        options={"nested": {"secret": "excluded-example-option"}},
    )
    entity_registry.async_update_entity(
        "todo.maintenance",
        new_entity_id="todo.excluded_example_entity",
        name="excluded-example-entity-name",
    )
    await hass.async_block_till_done()
    before = len(backend.requests)
    client = await hass_client()
    path = f"/api/diagnostics/config_entry/{entry.entry_id}"
    response = await client.get(path)
    assert response.status == 200
    body = await response.text()
    assert "excluded-example" not in body
    assert "excluded_example" not in body
    assert STEP_ID not in body
    assert "2026-10-16T12:00:00" not in body
    payload = json.loads(body)
    assert payload["data"]["connection_mode"] == "app"
    assert payload["data"]["snapshot_available"] is True
    assert payload["data"]["summary"] == SUMMARY
    error = UpdateFailed("excluded-example-error-message")
    error.__cause__ = RuntimeError("excluded-example-error-cause")
    error.__context__ = ValueError("excluded-example-error-context")
    error.add_note("excluded-example-error-note")
    entry.runtime_data.last_exception = error
    entry.runtime_data.last_update_success = False
    response = await client.get(path)
    assert response.status == 200
    body = await response.text()
    assert "excluded-example" not in body
    assert "excluded_example" not in body
    assert (await response.json())["data"]["last_exception_type"] == "UpdateFailed"
    assert len(backend.requests) == before


async def test_inventory_aggregates_all_supported_task_lifecycles(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    records = [
        item_record(schedule_kind="recurring"),
        item_record(schedule_kind="once", status="completed"),
        item_record(schedule_kind="standing", due=None),
        item_record(schedule_kind="instruction", due=None),
        item_record(schedule_kind="recurring", archived=True),
    ]
    for number, record in enumerate(records):
        record["id"] = f"example-task-{number}"
        record["occurrence"] = {
            "id": str(uuid5(NAMESPACE_URL, f"diagnostics-task-{number}")),
            "due": "2026-10-15" if number in {0, 1, 4} else None,
            "checklist": [],
        }
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": records, "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    before = len(backend.requests)
    result = await download(hass_client, entry)
    assert result["inventory"] == {
        "total": 5,
        "archived": 1,
        "needs_action": 4,
        "completed": 1,
        "dated": 3,
        "undated": 2,
        "schedule_kinds": {"once": 1, "standing": 1, "recurring": 2, "instruction": 1},
    }
    assert len(backend.requests) == before


async def test_entity_health_is_aggregate_and_scoped_to_the_entry(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    entity_registry: er.EntityRegistry,
) -> None:
    hass.states.async_set("sensor.maintenance_active", STATE_UNKNOWN)
    hass.states.async_set("sensor.maintenance_overdue", STATE_UNAVAILABLE)
    hass.states.async_remove("sensor.maintenance_completed")
    unrelated = MockConfigEntry(domain="test")
    unrelated.add_to_hass(hass)
    entity_registry.async_get_or_create(
        "sensor",
        "test",
        "excluded-example-unrelated",
        config_entry=unrelated,
    )
    before = len(backend.requests)
    result = await download(hass_client, entry)
    assert result["entities"] == {
        "registered": 6,
        "available": 3,
        "unavailable": 1,
        "unknown": 1,
        "not_loaded": 1,
    }
    assert len(backend.requests) == before


async def test_assist_exposure_follows_the_renamed_gateway(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    entity_registry: er.EntityRegistry,
) -> None:
    entity_registry.async_update_entity(
        "todo.maintenance", new_entity_id="todo.example_tasks"
    )
    await hass.async_block_till_done()
    async_expose_entity(hass, "conversation", "todo.example_tasks", False)
    before = len(backend.requests)
    assert (await download(hass_client, entry))["assist_exposed"] is False
    async_expose_entity(hass, "conversation", "todo.example_tasks", True)
    assert (await download(hass_client, entry))["assist_exposed"] is True
    assert len(backend.requests) == before


async def test_uncached_exposure_download_does_not_write_registry_or_backend(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    entity_registry: er.EntityRegistry,
) -> None:
    """Include the real deferred catalog listener in the read-only contract."""
    entity_registry.async_update_entity_options("todo.maintenance", "conversation", {})
    await asyncio.sleep(0.55)
    await hass.async_block_till_done(wait_background_tasks=True)
    gateway = entity_registry.async_get("todo.maintenance")
    assert gateway is not None
    before_options = gateway.options
    assert "should_expose" not in before_options.get("conversation", {})
    before_requests = len(backend.requests)
    result = await download(hass_client, entry)
    await asyncio.sleep(0.55)
    await hass.async_block_till_done(wait_background_tasks=True)
    gateway = entity_registry.async_get("todo.maintenance")
    assert gateway is not None
    assert {
        "options": gateway.options,
        "backend_calls": len(backend.requests),
    } == {"options": before_options, "backend_calls": before_requests}
    assert result["assist_exposed"] is None


async def test_failed_refresh_does_not_report_old_counts_as_current(
    entry: MaintenanceConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    cached = entry.runtime_data.data
    backend.responses[("GET", "/v1/summary")] = (503, b"excluded-example-body", {})
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.data is cached
    before = len(backend.requests)
    result = await download(hass_client, entry)
    assert result["snapshot_available"] is False
    assert result["last_update_success"] is False
    assert result["last_exception_type"] == "UpdateFailed"
    assert result["summary"] is None
    assert result["inventory"] is None
    assert result["entities"] == {
        "registered": 6,
        "available": 0,
        "unavailable": 6,
        "unknown": 0,
        "not_loaded": 0,
    }
    assert len(backend.requests) == before


async def test_unloaded_entry_has_no_current_snapshot(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    assert await hass.config_entries.async_unload(entry.entry_id)
    before = len(backend.requests)
    result = await download(hass_client, entry)
    assert result["entry_state"] == ConfigEntryState.NOT_LOADED.value
    assert result["snapshot_available"] is False
    assert result["last_update_success"] is None
    assert result["last_exception_type"] is None
    assert result["update_interval_seconds"] is None
    assert result["summary"] is None
    assert result["inventory"] is None
    assert len(backend.requests) == before


@pytest.mark.parametrize("failed_setup", [False, True])
async def test_diagnostics_without_runtime_data(
    hass: HomeAssistant,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    failed_setup: bool,
) -> None:
    if not failed_setup:
        # Discover the platform before adding an entry that has never been set up.
        assert await async_setup_component(hass, DOMAIN, {})
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: backend.url,
            CONF_TOKEN: "excluded-example-credential",
        },
    )
    entry.add_to_hass(hass)
    if failed_setup:
        backend.responses[("GET", "/v1/summary")] = (503, b"excluded-example-body", {})
        assert not await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        assert entry.state is ConfigEntryState.SETUP_RETRY
    assert not hasattr(entry, "runtime_data")
    before = len(backend.requests)
    result = await download(hass_client, entry)
    assert result == {
        "connection_mode": "standalone",
        "entry_state": entry.state.value,
        "snapshot_available": False,
        "assist_exposed": None,
        "last_update_success": None,
        "last_exception_type": None,
        "update_interval_seconds": None,
        "summary": None,
        "inventory": None,
        "entities": {
            "registered": 0,
            "available": 0,
            "unavailable": 0,
            "unknown": 0,
            "not_loaded": 0,
        },
    }
    assert len(backend.requests) == before
