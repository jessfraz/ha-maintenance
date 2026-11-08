"""Native Repairs expose backend failures without duplicating overdue tasks."""

import asyncio
from datetime import timedelta

import pytest
from freezegun.api import FrozenDateTimeFactory
from homeassistant.core import HomeAssistant
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.update_coordinator import UpdateFailed
from homeassistant.util import dt as dt_util
from pytest_homeassistant.common import MockConfigEntry, async_fire_time_changed

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import DOMAIN

from .conftest import Backend


def issue(hass: HomeAssistant, entry: MaintenanceConfigEntry) -> ir.IssueEntry | None:
    return ir.async_get(hass).async_get_issue(DOMAIN, f"{entry.entry_id}_backend")


def core_auth_key(entry: MaintenanceConfigEntry) -> tuple[str, str]:
    return ("homeassistant", f"config_entry_reauth_home_maintenance_{entry.entry_id}")


def owned_issues(
    hass: HomeAssistant, entry: MaintenanceConfigEntry
) -> set[tuple[str, str]]:
    return {
        (item.domain, item.issue_id)
        for item in ir.async_get(hass).issues.values()
        if item.config_entry_id == entry.entry_id
    }


async def test_native_repair_failure_and_confirmed_recovery(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    assert issue(hass, entry) is None
    backend.responses[("GET", "/v1/summary")] = (503, b"private backend detail", {})
    await entry.runtime_data.async_refresh()
    failed = issue(hass, entry)
    assert failed is not None and failed.translation_key == "backend_unavailable"
    assert failed.config_entry_id == entry.entry_id
    assert (
        failed.learn_more_url
        == "homeassistant://config/integrations/integration/home_maintenance"
    )
    assert "private backend detail" not in str(failed)
    backend.responses[("GET", "/v1/summary")] = (200, b'{"active": 1}', {})
    await entry.runtime_data.async_refresh()
    assert issue(hass, entry) is not None
    del backend.responses[("GET", "/v1/summary")]
    await entry.runtime_data.async_refresh()
    assert issue(hass, entry) is None
    overdue = hass.states.get("sensor.maintenance_overdue")
    assert overdue is not None and overdue.state == "1"


@pytest.mark.parametrize("status", [503, 401])
async def test_initial_failure_owns_repair_and_entry_removal_clears(
    hass: HomeAssistant, backend: Backend, status: int
) -> None:
    backend.responses[("GET", "/v1/summary")] = (status, b"private backend detail", {})
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            "backend_url": backend.url,
            "token": "test-only-token",
            "addon_slug": "test_app",
        },
    )
    entry.add_to_hass(hass)
    assert not await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    if status == 401:
        assert owned_issues(hass, entry) == {core_auth_key(entry)}
    else:
        failed = issue(hass, entry)
        assert failed is not None and failed.config_entry_id == entry.entry_id
        assert failed.learn_more_url == "homeassistant://config/app/test_app/logs"
    await hass.config_entries.async_remove(entry.entry_id)
    assert not owned_issues(hass, entry)


async def test_stale_observation_and_unload_cleanup(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    freezer: FrozenDateTimeFactory,
) -> None:
    # Pause scheduled polling while the installed entry still owns its timers.
    entry.runtime_data._unschedule_refresh()
    entry.runtime_data.last_success_at = dt_util.utcnow() - timedelta(seconds=181)
    freezer.tick(timedelta(seconds=60))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done()
    stale = issue(hass, entry)
    assert stale is not None and stale.translation_key == "backend_unavailable"
    assert await hass.config_entries.async_unload(entry.entry_id)
    ir.async_delete_issue(hass, DOMAIN, f"{entry.entry_id}_backend")
    freezer.tick(timedelta(minutes=2))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done()
    assert issue(hass, entry) is None


@pytest.mark.parametrize("remove", [False, True])
async def test_late_failed_http_response_cannot_publish_after_unload(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    remove: bool,
) -> None:
    coordinator = entry.runtime_data
    backend.blocked_path = "/v1/summary"
    backend.request_received.clear()
    backend.responses[("GET", "/v1/summary")] = (503, b"private backend detail", {})
    refresh = hass.async_create_background_task(
        coordinator.async_refresh(), "late inventory fixture"
    )
    try:
        async with asyncio.timeout(2):
            await backend.request_received.wait()
            if remove:
                assert await hass.config_entries.async_remove(entry.entry_id)
            else:
                assert await hass.config_entries.async_unload(entry.entry_id)
    finally:
        backend.response_release.set()
        await refresh
    assert issue(hass, entry) is None
    assert isinstance(coordinator.last_exception, UpdateFailed)


async def test_credential_failure_replaces_only_generic_availability(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    freezer: FrozenDateTimeFactory,
) -> None:
    backend.responses[("GET", "/v1/summary")] = (503, b"private detail", {})
    await entry.runtime_data.async_refresh()
    assert issue(hass, entry) is not None
    backend.responses[("GET", "/v1/summary")] = (401, b"private detail", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert owned_issues(hass, entry) == {core_auth_key(entry)}
    freezer.tick(timedelta(seconds=240))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done()
    assert owned_issues(hass, entry) == {core_auth_key(entry)}
    overdue = hass.states.get("sensor.maintenance_overdue")
    assert overdue is not None and overdue.state == "unavailable"
