"""Exercise native sensors, to-do behavior and service identity in real HA."""

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
from homeassistant.helpers import intent
from pytest_homeassistant_custom_component.common import MockUser
from pytest_homeassistant_custom_component.typing import WebSocketGenerator

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import DOMAIN, SUMMARY_KEYS

from .conftest import (
    ITEM_ID,
    NEXT_OCCURRENCE_ID,
    OCCURRENCE_ID,
    SUMMARY,
    Backend,
    entity_items,
    item_record,
)


async def test_native_sensors_and_todo_list(
    hass: HomeAssistant, entry: MaintenanceConfigEntry
) -> None:
    for key in SUMMARY_KEYS:
        state = hass.states.get(f"sensor.maintenance_{key}")
        assert state is not None
        assert state.state == str(SUMMARY[key])
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "1"
    # Only completion is supported, so definitions cannot be edited via todo.
    assert state.attributes["supported_features"] == 4
    result = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    assert result == {
        "todo.maintenance": {
            "items": [
                {
                    "summary": "Replace filter",
                    "uid": OCCURRENCE_ID,
                    "status": "needs_action",
                    "due": "2026-10-15",
                    "description": (
                        "Instructions\n1. Turn off the appliance\n2. Replace the filter"
                    ),
                }
            ]
        }
    }


async def test_native_entities_follow_successive_snapshots(
    hass: HomeAssistant,
    hass_ws_client: WebSocketGenerator,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    """Counts, service reads, and subscriptions follow updated and empty lists."""
    client = await hass_ws_client(hass)
    await client.send_json(
        {"id": 1, "type": "todo/item/subscribe", "entity_id": "todo.maintenance"}
    )
    assert (await client.receive_json())["success"]
    assert (await client.receive_json())["event"]["items"][0]["uid"] == OCCURRENCE_ID

    updated = item_record(
        occurrence_id=NEXT_OCCURRENCE_ID,
        revision=2,
        summary="Service the example filter",
        description="Use the example replacement.",
        due="2026-10-17",
    )
    expected_items = [
        {
            "summary": "Service the example filter",
            "uid": NEXT_OCCURRENCE_ID,
            "status": "needs_action",
            "due": "2026-10-17",
            "description": (
                "Use the example replacement.\n\n"
                "Instructions\n1. Turn off the appliance\n2. Replace the filter"
            ),
        }
    ]
    for summary, records, expected in (
        (
            {"active": 1, "overdue": 0, "due_soon": 1, "completed": 4},
            [updated],
            expected_items,
        ),
        ({"active": 0, "overdue": 0, "due_soon": 0, "completed": 5}, [], []),
    ):
        backend.responses[("GET", "/v1/summary")] = (
            200,
            json.dumps(summary).encode(),
            {},
        )
        backend.responses[("GET", "/v1/items")] = (
            200,
            json.dumps({"items": records, "next_page": None}).encode(),
            {},
        )
        await entry.runtime_data.async_refresh()
        await hass.async_block_till_done()
        for key, value in summary.items():
            state = hass.states.get(f"sensor.maintenance_{key}")
            assert state is not None
            assert state.state == str(value)
        state = hass.states.get("todo.maintenance")
        assert state is not None
        assert state.state == str(len(expected))
        result = await hass.services.async_call(
            "todo",
            "get_items",
            {"entity_id": "todo.maintenance"},
            blocking=True,
            return_response=True,
        )
        assert result == {"todo.maintenance": {"items": expected}}
        async with asyncio.timeout(5):
            changed = await client.receive_json()
        assert changed == {
            "id": 1,
            "type": "event",
            "event": {"items": [{**item, "completed": None} for item in expected]},
        }


@pytest.mark.parametrize("native_todo", [False, True], ids=["action", "todo"])
async def test_completion_has_authenticated_human_context(
    hass: HomeAssistant,
    hass_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    native_todo: bool,
) -> None:
    backend.responses[("POST", f"/v1/items/{ITEM_ID}/completions")] = (
        200,
        json.dumps(item_record(occurrence_id=NEXT_OCCURRENCE_ID, revision=2)).encode(),
        {"ETag": '"2"'},
    )
    if native_todo:
        await hass.services.async_call(
            "todo",
            "update_item",
            {
                "entity_id": "todo.maintenance",
                "item": OCCURRENCE_ID,
                "status": "completed",
            },
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    else:
        result = await hass.services.async_call(
            DOMAIN,
            "complete",
            {"item_id": ITEM_ID, "occurrence_id": OCCURRENCE_ID},
            context=Context(user_id=hass_user.id),
            blocking=True,
            return_response=True,
        )
        assert result == {
            "id": ITEM_ID,
            "revision": 2,
            "occurrence_id": NEXT_OCCURRENCE_ID,
            "due": "2026-10-15",
        }
    forwarded = next(
        request for request in backend.requests if request.path.endswith("/completions")
    )
    assert json.loads(forwarded.body) == {"occurrence_id": OCCURRENCE_ID}
    assert forwarded.headers["If-Match"] == '"1"'
    assert forwarded.headers["Idempotency-Key"]
    assert forwarded.headers["X-Maintenance-Actor-Id"] == f"ha:{hass_user.id}"
    assert forwarded.headers["X-Maintenance-Actor-Name"] == hass_user.name
    assert "X-Maintenance-System-Actor" not in forwarded.headers
    assert "X-Maintenance-Admin" not in forwarded.headers


async def test_no_user_completion_has_explicit_system_attribution(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    await hass.services.async_call(
        DOMAIN,
        "complete",
        {
            "item_id": ITEM_ID,
            "occurrence_id": OCCURRENCE_ID,
            "if_match": '"1"',
            "idempotency_key": "automation-run-one",
        },
        blocking=True,
    )
    forwarded = next(
        request for request in backend.requests if request.path.endswith("/completions")
    )
    assert forwarded.headers["X-Maintenance-System-Actor"] == "ha:automation"
    assert forwarded.headers["Idempotency-Key"] == "automation-run-one"
    assert "X-Maintenance-Actor-Id" not in forwarded.headers
    assert "X-Maintenance-Actor-Name" not in forwarded.headers
    assert "X-Maintenance-Admin" not in forwarded.headers


async def test_unknown_user_cannot_become_system_actor(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    before = len(backend.requests)
    with pytest.raises(Unauthorized):
        await hass.services.async_call(
            DOMAIN,
            "complete",
            {"item_id": ITEM_ID, "occurrence_id": OCCURRENCE_ID},
            context=Context(user_id="deleted-user"),
            blocking=True,
        )
    assert len(backend.requests) == before


@pytest.mark.parametrize("status", [409, 412])
async def test_conflict_does_not_retry_or_complete_the_next_occurrence(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend, status: int
) -> None:
    backend.responses[("POST", f"/v1/items/{ITEM_ID}/completions")] = (
        status,
        b'{"message":"changed"}',
        {},
    )
    with pytest.raises(HomeAssistantError, match="changed or this occurrence"):
        await hass.services.async_call(
            DOMAIN,
            "complete",
            {"item_id": ITEM_ID, "occurrence_id": OCCURRENCE_ID},
            blocking=True,
        )
    writes = [
        request for request in backend.requests if request.path.endswith("/completions")
    ]
    assert len(writes) == 1
    assert json.loads(writes[0].body) == {"occurrence_id": OCCURRENCE_ID}


@pytest.mark.parametrize(
    "change", [{"rename": "Forged edit"}, {"status": "needs_action"}]
)
async def test_todo_cannot_edit_or_reopen_backend_definitions(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    change: dict[str, str],
) -> None:
    before = len(backend.requests)
    with pytest.raises(ServiceValidationError, match="Only completion"):
        await hass.services.async_call(
            "todo",
            "update_item",
            {"entity_id": "todo.maintenance", "item": OCCURRENCE_ID, **change},
            blocking=True,
        )
    assert len(backend.requests) == before


async def test_entities_unload_and_actions_stay_registered(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    assert await hass.config_entries.async_unload(entry.entry_id)
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "unavailable"
    for key in SUMMARY_KEYS:
        state = hass.states.get(f"sensor.maintenance_{key}")
        assert state is not None
        assert state.state == "unavailable"
    with pytest.raises(ServiceValidationError, match="not loaded"):
        await hass.services.async_call(
            DOMAIN,
            "complete",
            {"item_id": ITEM_ID, "occurrence_id": OCCURRENCE_ID},
            blocking=True,
        )


async def test_actual_assist_completion_accepts_partial_item_without_stale_actor(
    hass: HomeAssistant,
    hass_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    """Assist omits optional fields and does not forward an entity user context."""
    successor = item_record(occurrence_id=NEXT_OCCURRENCE_ID, revision=2)
    backend.responses[("POST", f"/v1/items/{ITEM_ID}/completions")] = (
        200,
        json.dumps(successor).encode(),
        {"ETag": '"2"'},
    )
    await hass.services.async_call(
        "todo",
        "update_item",
        {
            "entity_id": "todo.maintenance",
            "item": OCCURRENCE_ID,
            "status": "completed",
        },
        context=Context(user_id=hass_user.id),
        blocking=True,
    )
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": [successor], "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    await async_setup_intents(hass)
    await intent.async_handle(
        hass,
        "conversation",
        INTENT_LIST_COMPLETE_ITEM,
        {"name": {"value": "Maintenance"}, "item": {"value": "Replace filter"}},
    )
    writes = [
        request for request in backend.requests if request.path.endswith("/completions")
    ]
    assert len(writes) == 2
    assert writes[0].headers["X-Maintenance-Actor-Id"] == f"ha:{hass_user.id}"
    assert writes[1].headers["X-Maintenance-System-Actor"] == "ha:automation"
    assert "X-Maintenance-Actor-Id" not in writes[1].headers
    assert json.loads(writes[1].body) == {"occurrence_id": NEXT_OCCURRENCE_ID}


async def test_assist_does_not_inherit_a_native_read_user(
    hass: HomeAssistant,
    hass_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    """Core assigns entity context to reads too, with no mutation to consume it."""
    await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        context=Context(user_id=hass_user.id),
        blocking=True,
        return_response=True,
    )
    await async_setup_intents(hass)
    await intent.async_handle(
        hass,
        "conversation",
        INTENT_LIST_COMPLETE_ITEM,
        {"name": {"value": "Maintenance"}, "item": {"value": "Replace filter"}},
    )
    forwarded = next(
        request for request in backend.requests if request.path.endswith("/completions")
    )
    assert "X-Maintenance-Actor-Id" not in forwarded.headers
    assert forwarded.headers["X-Maintenance-System-Actor"] == "ha:automation"


async def test_concurrent_native_completions_keep_each_users_identity(
    hass: HomeAssistant,
    hass_user: MockUser,
    hass_admin_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    """Core's shared entity must not exchange actors between service tasks."""
    second_item = item_record(item_id="second-filter", occurrence_id=NEXT_OCCURRENCE_ID)
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": [item_record(), second_item], "next_page": None}).encode(),
        {},
    )
    backend.responses[("POST", "/v1/items/second-filter/completions")] = (
        200,
        json.dumps(second_item).encode(),
        {"ETag": '"1"'},
    )
    await entry.runtime_data.async_refresh()
    backend.delay_seconds = 0.01
    await asyncio.gather(
        hass.services.async_call(
            "todo",
            "update_item",
            {
                "entity_id": "todo.maintenance",
                "item": OCCURRENCE_ID,
                "status": "completed",
            },
            context=Context(user_id=hass_user.id),
            blocking=True,
        ),
        hass.services.async_call(
            "todo",
            "update_item",
            {
                "entity_id": "todo.maintenance",
                "item": NEXT_OCCURRENCE_ID,
                "status": "completed",
            },
            context=Context(user_id=hass_admin_user.id),
            blocking=True,
        ),
    )
    actors = {
        request.path: request.headers["X-Maintenance-Actor-Id"]
        for request in backend.requests
        if request.path.endswith("/completions")
    }
    assert actors == {
        f"/v1/items/{ITEM_ID}/completions": f"ha:{hass_user.id}",
        "/v1/items/second-filter/completions": f"ha:{hass_admin_user.id}",
    }


@pytest.mark.parametrize(
    ("completed", "due"),
    [(False, None), (True, None), (True, "2026-10-15")],
    ids=["undated-active", "undated-completed", "dated-completed"],
)
async def test_undated_tasks_preserve_native_status_without_inventing_dates(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    completed: bool,
    due: str | None,
) -> None:
    """Unknown dates remain absent in HA, including completed one-off tasks."""
    status = "completed" if completed else "needs_action"
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps(
            {
                "items": [item_record(due=due, status=status)],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == ("0" if completed else "1")
    result = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    items = entity_items(result, "todo.maintenance")
    assert len(items) == 1
    assert items[0]["status"] == status
    assert items[0]["uid"] == OCCURRENCE_ID
    if due is None:
        assert "due" not in items[0]
    else:
        assert items[0]["due"] == due
    calendar = await hass.services.async_call(
        "calendar",
        "get_events",
        {
            "entity_id": "calendar.maintenance",
            "start_date_time": "2026-01-01T00:00:00Z",
            "end_date_time": "2027-01-01T00:00:00Z",
        },
        blocking=True,
        return_response=True,
    )
    assert entity_items(calendar, "calendar.maintenance", "events") == []
    if completed:
        before = len(backend.requests)
        with pytest.raises(ServiceValidationError, match="no longer active"):
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
        assert len(backend.requests) == before
