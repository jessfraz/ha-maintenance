"""Persistent instructions remain readable without becoming checkable tasks."""

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
from homeassistant.helpers import intent
from pytest_homeassistant_custom_component.common import MockUser

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import DOMAIN
from custom_components.home_maintenance.models import ScheduleKind
from custom_components.home_maintenance.services import async_actor

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
    references = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance_instructions"},
        blocking=True,
        return_response=True,
    )
    assert entity_items(references, "todo.maintenance_instructions") == []


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
