"""Notification blueprints executed by Home Assistant's automation engine."""

import json
import shutil
from collections.abc import AsyncIterator
from dataclasses import dataclass
from pathlib import Path
from uuid import uuid4

import pytest
from freezegun.api import FrozenDateTimeFactory
from homeassistant.components.automation.helpers import async_get_blueprints
from homeassistant.core import HomeAssistant, ServiceCall, callback
from homeassistant.setup import async_setup_component
from homeassistant.util import dt as dt_util
from pytest_homeassistant_custom_component.common import async_fire_time_changed

from custom_components.home_maintenance import MaintenanceConfigEntry

from .conftest import SUMMARY, Backend, item_record

BLUEPRINTS = Path(__file__).resolve().parents[1] / "blueprints/automation/maintenance"


@pytest.fixture
def hass_config_dir(hass_tmp_config_dir: str) -> str:
    """Keep imported blueprints in HA's disposable test configuration."""
    return hass_tmp_config_dir


@dataclass(frozen=True)
class Notification:
    """Data received by the user-selected notification action."""

    title: str
    message: str
    count: int


@pytest.fixture(autouse=True)
async def stop_automation(hass: HomeAssistant) -> AsyncIterator[None]:
    """Release the time trigger before HA's fixture shuts down its event loop."""
    yield
    if hass.services.has_service("automation", "turn_off"):
        await hass.services.async_call(
            "automation",
            "turn_off",
            {"entity_id": "automation.maintenance_notification"},
            blocking=True,
        )
        await hass.async_block_till_done()


@pytest.fixture
def notifications(hass: HomeAssistant) -> list[Notification]:
    """Record a local service call without sending any notifications."""
    received: list[Notification] = []

    @callback
    def record(call: ServiceCall) -> None:
        title = call.data["title"]
        message = call.data["message"]
        count = call.data["count"]
        assert isinstance(title, str)
        assert isinstance(message, str)
        assert isinstance(count, int)
        received.append(Notification(title, message, count))

    hass.services.async_register("notify", "record_maintenance", record)
    return received


async def setup_automation(
    hass: HomeAssistant, filename: str, inputs: dict[str, object]
) -> None:
    """Import the shipped file and instantiate it through native blueprint loading."""

    def install() -> None:
        destination = Path(hass.config.path("blueprints/automation/maintenance"))
        destination.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(BLUEPRINTS / filename, destination / filename)

    await hass.async_add_executor_job(install)
    config = {
        "id": "maintenance_notification",
        "alias": "Maintenance notification",
        "use_blueprint": {
            "path": f"maintenance/{filename}",
            "input": {
                **inputs,
                "notify_actions": [
                    {
                        "action": "notify.record_maintenance",
                        "data": {
                            "title": "{{ title }}",
                            "message": "{{ message }}",
                            "count": "{{ count }}",
                        },
                    }
                ],
            },
        },
    }
    blueprint_inputs = await async_get_blueprints(hass).async_inputs_from_config(config)
    assert blueprint_inputs.blueprint.validate() is None
    assert await async_setup_component(hass, "automation", {"automation": [config]})
    await hass.async_start()
    await hass.async_block_till_done()
    state = hass.states.get("automation.maintenance_notification")
    assert state is not None and state.state == "on"


async def set_items(
    entry: MaintenanceConfigEntry, backend: Backend, records: list[dict[str, object]]
) -> None:
    """Refresh the actual calendar projection through its HTTP backend."""
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": records, "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()


@pytest.mark.freeze_time("2026-10-31T11:59:55Z")
async def test_daily_digest_uses_local_calendar_dates_across_dst(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    notifications: list[Notification],
    freezer: FrozenDateTimeFactory,
) -> None:
    """The configured time sends only dated work inside the selected local days."""
    await hass.config.async_set_time_zone("America/New_York")
    records = [
        item_record(
            item_id=name.replace(" ", "-"),
            occurrence_id=str(uuid4()),
            summary=f"Example {name}",
            due=due,
        )
        for name, due in (
            ("overdue", "2026-10-30"),
            ("today", "2026-10-31"),
            ("tomorrow", "2026-11-01"),
            ("last day", "2026-11-02"),
            ("outside window", "2026-11-03"),
        )
    ]
    await set_items(entry, backend, records)
    await setup_automation(
        hass,
        "daily_digest.yaml",
        {
            "calendar_entity": "calendar.maintenance",
            "notification_time": "08:00:00",
            "days_ahead": 3,
        },
    )
    assert notifications == []
    freezer.move_to("2026-10-31T12:00:00Z")
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done()
    assert notifications == [
        Notification(
            "Maintenance due in the next 3 days",
            "2026-10-31: Example today\n"
            "2026-11-01: Example tomorrow\n"
            "2026-11-02: Example last day",
            3,
        )
    ]


@pytest.mark.freeze_time("2026-10-15T08:00:00Z")
async def test_daily_digest_bounds_message_without_losing_the_total(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    notifications: list[Notification],
) -> None:
    """A large schedule fits a notification while retaining its complete count."""
    await set_items(
        entry,
        backend,
        [
            item_record(
                item_id=f"task-{number:02}",
                occurrence_id=f"0193c69f-e5c4-7e93-b6cb-{number:012}",
                summary=f"Example task {number:02}",
                due="2026-10-15",
            )
            for number in range(1, 18)
        ],
    )
    await setup_automation(
        hass,
        "daily_digest.yaml",
        {"calendar_entity": "calendar.maintenance", "days_ahead": 1},
    )
    await hass.services.async_call(
        "automation",
        "trigger",
        {"entity_id": "automation.maintenance_notification", "skip_condition": False},
        blocking=True,
    )
    assert notifications == [
        Notification(
            "Maintenance due today",
            "\n".join(
                [f"2026-10-15: Example task {number:02}" for number in range(1, 11)]
                + ["And 7 more."]
            ),
            17,
        )
    ]


@pytest.mark.freeze_time("2026-10-15T08:00:00Z")
@pytest.mark.parametrize("available", [True, False], ids=["empty", "unavailable"])
async def test_daily_digest_skips_empty_and_unavailable_calendars(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    notifications: list[Notification],
    available: bool,
) -> None:
    """Missing data and an empty schedule never produce an invented reminder."""
    await set_items(entry, backend, [])
    if not available:
        backend.responses[("GET", "/v1/summary")] = (503, b"unavailable", {})
        await entry.runtime_data.async_refresh()
    await setup_automation(
        hass, "daily_digest.yaml", {"calendar_entity": "calendar.maintenance"}
    )
    await hass.services.async_call(
        "automation",
        "trigger",
        {"entity_id": "automation.maintenance_notification", "skip_condition": False},
        blocking=True,
    )
    assert notifications == []


@pytest.mark.parametrize("days_ahead", [0, 31, 1.5, "invalid", True])
async def test_daily_digest_rejects_invalid_day_windows_at_runtime(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    notifications: list[Notification],
    days_ahead: object,
) -> None:
    """Hand-edited blueprint inputs cannot silently truncate or expand the horizon."""
    await setup_automation(
        hass,
        "daily_digest.yaml",
        {"calendar_entity": "calendar.maintenance", "days_ahead": days_ahead},
    )
    await hass.services.async_call(
        "automation",
        "trigger",
        {"entity_id": "automation.maintenance_notification", "skip_condition": False},
        blocking=True,
    )
    assert notifications == []


async def test_overdue_alert_only_sends_for_a_real_count_increase(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    notifications: list[Notification],
) -> None:
    """A decrease or a reconnect does not count as newly overdue work."""
    await setup_automation(
        hass, "overdue_alert.yaml", {"overdue_sensor": "sensor.maintenance_overdue"}
    )

    async def refresh_count(count: int) -> None:
        backend.responses[("GET", "/v1/summary")] = (
            200,
            json.dumps(SUMMARY | {"overdue": count}).encode(),
            {},
        )
        await entry.runtime_data.async_refresh()
        await hass.async_block_till_done()

    await refresh_count(2)
    assert notifications == [
        Notification("Maintenance overdue", "2 tasks overdue (1 new).", 2)
    ]
    await refresh_count(1)
    assert len(notifications) == 1
    backend.responses[("GET", "/v1/summary")] = (503, b"unavailable", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    await refresh_count(2)
    assert len(notifications) == 1
    await refresh_count(3)
    assert notifications[-1] == Notification(
        "Maintenance overdue", "3 tasks overdue (1 new).", 3
    )
    assert len(notifications) == 2


@pytest.mark.parametrize("previous", [None, "unknown", "unavailable", "invalid", "-1"])
async def test_overdue_alert_ignores_missing_or_invalid_previous_counts(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    notifications: list[Notification],
    previous: str | None,
) -> None:
    """Startup and malformed states never become a zero baseline."""
    if previous is None:
        hass.states.async_remove("sensor.maintenance_overdue")
    else:
        hass.states.async_set("sensor.maintenance_overdue", previous)
    await hass.async_block_till_done()
    await setup_automation(
        hass, "overdue_alert.yaml", {"overdue_sensor": "sensor.maintenance_overdue"}
    )
    hass.states.async_set("sensor.maintenance_overdue", "2")
    await hass.async_block_till_done()
    assert notifications == []
