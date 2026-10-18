"""Project HA registry identities without retaining state or hardware inventories."""

import json
import re
from itertools import chain
from typing import Literal, TypedDict
from urllib.parse import urlencode

from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.network import NoURLAvailableError, get_url

from .client import InvalidBackendURL, normalize_backend_url
from .const import MAX_REGISTRY_BYTES
from .models import HomeAssistantKind, HomeAssistantReference

MAX_REGISTRY_TARGETS = 20_000
_MARKDOWN = re.compile(r"([\\`*_{}\[\]()<>#+.!|])")
_CONTROL = re.compile(r"[\x00-\x1f\x7f-\x9f]")


class DeviceTarget(TypedDict):
    kind: Literal["device"]
    id: str
    name: str
    disabled: bool


class EntityTarget(TypedDict):
    kind: Literal["entity"]
    id: str
    name: str
    disabled: bool
    entity_id: str
    device_id: str | None


class ReadyRegistry(TypedDict):
    status: Literal["ready"]
    instance_id: str
    base_url: str | None
    targets: list[DeviceTarget | EntityTarget]


class UnavailableRegistry(TypedDict):
    status: Literal["unavailable"]
    instance_id: str
    reason: Literal["limit_exceeded", "unavailable"]


def base_url(hass: HomeAssistant) -> str | None:
    """Prefer HA's configured external origin, with its supported internal fallback."""
    try:
        return normalize_backend_url(get_url(hass, prefer_external=True))
    except NoURLAvailableError, InvalidBackendURL:
        return None


def device_name(device: dr.AnyDeviceEntry) -> str:
    return device.name_by_user or device.name or device.id


def entity_name(hass: HomeAssistant, entry: er.RegistryEntry) -> str:
    state = hass.states.get(entry.entity_id)
    return (
        entry.name
        or (state.name if state is not None else None)
        or entry.original_name
        or entry.entity_id
    )


def _display_name(value: str, identity: str) -> str:
    """Bound a display label while preserving the object's complete identity."""
    value = _CONTROL.sub(" ", value).strip() or identity
    return value if len(value) <= 200 else value[:199] + "…"


def registry_payload(hass: HomeAssistant, instance_id: str) -> bytes:
    """Serialize one complete, bounded snapshot; an over-limit snapshot is unusable."""
    devices = dr.async_get(hass)
    entities = er.async_get(hass)
    targets: list[DeviceTarget | EntityTarget] = []
    for device in chain(devices.devices, devices.child_devices):
        if len(targets) == MAX_REGISTRY_TARGETS:
            return unavailable_payload(instance_id, "limit_exceeded")
        targets.append(
            {
                "kind": "device",
                "id": device.id,
                "name": _display_name(device_name(device), device.id),
                "disabled": device.disabled_by is not None,
            }
        )
    for entry in entities.entities.values():
        if len(targets) == MAX_REGISTRY_TARGETS:
            return unavailable_payload(instance_id, "limit_exceeded")
        targets.append(
            {
                "kind": "entity",
                "id": entry.id,
                "name": _display_name(entity_name(hass, entry), entry.entity_id),
                "disabled": entry.disabled_by is not None,
                "entity_id": entry.entity_id,
                "device_id": entry.device_id,
            }
        )
    report: ReadyRegistry = {
        "status": "ready",
        "instance_id": instance_id,
        "base_url": base_url(hass),
        "targets": targets,
    }
    payload = json.dumps(report, separators=(",", ":"), ensure_ascii=False).encode()
    if len(payload) > MAX_REGISTRY_BYTES:
        return unavailable_payload(instance_id, "limit_exceeded")
    return payload


def unavailable_payload(
    instance_id: str, reason: Literal["limit_exceeded", "unavailable"]
) -> bytes:
    report: UnavailableRegistry = {
        "status": "unavailable",
        "instance_id": instance_id,
        "reason": reason,
    }
    return json.dumps(report).encode()


def describe_references(
    hass: HomeAssistant,
    instance_id: str | None,
    references: tuple[HomeAssistantReference, ...],
    *,
    markdown: bool = True,
) -> str | None:
    """Resolve selected links locally; missing objects never become another object."""
    if not references:
        return None
    origin = base_url(hass) or ""
    lines = ["Home Assistant"]
    for reference in references:
        kind = reference.kind.value.capitalize()
        if reference.instance_id != instance_id:
            lines.append(f"- {kind} {reference.id} (another Home Assistant instance)")
            continue
        if reference.kind is HomeAssistantKind.DEVICE:
            device = dr.async_get(hass).async_get(
                reference.id, include_composite_devices=False
            )
            if device is None:
                lines.append(f"- Device {reference.id} (removed from Home Assistant)")
                continue
            name = device_name(device)
            disabled = device.disabled_by is not None
            path = f"/config/devices/device/{device.id}"
        else:
            entry = er.async_get(hass).async_get(reference.id)
            if entry is None:
                lines.append(f"- Entity {reference.id} (removed from Home Assistant)")
                continue
            name = entity_name(hass, entry)
            disabled = entry.disabled_by is not None
            path = "/?" + urlencode(
                {
                    "more-info-entity-id": entry.entity_id,
                    "more-info-view": "info",
                }
            )
        label = _display_name(name, reference.id)
        if markdown:
            label = _MARKDOWN.sub(r"\\\1", label)
            link = f"[{label}]({origin}{path})"
        else:
            link = f"{label}: {origin}{path}"
        lines.append(f"- {link}" + (" (disabled)" if disabled else ""))
    return "\n".join(lines)
