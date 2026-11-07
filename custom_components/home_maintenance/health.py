"""Connection health is separate from the task catalog and overdue counts."""

from homeassistant.util import dt as dt_util
from homeassistant.util.json import JsonObjectType

from .coordinator import MaintenanceCoordinator

FRESH_SECONDS = 180


def health_snapshot(coordinator: MaintenanceCoordinator) -> JsonObjectType:
    """Expose observation age without backend messages or task contents."""
    checked = coordinator.last_success_at
    age = (dt_util.utcnow() - checked).total_seconds() if checked else None
    fresh = age is not None and 0 <= age <= FRESH_SECONDS
    available = coordinator.last_update_success and fresh
    reason = "stale"
    if not coordinator.last_update_success:
        reason = "connection_failed"
    elif fresh:
        reason = "ready"
    return {
        "available": available,
        "reason": reason,
        "last_success_at": checked.isoformat() if checked else None,
        "observation_age_seconds": int(age) if age is not None and age >= 0 else None,
    }
