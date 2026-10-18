"""Allowlisted operational diagnostics from cached Home Assistant state."""

from collections import Counter
from typing import Literal, TypedDict

from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.const import STATE_UNAVAILABLE, STATE_UNKNOWN
from homeassistant.core import HomeAssistant
from homeassistant.helpers import entity_registry as er

from .const import CONF_ADDON_SLUG, DOMAIN
from .coordinator import MaintenanceCoordinator
from .models import ItemStatus, MaintenanceData, ScheduleKind


class SummaryCounts(TypedDict):
    active: int
    overdue: int
    due_soon: int
    completed: int


class ScheduleCounts(TypedDict):
    once: int
    standing: int
    recurring: int
    instruction: int


class InventoryCounts(TypedDict):
    total: int
    archived: int
    needs_action: int
    completed: int
    dated: int
    undated: int
    schedule_kinds: ScheduleCounts


class EntityCounts(TypedDict):
    registered: int
    available: int
    unavailable: int
    unknown: int
    not_loaded: int


class MaintenanceDiagnostics(TypedDict):
    """The complete public payload; Core adds its own download envelope."""

    connection_mode: Literal["app", "standalone"]
    entry_state: str
    last_update_success: bool | None
    last_exception_type: str | None
    update_interval_seconds: float | None
    snapshot_available: bool
    assist_exposed: bool | None
    summary: SummaryCounts | None
    inventory: InventoryCounts | None
    entities: EntityCounts


async def async_get_config_entry_diagnostics(
    hass: HomeAssistant, entry: ConfigEntry[MaintenanceCoordinator]
) -> MaintenanceDiagnostics:
    """Return operational aggregates without contacting or refreshing the backend."""
    coordinator = getattr(entry, "runtime_data", None)
    snapshot: MaintenanceData | None = None
    success = None
    exception_type = None
    interval_seconds = None
    if isinstance(coordinator, MaintenanceCoordinator):
        success = coordinator.last_update_success
        if coordinator.last_exception is not None:
            # Exception messages, chains and arguments may contain private data.
            exception_type = type(coordinator.last_exception).__name__
        if coordinator.update_interval is not None:
            interval_seconds = coordinator.update_interval.total_seconds()
        if (
            entry.state is ConfigEntryState.LOADED
            and success
            and isinstance(coordinator.data, MaintenanceData)
        ):
            snapshot = coordinator.data

    summary: SummaryCounts | None = None
    inventory: InventoryCounts | None = None
    if snapshot is not None:
        # Keep every exported key explicit, including the backend summary keys.
        summary = {
            "active": snapshot.summary["active"],
            "overdue": snapshot.summary["overdue"],
            "due_soon": snapshot.summary["due_soon"],
            "completed": snapshot.summary["completed"],
        }
        kinds = Counter(item.schedule_kind for item in snapshot.items)
        inventory = {
            "total": len(snapshot.items),
            "archived": sum(item.archived for item in snapshot.items),
            "needs_action": sum(
                item.status is ItemStatus.NEEDS_ACTION for item in snapshot.items
            ),
            "completed": sum(
                item.status is ItemStatus.COMPLETED for item in snapshot.items
            ),
            "dated": sum(item.due is not None for item in snapshot.items),
            "undated": sum(item.due is None for item in snapshot.items),
            "schedule_kinds": {
                "once": kinds[ScheduleKind.ONCE],
                "standing": kinds[ScheduleKind.STANDING],
                "recurring": kinds[ScheduleKind.RECURRING],
                "instruction": kinds[ScheduleKind.INSTRUCTION],
            },
        }

    registry = er.async_get(hass)
    entities: EntityCounts = {
        "registered": 0,
        "available": 0,
        "unavailable": 0,
        "unknown": 0,
        "not_loaded": 0,
    }
    for entity in er.async_entries_for_config_entry(registry, entry.entry_id):
        entities["registered"] += 1
        state = hass.states.get(entity.entity_id)
        if state is None:
            entities["not_loaded"] += 1
        elif state.state == STATE_UNAVAILABLE:
            entities["unavailable"] += 1
        elif state.state == STATE_UNKNOWN:
            entities["unknown"] += 1
        else:
            entities["available"] += 1
    gateway_id = registry.async_get_entity_id("todo", DOMAIN, f"{entry.entry_id}_tasks")
    gateway = registry.async_get(gateway_id) if gateway_id is not None else None
    # Read the cached preference only. Resolving Core's implicit default would
    # initialize registry options and schedule a backend catalog publication.
    exposure = (
        gateway.options.get("conversation", {}).get("should_expose")
        if gateway
        else None
    )
    return {
        "connection_mode": "app" if entry.data.get(CONF_ADDON_SLUG) else "standalone",
        "entry_state": entry.state.value,
        "last_update_success": success,
        "last_exception_type": exception_type,
        "update_interval_seconds": interval_seconds,
        "snapshot_available": snapshot is not None,
        "assist_exposed": exposure if isinstance(exposure, bool) else None,
        "summary": summary,
        "inventory": inventory,
        "entities": entities,
    }
