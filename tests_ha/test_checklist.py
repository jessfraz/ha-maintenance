"""Checklist progress stays attached to one backend-owned parent occurrence."""

import json
from uuid import UUID

import probatio as vol
import pytest
from homeassistant.core import Context, HomeAssistant
from homeassistant.exceptions import HomeAssistantError, ServiceValidationError
from pytest_homeassistant_custom_component.common import MockUser
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import DOMAIN
from custom_components.home_maintenance.models import MaintenanceItem

from .conftest import (
    ITEM_ID,
    OCCURRENCE_ID,
    SECOND_STEP_ID,
    STEP_ID,
    Backend,
    item_record,
)

PREFIX = "/api/home_maintenance/v1"


def checklist_step(
    step_id: str = STEP_ID,
    summary: str = "Turn off the appliance",
    *,
    actor: dict[str, str] | None = None,
) -> dict[str, object]:
    return {
        "id": step_id,
        "summary": summary,
        "completion": {"completed_at": "2026-10-15T10:00:00Z", "actor": actor}
        if actor is not None
        else None,
    }


async def test_native_todo_uses_summary_and_shows_details_and_step_progress(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    record = item_record(
        summary="Service the appliance",
        description="Use the replacement specified in the task details.",
        checklist=[
            checklist_step(
                actor={"kind": "user", "id": "ha:example", "name": "Example"}
            ),
            checklist_step(SECOND_STEP_ID, "Install the replacement"),
        ],
    )
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": [record], "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    result = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "1"
    assert result == {
        "todo.maintenance": {
            "items": [
                {
                    "summary": "Service the appliance",
                    "uid": OCCURRENCE_ID,
                    "status": "needs_action",
                    "due": "2026-10-15",
                    "description": (
                        "Use the replacement specified in the task details.\n\n"
                        "Instructions\n1. Turn off the appliance\n"
                        "2. Replace the filter\n\n"
                        "Checklist (1/2 complete)\n- [x] Turn off the appliance\n"
                        "- [ ] Install the replacement"
                    ),
                }
            ]
        }
    }


@pytest.mark.parametrize("human", [True, False], ids=["human", "automation"])
async def test_step_action_preserves_parent_and_uses_trusted_context(
    hass: HomeAssistant,
    hass_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    human: bool,
) -> None:
    assert hass_user.name is not None
    actor = {
        "kind": "user" if human else "system",
        "id": f"ha:{hass_user.id}" if human else "ha:automation",
        "name": hass_user.name if human else "Home Assistant automation",
    }
    record = item_record(revision=2, checklist=[checklist_step(actor=actor)])
    backend.responses[("PUT", f"/v1/items/{ITEM_ID}/checklist/{STEP_ID}")] = (
        200,
        json.dumps(record).encode(),
        {"ETag": '"2"'},
    )
    result = await hass.services.async_call(
        DOMAIN,
        "set_checklist_step",
        {
            "item_id": ITEM_ID,
            "occurrence_id": OCCURRENCE_ID,
            "step_id": STEP_ID,
            "completed": True,
            "idempotency_key": "step-update",
        },
        context=Context(user_id=hass_user.id) if human else Context(),
        blocking=True,
        return_response=True,
    )
    assert result == {
        "id": ITEM_ID,
        "revision": 2,
        "occurrence_id": OCCURRENCE_ID,
        "due": "2026-10-15",
        "step_id": STEP_ID,
        "completed": True,
    }
    forwarded = next(
        request
        for request in backend.requests
        if request.path == f"/v1/items/{ITEM_ID}/checklist/{STEP_ID}"
    )
    assert forwarded.path == f"/v1/items/{ITEM_ID}/checklist/{STEP_ID}"
    assert forwarded.headers["If-Match"] == '"1"'
    assert forwarded.headers["Idempotency-Key"] == "step-update"
    assert json.loads(forwarded.body) == {
        "occurrence_id": OCCURRENCE_ID,
        "completed": True,
    }
    if human:
        assert forwarded.headers["X-Maintenance-Actor-Id"] == f"ha:{hass_user.id}"
        assert forwarded.headers["X-Maintenance-Actor-Name"] == hass_user.name
        assert "X-Maintenance-System-Actor" not in forwarded.headers
    else:
        assert forwarded.headers["X-Maintenance-System-Actor"] == "ha:automation"
        assert "X-Maintenance-Actor-Id" not in forwarded.headers
    assert not any(
        request.path.endswith("/completions") for request in backend.requests
    )
    parsed = MaintenanceItem.parse(record)
    assert parsed.occurrence_id == UUID(OCCURRENCE_ID)
    assert parsed.checklist[0].completion is not None
    assert parsed.checklist[0].completion.actor.id == actor["id"]


async def test_clear_step_uses_false_without_advancing_the_parent(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    record = item_record(revision=3, checklist=[checklist_step()])
    backend.responses[("PUT", f"/v1/items/{ITEM_ID}/checklist/{STEP_ID}")] = (
        200,
        json.dumps(record).encode(),
        {"ETag": '"3"'},
    )
    result = await hass.services.async_call(
        DOMAIN,
        "set_checklist_step",
        {
            "item_id": ITEM_ID,
            "occurrence_id": OCCURRENCE_ID,
            "step_id": STEP_ID,
            "completed": False,
            "if_match": '"2"',
        },
        blocking=True,
        return_response=True,
    )
    assert result is not None
    assert result["completed"] is False
    assert result["occurrence_id"] == OCCURRENCE_ID
    forwarded = next(
        request
        for request in backend.requests
        if request.path == f"/v1/items/{ITEM_ID}/checklist/{STEP_ID}"
    )
    assert forwarded.headers["If-Match"] == '"2"'
    assert json.loads(forwarded.body) == {
        "occurrence_id": OCCURRENCE_ID,
        "completed": False,
    }


@pytest.mark.parametrize("status", [409, 412])
async def test_step_conflicts_never_retry_against_a_new_parent(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend, status: int
) -> None:
    backend.responses[("PUT", f"/v1/items/{ITEM_ID}/checklist/{STEP_ID}")] = (
        status,
        b"{}",
        {},
    )
    with pytest.raises(HomeAssistantError, match="task changed"):
        await hass.services.async_call(
            DOMAIN,
            "set_checklist_step",
            {
                "item_id": ITEM_ID,
                "occurrence_id": OCCURRENCE_ID,
                "step_id": STEP_ID,
                "completed": True,
            },
            blocking=True,
        )
    writes = [
        request
        for request in backend.requests
        if request.path == f"/v1/items/{ITEM_ID}/checklist/{STEP_ID}"
    ]
    assert len(writes) == 1
    assert json.loads(writes[0].body)["occurrence_id"] == OCCURRENCE_ID


@pytest.mark.parametrize(
    "change",
    [
        {"step_id": "../escape"},
        {"item_id": "invalid_id"},
        {"occurrence_id": "not-a-uuid"},
        {"completed": "false"},
        {"actor_id": "forged-person"},
    ],
)
async def test_step_action_rejects_invalid_or_untrusted_service_data(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    change: dict[str, object],
) -> None:
    before = len(backend.requests)
    with pytest.raises((ServiceValidationError, vol.Invalid)):
        await hass.services.async_call(
            DOMAIN,
            "set_checklist_step",
            {
                "item_id": ITEM_ID,
                "occurrence_id": OCCURRENCE_ID,
                "step_id": STEP_ID,
                "completed": True,
                **change,
            },
            blocking=True,
        )
    assert len(backend.requests) == before


async def test_proxy_allows_real_step_updates_and_overrides_forged_actor_headers(
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
    hass_user_token: str,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    client = await hass_client(hass_user_token)
    response = await client.put(
        f"{PREFIX}/items/{ITEM_ID}/checklist/{STEP_ID}",
        json={"occurrence_id": OCCURRENCE_ID, "completed": True},
        headers={
            "If-Match": '"1"',
            "Idempotency-Key": "proxy-step",
            "X-Maintenance-Actor-Id": "forged",
            "X-Maintenance-Admin": "true",
            "X-Maintenance-System-Actor": "ha:automation",
        },
    )
    assert response.status == 200
    forwarded = next(
        request
        for request in backend.requests
        if request.path == f"/v1/items/{ITEM_ID}/checklist/{STEP_ID}"
    )
    assert forwarded.headers["X-Maintenance-Actor-Id"] == f"ha:{hass_user.id}"
    assert forwarded.headers["X-Maintenance-Actor-Name"] == hass_user.name
    assert "X-Maintenance-Admin" not in forwarded.headers
    assert "X-Maintenance-System-Actor" not in forwarded.headers


@pytest.mark.parametrize(
    "method,suffix,status",
    [
        ("put", "not-a-uuid", 404),
        ("put", f"{STEP_ID}/extra", 404),
        ("put", f"{STEP_ID}?actor=forged", 400),
        ("get", STEP_ID, 405),
        ("post", STEP_ID, 405),
    ],
)
async def test_checklist_proxy_preserves_route_method_and_query_allowlists(
    hass_client: ClientSessionGenerator,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    method: str,
    suffix: str,
    status: int,
) -> None:
    client = await hass_client()
    before = len(backend.requests)
    response = await getattr(client, method)(
        f"{PREFIX}/items/{ITEM_ID}/checklist/{suffix}"
    )
    assert response.status == status
    assert len(backend.requests) == before
