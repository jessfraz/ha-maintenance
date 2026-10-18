"""Native completion actions using the same versioned Rust contract."""

import json
from typing import Literal, TypedDict
from uuid import UUID, uuid4

import probatio as vol
from aiohttp import ClientError
from homeassistant.core import (
    Context,
    HomeAssistant,
    ServiceCall,
    ServiceResponse,
    SupportsResponse,
)
from homeassistant.exceptions import (
    HomeAssistantError,
    ServiceValidationError,
    Unauthorized,
)

from .client import Actor, BackendResponse, ResponseTooLarge
from .const import DOMAIN
from .coordinator import MaintenanceCoordinator
from .http import loaded_coordinator
from .models import ITEM_ID, MaintenanceItem, ScheduleKind


class CompletionResult(TypedDict):
    """The backend-owned occurrence returned to an HA script."""

    id: str
    revision: int
    occurrence_id: str
    due: str | None


class ChecklistStepResult(CompletionResult):
    """The updated step on the same parent occurrence."""

    step_id: str
    completed: bool


class CompletionRequest(TypedDict):
    occurrence_id: str


class ChecklistStepRequest(TypedDict):
    occurrence_id: str
    completed: bool


async def async_actor(hass: HomeAssistant, context: Context) -> Actor:
    """Resolve human identity from HA authentication or attribute system work."""
    if context.user_id is None:
        return Actor()
    user = await hass.auth.async_get_user(context.user_id)
    if user is None or not user.is_active:
        raise Unauthorized(context=context)
    return Actor(
        user.id,
        user.name or "Home Assistant user",
        user.is_admin,
        owner=user.is_owner and user.is_admin,
    )


def check_response(response: BackendResponse) -> None:
    """Surface conflicts without retrying a write against a newer revision."""
    if 200 <= response.status < 300:
        return
    if response.status in {409, 412}:
        raise HomeAssistantError(
            "The task changed or this occurrence was already completed. "
            "Refresh Maintenance before trying again."
        )
    raise HomeAssistantError(
        f"The Maintenance backend rejected the action (HTTP {response.status})"
    )


async def _async_mutate_occurrence(
    coordinator: MaintenanceCoordinator,
    item_id: str,
    actor: Actor,
    method: Literal["POST", "PUT"],
    action: str,
    payload: CompletionRequest | ChecklistStepRequest,
    *,
    if_match: str | None = None,
    idempotency_key: str | None = None,
) -> MaintenanceItem:
    """Apply one versioned mutation without retrying against a newer occurrence."""
    if any(
        item.id == item_id and item.schedule_kind is ScheduleKind.INSTRUCTION
        for item in coordinator.data.items
    ):
        raise ServiceValidationError(
            "Standing instructions cannot be completed or checked off"
        )
    path = f"/v1/items/{item_id}"
    try:
        if if_match is None:
            current = await coordinator.client.request("GET", path, actor=actor)
            check_response(current)
            if_match = current.headers.get("ETag")
            if not if_match:
                raise HomeAssistantError("The backend omitted the item version")
        response = await coordinator.client.request(
            method,
            f"{path}/{action}",
            actor=actor,
            body=json.dumps(payload).encode(),
            headers={
                "If-Match": if_match,
                "Idempotency-Key": idempotency_key or str(uuid4()),
            },
        )
    except ClientError, TimeoutError, ResponseTooLarge:
        raise HomeAssistantError("Cannot contact the Maintenance backend") from None
    check_response(response)
    try:
        item = MaintenanceItem.parse(response.json())
    except ValueError:
        raise HomeAssistantError("The backend returned an invalid item") from None
    await coordinator.async_request_refresh()
    return item


async def async_complete(
    coordinator: MaintenanceCoordinator,
    item_id: str,
    occurrence_id: UUID,
    actor: Actor,
    *,
    if_match: str | None = None,
    idempotency_key: str | None = None,
) -> MaintenanceItem:
    """Complete exactly the occurrence the caller saw, never a future one."""
    payload: CompletionRequest = {"occurrence_id": str(occurrence_id)}
    return await _async_mutate_occurrence(
        coordinator,
        item_id,
        actor,
        "POST",
        "completions",
        payload,
        if_match=if_match,
        idempotency_key=idempotency_key,
    )


async def async_set_checklist_step(
    coordinator: MaintenanceCoordinator,
    item_id: str,
    occurrence_id: UUID,
    step_id: UUID,
    completed: bool,
    actor: Actor,
    *,
    if_match: str | None = None,
    idempotency_key: str | None = None,
) -> MaintenanceItem:
    """Update one real step while leaving the parent due date and identity intact."""
    payload: ChecklistStepRequest = {
        "occurrence_id": str(occurrence_id),
        "completed": completed,
    }
    return await _async_mutate_occurrence(
        coordinator,
        item_id,
        actor,
        "PUT",
        f"checklist/{step_id}",
        payload,
        if_match=if_match,
        idempotency_key=idempotency_key,
    )


def async_register_services(hass: HomeAssistant) -> None:
    """Register completion even when an entry is temporarily unavailable."""

    async def handle(
        call: ServiceCall,
    ) -> ServiceResponse:
        actor = await async_actor(hass, call.context)
        coordinator = loaded_coordinator(hass)
        if coordinator is None:
            raise ServiceValidationError("Maintenance is not loaded")
        item_id = call.data["item_id"]
        if not ITEM_ID.fullmatch(item_id):
            raise ServiceValidationError("Invalid item ID")
        try:
            occurrence_id = UUID(call.data["occurrence_id"])
        except ValueError as err:
            raise ServiceValidationError("Invalid occurrence ID") from err
        if call.service == "set_checklist_step":
            try:
                step_id = UUID(call.data["step_id"])
            except ValueError as err:
                raise ServiceValidationError("Invalid checklist step ID") from err
            item = await async_set_checklist_step(
                coordinator,
                item_id,
                occurrence_id,
                step_id,
                call.data["completed"],
                actor,
                if_match=call.data.get("if_match"),
                idempotency_key=call.data.get("idempotency_key"),
            )
        else:
            item = await async_complete(
                coordinator,
                item_id,
                occurrence_id,
                actor,
                if_match=call.data.get("if_match"),
                idempotency_key=call.data.get("idempotency_key"),
            )
        if call.return_response:
            result: CompletionResult = {
                "id": item.id,
                "revision": item.revision,
                "occurrence_id": str(item.occurrence_id),
                "due": item.due.isoformat() if item.due is not None else None,
            }
            if call.service == "set_checklist_step":
                step = next(
                    (step for step in item.checklist if step.id == step_id), None
                )
                if step is None:
                    raise HomeAssistantError(
                        "The backend omitted the updated checklist step"
                    )
                step_result = ChecklistStepResult(
                    **result,
                    step_id=str(step.id),
                    completed=step.completion is not None,
                )
                return {**step_result}
            return {**result}
        return None

    hass.services.async_register(
        DOMAIN,
        "complete",
        handle,
        schema=vol.Schema(
            {
                vol.Required("item_id"): str,
                vol.Required("occurrence_id"): str,
                vol.Optional("if_match"): str,
                vol.Optional("idempotency_key"): str,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )
    hass.services.async_register(
        DOMAIN,
        "set_checklist_step",
        handle,
        schema=vol.Schema(
            {
                vol.Required("item_id"): str,
                vol.Required("occurrence_id"): str,
                vol.Required("step_id"): str,
                vol.Required("completed"): bool,
                vol.Optional("if_match"): str,
                vol.Optional("idempotency_key"): str,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )
