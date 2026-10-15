"""Native completion actions using the same versioned Rust contract."""

import json
from typing import TypedDict
from uuid import UUID, uuid4

import probatio as vol
from aiohttp import ClientError
from homeassistant.core import (
    Context,
    HomeAssistant,
    ServiceCall,
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
from .models import ITEM_ID, MaintenanceItem


class CompletionResult(TypedDict):
    """The next backend-owned occurrence returned to an HA script."""

    id: str
    revision: int
    occurrence_id: str
    due: str


async def async_actor(hass: HomeAssistant, context: Context) -> Actor:
    """Resolve human identity from HA authentication or attribute system work."""
    if context.user_id is None:
        return Actor()
    user = await hass.auth.async_get_user(context.user_id)
    if user is None or not user.is_active:
        raise Unauthorized(context=context)
    return Actor(user.id, user.name or "Home Assistant user", user.is_admin)


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
    path = f"/v1/items/{item_id}"
    try:
        if if_match is None:
            current = await coordinator.client.request("GET", path, actor=actor)
            check_response(current)
            if_match = current.headers.get("ETag")
            if not if_match:
                raise HomeAssistantError("The backend omitted the item version")
        response = await coordinator.client.request(
            "POST",
            f"{path}/completions",
            actor=actor,
            body=json.dumps({"occurrence_id": str(occurrence_id)}).encode(),
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


def async_register_services(hass: HomeAssistant) -> None:
    """Register completion even when an entry is temporarily unavailable."""

    async def handle(call: ServiceCall) -> CompletionResult | None:
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
        item = await async_complete(
            coordinator,
            item_id,
            occurrence_id,
            actor,
            if_match=call.data.get("if_match"),
            idempotency_key=call.data.get("idempotency_key"),
        )
        if call.return_response:
            return {
                "id": item.id,
                "revision": item.revision,
                "occurrence_id": str(item.occurrence_id),
                "due": item.due.isoformat(),
            }
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
