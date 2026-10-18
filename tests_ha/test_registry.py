"""Real HA registries provide bounded metadata and stable task associations."""

import asyncio
import json
from datetime import timedelta

import pytest
from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import instance_id
from homeassistant.util import dt as dt_util
from pytest_homeassistant_custom_component.common import async_fire_time_changed
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import MAX_REGISTRY_BYTES
from custom_components.home_maintenance.registry import registry_payload

from .conftest import (
    ITEM_ID,
    NEXT_OCCURRENCE_ID,
    OCCURRENCE_ID,
    Backend,
    ReceivedRequest,
    RegisteredObjects,
    entity_items,
    item_record,
)

REGISTRY_PATH = "/v1/integration/registry"


def reports(backend: Backend) -> list[ReceivedRequest]:
    return [request for request in backend.requests if request.path == REGISTRY_PATH]


async def test_catalog_uses_real_registry_ids_without_hardware_or_state_inventory(
    hass: HomeAssistant,
    registered_objects: RegisteredObjects,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    report_request = reports(backend)[-1]
    report = json.loads(report_request.body)
    assert report["status"] == "ready"
    assert report["instance_id"] == await instance_id.async_get(hass)
    objects = {(target["kind"], target["id"]): target for target in report["targets"]}
    assert objects[("device", registered_objects.device_id)] == {
        "kind": "device",
        "id": registered_objects.device_id,
        "name": "Example appliance",
        "disabled": False,
    }
    assert objects[("entity", registered_objects.registry_id)] == {
        "kind": "entity",
        "id": registered_objects.registry_id,
        "entity_id": registered_objects.entity_id,
        "device_id": registered_objects.device_id,
        "name": "Example temperature",
        "disabled": False,
    }
    assert "excluded-example" not in report_request.body.decode()
    assert report_request.headers["X-Maintenance-System-Actor"] == "ha:integration"
    assert "X-Maintenance-Actor-Id" not in report_request.headers
    assert "X-Maintenance-Owner" not in report_request.headers


async def test_registry_links_follow_renames_deletions_and_shared_item_references(
    hass: HomeAssistant,
    registered_objects: RegisteredObjects,
    entity_registry: er.EntityRegistry,
    device_registry: dr.DeviceRegistry,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    identity = await instance_id.async_get(hass)
    references = [
        {"kind": "device", "instance_id": identity, "id": registered_objects.device_id},
        {
            "kind": "entity",
            "instance_id": identity,
            "id": registered_objects.registry_id,
        },
        {
            "kind": "entity",
            "instance_id": "another-instance",
            "id": registered_objects.registry_id,
        },
    ]
    records = [
        {**item_record(), "home_assistant": references},
        {
            **item_record(
                item_id="second-example-task", occurrence_id=NEXT_OCCURRENCE_ID
            ),
            "home_assistant": references,
        },
    ]
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": records, "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()

    async def descriptions() -> list[str]:
        result = await hass.services.async_call(
            "todo",
            "get_items",
            {"entity_id": "todo.maintenance"},
            blocking=True,
            return_response=True,
        )
        return [
            str(item["description"])
            for item in entity_items(result, "todo.maintenance")
        ]

    original = await descriptions()
    assert len(original) == 2
    assert original[0] == original[1]
    assert f"/config/devices/device/{registered_objects.device_id}" in original[0]
    assert f"more-info-entity-id={registered_objects.entity_id}" in original[0]
    assert "another Home Assistant instance" in original[0]
    calendar = await hass.services.async_call(
        "calendar",
        "get_events",
        {
            "entity_id": "calendar.maintenance",
            "start_date_time": "2026-10-14T00:00:00Z",
            "end_date_time": "2026-10-17T00:00:00Z",
        },
        blocking=True,
        return_response=True,
    )
    assert [
        event["description"]
        for event in entity_items(calendar, "calendar.maintenance", "events")
    ] == original
    # Core copies the displayed description into its partial completion update.
    # The enriched producer must also own the completion comparison.
    await hass.services.async_call(
        "todo",
        "update_item",
        {
            "entity_id": "todo.maintenance",
            "item": OCCURRENCE_ID,
            "status": "completed",
        },
        blocking=True,
    )
    assert any(
        request.path == f"/v1/items/{ITEM_ID}/completions"
        for request in backend.requests
    )
    entity_registry.async_update_entity(
        registered_objects.entity_id,
        new_entity_id="sensor.renamed_example",
        name="Renamed example sensor",
    )
    device_registry.async_update_device(
        registered_objects.device_id, name_by_user="Renamed example appliance"
    )
    # Core still holds the previous displayed description during the registry
    # debounce. An unrelated rename must not prevent completing that task.
    await hass.services.async_call(
        "todo",
        "update_item",
        {
            "entity_id": "todo.maintenance",
            "item": NEXT_OCCURRENCE_ID,
            "status": "completed",
        },
        blocking=True,
    )
    await asyncio.sleep(0.55)
    await hass.async_block_till_done(wait_background_tasks=True)
    renamed = await descriptions()
    assert "Renamed example sensor" in renamed[0]
    assert "Renamed example appliance" in renamed[0]
    assert "more-info-entity-id=sensor.renamed_example" in renamed[0]
    report = json.loads(reports(backend)[-1].body)
    resolved = next(
        target
        for target in report["targets"]
        if target["id"] == registered_objects.registry_id
    )
    assert resolved["entity_id"] == "sensor.renamed_example"
    entity_registry.async_remove("sensor.renamed_example")
    device_registry.async_remove_device(registered_objects.device_id)
    await asyncio.sleep(0.55)
    await hass.async_block_till_done(wait_background_tasks=True)
    removed = await descriptions()
    assert removed[0].count("removed from Home Assistant") == 2
    assert "more-info-entity-id=" not in removed[0]
    assert len(entry.runtime_data.data.items[0].home_assistant) == 3


async def test_registry_labels_and_origins_are_safe_display_data(
    hass: HomeAssistant,
    registered_objects: RegisteredObjects,
    entity_registry: er.EntityRegistry,
) -> None:
    hass.config.external_url = "https://example.invalid"
    hass.config.internal_url = "http://internal.example.invalid:8123"
    identity = await instance_id.async_get(hass)
    entity_registry.async_update_entity(
        registered_objects.entity_id, name="x" * 201 + "\n"
    )
    report = json.loads(registry_payload(hass, identity))
    assert report["base_url"] == "https://example.invalid"
    target = next(
        target
        for target in report["targets"]
        if target["id"] == registered_objects.registry_id
    )
    assert target["name"] == "x" * 199 + "…"
    hass.config.external_url = None
    assert (
        json.loads(registry_payload(hass, identity))["base_url"]
        == "http://internal.example.invalid:8123"
    )
    hass.config.external_url = "https://userinfo:password@example.invalid"
    assert json.loads(registry_payload(hass, identity))["base_url"] is None


@pytest.mark.parametrize("limit", ["targets", "bytes"])
async def test_catalog_limit_reports_unavailable_instead_of_truncating(
    hass: HomeAssistant,
    registered_objects: RegisteredObjects,
    monkeypatch: pytest.MonkeyPatch,
    limit: str,
) -> None:
    attribute = "MAX_REGISTRY_TARGETS" if limit == "targets" else "MAX_REGISTRY_BYTES"
    monkeypatch.setattr(f"custom_components.home_maintenance.registry.{attribute}", 1)
    identity = await instance_id.async_get(hass)
    assert json.loads(registry_payload(hass, identity)) == {
        "status": "unavailable",
        "instance_id": identity,
        "reason": "limit_exceeded",
    }


async def test_periodic_registry_reports_stop_on_unload_and_listeners_are_removed(
    hass: HomeAssistant,
    registered_objects: RegisteredObjects,
    entity_registry: er.EntityRegistry,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    coordinator = entry.runtime_data
    before = len(reports(backend))
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(seconds=65))
    await hass.async_block_till_done(wait_background_tasks=True)
    assert len(reports(backend)) == before + 1
    entity_registry.async_update_entity(
        registered_objects.entity_id, name="Pending example rename"
    )
    assert await hass.config_entries.async_unload(entry.entry_id)
    await asyncio.sleep(0.55)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert len(reports(backend)) == before + 1
    await coordinator.async_report_registry()
    assert len(reports(backend)) == before + 1


@pytest.mark.parametrize("status", [401, 413, 500])
async def test_optional_registry_failure_preserves_native_tasks(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend, status: int
) -> None:
    backend.responses[("PUT", REGISTRY_PATH)] = (status, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.last_update_success
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "1"


async def test_registry_read_bridge_is_admin_only_and_cannot_publish_catalogs(
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    user = await hass_client(hass_user_token)
    before = len(backend.requests)
    response = await user.get("/api/home_maintenance/v1/home-assistant/registry")
    assert response.status == 403
    assert len(backend.requests) == before
    admin = await hass_client()
    response = await admin.get("/api/home_maintenance/v1/home-assistant/registry")
    assert response.status == 200
    response = await admin.put("/api/home_maintenance/v1/integration/registry", json={})
    assert response.status == 404


async def test_registry_bridge_still_bounds_large_responses(
    hass_client: ClientSessionGenerator,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    backend.responses[("GET", "/v1/home-assistant/registry")] = (
        200,
        b"x" * (MAX_REGISTRY_BYTES + 1),
        {},
    )
    admin = await hass_client()
    response = await admin.get("/api/home_maintenance/v1/home-assistant/registry")
    assert response.status == 502
