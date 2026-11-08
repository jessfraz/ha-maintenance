"""Native connection health stays separate from due work and expired evidence."""

from datetime import timedelta

from freezegun.api import FrozenDateTimeFactory
from homeassistant.core import HomeAssistant, State
from homeassistant.util import dt as dt_util
from pytest_homeassistant.common import async_fire_time_changed

from custom_components.home_maintenance import MaintenanceConfigEntry

from .conftest import Backend

PROBLEM = "binary_sensor.maintenance_connection_problem"


def state(hass: HomeAssistant, entity_id: str) -> State:
    result = hass.states.get(entity_id)
    assert result is not None
    return result


async def test_connection_failure_and_recovery_do_not_depend_on_due_counts(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    assert state(hass, "sensor.maintenance_overdue").state == "1"
    assert state(hass, PROBLEM).state == "off"
    backend.responses[("GET", "/v1/summary")] = (503, b"private backend detail", {})
    await entry.runtime_data.async_refresh()
    problem = state(hass, PROBLEM)
    assert problem.state == "on"
    assert problem.attributes["reason"] == "connection_failed"
    assert "private backend detail" not in str(problem.attributes)
    del backend.responses[("GET", "/v1/summary")]
    await entry.runtime_data.async_refresh()
    assert state(hass, PROBLEM).state == "off"


async def test_installed_health_entity_expires_when_polling_stops(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    freezer: FrozenDateTimeFactory,
) -> None:
    await entry.runtime_data.async_shutdown()
    entry.runtime_data.last_success_at = dt_util.utcnow() - timedelta(seconds=181)
    freezer.tick(timedelta(seconds=60))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done()
    problem = state(hass, PROBLEM)
    assert problem.state == "on"
    assert problem.attributes["reason"] == "stale"
    assert await hass.config_entries.async_unload(entry.entry_id)
