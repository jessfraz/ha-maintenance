"""Persistent instructions remain readable without becoming checkable tasks."""

import asyncio
import json

import pytest
from homeassistant.components.todo.intent import (
    INTENT_LIST_COMPLETE_ITEM,
    async_setup_intents,
)
from homeassistant.core import Context, HomeAssistant
from homeassistant.exceptions import (
    HomeAssistantError,
    ServiceValidationError,
    Unauthorized,
)
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import intent
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import (
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)
from custom_components.home_maintenance.models import ScheduleKind
from custom_components.home_maintenance.services import async_actor
from custom_components.home_maintenance.todo import MaintenanceInstructions

from .conftest import (
    NEXT_OCCURRENCE_ID,
    OCCURRENCE_ID,
    STEP_ID,
    Backend,
    entity_items,
    item_record,
)


def instruction_record(*, archived: bool = False) -> dict[str, object]:
    """A fictional reference with no relationship to the household's content."""
    return item_record(
        item_id="example-instruction",
        occurrence_id=NEXT_OCCURRENCE_ID,
        summary="Keep this example reference",
        description="A permanent reference for everyone to read.",
        schedule_kind="instruction",
        due=None,
        archived=archived,
    )


async def async_set_inventory(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    items: list[dict[str, object]],
) -> None:
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": items, "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()


@pytest.mark.parametrize("change", ["archive", "once", "missing"])
async def test_instructions_reappear_with_saved_identity_and_name(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    entity_registry: er.EntityRegistry,
    change: str,
) -> None:
    await async_set_inventory(hass, entry, backend, [instruction_record()])
    registered = entity_registry.async_get("todo.maintenance_instructions")
    assert registered is not None
    entity_registry.async_update_entity(
        registered.entity_id,
        new_entity_id="todo.example_references",
        name="Example reference list",
    )
    await hass.async_block_till_done()
    instruction = instruction_record()
    if change == "archive":
        instruction["archived"] = True
    elif change == "once":
        instruction["schedule"] = {"kind": "once"}
    await async_set_inventory(
        hass,
        entry,
        backend,
        [] if change == "missing" else [instruction],
    )
    assert hass.states.get("todo.example_references") is None
    assert entity_registry.async_get("todo.example_references") is None
    assert hass.states.get("todo.maintenance") is not None
    await async_set_inventory(hass, entry, backend, [instruction_record()])
    restored = entity_registry.async_get("todo.example_references")
    assert restored is not None
    assert restored.id == registered.id
    assert restored.unique_id == registered.unique_id
    assert restored.name == "Example reference list"
    assert hass.states.get("todo.example_references") is not None
    assert hass.states.get("todo.maintenance_instructions") is None
    response = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": restored.entity_id},
        blocking=True,
        return_response=True,
    )
    assert len(entity_items(response, restored.entity_id)) == 1


async def test_list_stays_until_the_last_instruction_is_archived(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    second = item_record(
        item_id="example-second-reference",
        occurrence_id=OCCURRENCE_ID,
        schedule_kind="instruction",
        due=None,
    )
    await async_set_inventory(hass, entry, backend, [instruction_record(), second])
    await async_set_inventory(
        hass,
        entry,
        backend,
        [instruction_record(archived=True), second],
    )
    assert hass.states.get("todo.maintenance_instructions") is not None
    second["archived"] = True
    await async_set_inventory(
        hass,
        entry,
        backend,
        [instruction_record(archived=True), second],
    )
    assert hass.states.get("todo.maintenance_instructions") is None


async def test_failed_inventory_does_not_remove_existing_instructions(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    entity_registry: er.EntityRegistry,
) -> None:
    await async_set_inventory(hass, entry, backend, [instruction_record()])
    registered = entity_registry.async_get("todo.maintenance_instructions")
    backend.responses[("GET", "/v1/items")] = (500, b"{}", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    state = hass.states.get("todo.maintenance_instructions")
    assert state is not None and state.state == "unavailable"
    assert entity_registry.async_get(state.entity_id) == registered
    await async_set_inventory(hass, entry, backend, [])
    assert hass.states.get("todo.maintenance_instructions") is None


async def test_user_disabled_instructions_stay_disabled_after_archive_and_restore(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    entity_registry: er.EntityRegistry,
) -> None:
    await async_set_inventory(hass, entry, backend, [instruction_record()])
    registered = entity_registry.async_get("todo.maintenance_instructions")
    assert registered is not None
    entity_registry.async_update_entity(
        registered.entity_id,
        disabled_by=er.RegistryEntryDisabler.USER,
    )
    await hass.async_block_till_done()
    await async_set_inventory(hass, entry, backend, [])
    assert entity_registry.async_get(registered.entity_id) is None
    await async_set_inventory(hass, entry, backend, [instruction_record()])
    restored = entity_registry.async_get(registered.entity_id)
    assert restored is not None and restored.id == registered.id
    assert restored.disabled_by is er.RegistryEntryDisabler.USER
    assert hass.states.get(registered.entity_id) is None


@pytest.mark.parametrize("unload", [False, True])
@pytest.mark.parametrize("stage", ["prepare", "added"])
async def test_inflight_add_does_not_leave_a_list_after_empty_update_or_unload(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    entity_registry: er.EntityRegistry,
    monkeypatch: pytest.MonkeyPatch,
    unload: bool,
    stage: str,
) -> None:
    entered = asyncio.Event()
    release = asyncio.Event()
    original = (
        MaintenanceInstructions.async_prepare_to_add_to_hass
        if stage == "prepare"
        else MaintenanceInstructions.async_added_to_hass
    )

    async def paused_prepare(entity: MaintenanceInstructions) -> None:
        entered.set()
        await release.wait()
        await original(entity)

    monkeypatch.setattr(
        MaintenanceInstructions,
        "async_prepare_to_add_to_hass" if stage == "prepare" else "async_added_to_hass",
        paused_prepare,
    )
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": [instruction_record()], "next_page": None}).encode(),
        {},
    )
    coordinator = entry.runtime_data
    await coordinator.async_refresh()
    await asyncio.wait_for(entered.wait(), timeout=5)
    if unload:
        assert await hass.config_entries.async_unload(entry.entry_id)
    else:
        backend.responses[("GET", "/v1/items")] = (
            200,
            json.dumps({"items": [], "next_page": None}).encode(),
            {},
        )
        await coordinator.async_refresh()
    release.set()
    await hass.async_block_till_done()
    assert hass.states.get("todo.maintenance_instructions") is None
    if not unload:
        assert (
            entity_registry.async_get_entity_id(
                "todo", DOMAIN, f"{entry.entry_id}_instructions"
            )
            is None
        )
    if unload:
        # A stale coordinator notification cannot recreate an unloaded list.
        coordinator.async_update_listeners()
        await hass.async_block_till_done()
        assert hass.states.get("todo.maintenance_instructions") is None
        monkeypatch.setattr(
            MaintenanceInstructions,
            "async_prepare_to_add_to_hass"
            if stage == "prepare"
            else "async_added_to_hass",
            original,
        )
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        assert hass.states.get("todo.maintenance_instructions") is not None
        await async_set_inventory(hass, entry, backend, [])
        assert hass.states.get("todo.maintenance_instructions") is None


async def test_empty_inventory_has_no_instructions_list(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    entity_registry: er.EntityRegistry,
) -> None:
    assert hass.states.get("todo.maintenance") is not None
    assert hass.states.get("todo.maintenance_instructions") is None
    assert (
        entity_registry.async_get_entity_id(
            "todo", DOMAIN, f"{entry.entry_id}_instructions"
        )
        is None
    )


@pytest.mark.parametrize("archived", [False, True])
async def test_upgrade_removes_empty_instructions_registry_and_restored_state(
    hass: HomeAssistant,
    backend: Backend,
    entity_registry: er.EntityRegistry,
    archived: bool,
) -> None:
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "test-integration-token"},
    )
    entry.add_to_hass(hass)
    previous = entity_registry.async_get_or_create(
        "todo",
        DOMAIN,
        f"{entry.entry_id}_instructions",
        config_entry=entry,
        suggested_object_id="example_instructions",
        original_name="Example instructions",
    )
    previous.write_unavailable_state(hass)
    assert hass.states.get(previous.entity_id) is not None
    if archived:
        backend.responses[("GET", "/v1/items")] = (
            200,
            json.dumps(
                {"items": [instruction_record(archived=True)], "next_page": None}
            ).encode(),
            {},
        )
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    assert entity_registry.async_get(previous.entity_id) is None
    assert hass.states.get(previous.entity_id) is None
    assert hass.states.get("todo.maintenance") is not None


async def test_native_instruction_reference_is_read_only_and_tracks_refresh(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps(
            {"items": [item_record(), instruction_record()], "next_page": None}
        ).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert entry.runtime_data.data.items[1].schedule_kind is ScheduleKind.INSTRUCTION
    state = hass.states.get("todo.maintenance_instructions")
    assert state is not None
    assert state.state == "0"
    assert state.attributes["supported_features"] == 0
    tasks = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    assert [item["uid"] for item in entity_items(tasks, "todo.maintenance")] == [
        OCCURRENCE_ID
    ]
    references = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance_instructions"},
        blocking=True,
        return_response=True,
    )
    items = entity_items(references, "todo.maintenance_instructions")
    assert len(items) == 1
    assert items[0]["uid"] == NEXT_OCCURRENCE_ID
    assert items[0]["summary"] == "Keep this example reference"
    assert "A permanent reference" in str(items[0]["description"])
    assert "status" not in items[0]
    assert "due" not in items[0]
    before = len(backend.requests)
    for service, extra in (
        ("update_item", {"item": NEXT_OCCURRENCE_ID, "status": "completed"}),
        ("remove_item", {"item": NEXT_OCCURRENCE_ID}),
        ("remove_completed_items", {}),
    ):
        with pytest.raises(HomeAssistantError):
            await hass.services.async_call(
                "todo",
                service,
                {"entity_id": "todo.maintenance_instructions", **extra},
                blocking=True,
            )
    await async_setup_intents(hass)
    with pytest.raises(intent.IntentHandleError, match="not found"):
        await intent.async_handle(
            hass,
            "conversation",
            INTENT_LIST_COMPLETE_ITEM,
            {
                "name": {"value": "Maintenance Instructions"},
                "item": {"value": "Keep this example reference"},
            },
        )
    assert len(backend.requests) == before
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps(
            {
                "items": [item_record(), instruction_record(archived=True)],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert hass.states.get("todo.maintenance_instructions") is None


@pytest.mark.parametrize("service", ["complete", "set_checklist_step"])
async def test_native_actions_reject_instructions_before_transport(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    service: str,
) -> None:
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": [instruction_record()], "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    before = len(backend.requests)
    data: dict[str, object] = {
        "item_id": "example-instruction",
        "occurrence_id": NEXT_OCCURRENCE_ID,
    }
    if service == "set_checklist_step":
        data.update({"step_id": STEP_ID, "completed": True})
    with pytest.raises(ServiceValidationError, match="cannot be completed"):
        await hass.services.async_call(DOMAIN, service, data, blocking=True)
    assert len(backend.requests) == before


async def test_native_actor_distinguishes_owner_admin_household_and_automation(
    hass: HomeAssistant,
    hass_owner_user: MockUser,
    hass_admin_user: MockUser,
    hass_user: MockUser,
) -> None:
    for user in (hass_owner_user, hass_admin_user, hass_user):
        actor = await async_actor(hass, Context(user_id=user.id))
        assert actor.owner == user.is_owner
        assert ("X-Maintenance-Owner" in actor.headers()) == user.is_owner
    assert "X-Maintenance-Owner" not in (await async_actor(hass, Context())).headers()
    await hass.auth.async_deactivate_user(hass_admin_user)
    with pytest.raises(Unauthorized):
        await async_actor(hass, Context(user_id=hass_admin_user.id))
