"""Native calendar behavior through Home Assistant's authenticated API."""

import asyncio
import json

import pytest
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.util import dt as dt_util
from pytest_homeassistant_custom_component.common import async_fire_time_changed

from .conftest import NEXT_OCCURRENCE_ID, OCCURRENCE_ID, Backend, item_record


async def test_due_task_is_an_all_day_calendar_event(
    hass: HomeAssistant, hass_client, hass_user_token, entry, backend: Backend
) -> None:
    """Ordinary household users can read dates without accessing the dashboard."""
    client = await hass_client(hass_user_token)
    requests_before = len(backend.requests)
    response = await client.get(
        "/api/calendars/calendar.maintenance",
        params={"start": "2026-10-15T00:00:00Z", "end": "2026-10-17T00:00:00Z"},
    )
    assert response.status == 200
    assert await response.json() == [
        {
            "start": {"date": "2026-10-15"},
            "end": {"date": "2026-10-16"},
            "summary": "Replace filter",
            "description": (
                "Instructions\n1. Turn off the appliance\n2. Replace the filter"
            ),
            "uid": OCCURRENCE_ID,
            "location": None,
            "recurrence_id": None,
            "rrule": None,
            "status": None,
        }
    ]
    state = hass.states.get("calendar.maintenance")
    assert state is not None
    assert state.name == "Maintenance"
    assert state.attributes["supported_features"] == 0
    assert len(backend.requests) == requests_before


@pytest.mark.parametrize(
    ("due", "start", "end", "matches"),
    [
        ("2026-03-08", "2026-03-08T05:00:00Z", "2026-03-09T04:00:00Z", True),
        ("2026-11-01", "2026-11-01T04:00:00Z", "2026-11-02T05:00:00Z", True),
        ("2026-11-01", "2026-11-02T04:59:00Z", "2026-11-02T05:00:00Z", True),
        ("2026-03-08", "2026-03-08T04:00:00Z", "2026-03-08T05:00:00Z", False),
        ("2026-11-01", "2026-11-02T05:00:00Z", "2026-11-02T06:00:00Z", False),
        ("2026-11-01", "2026-11-01T16:00:00Z", "2026-11-01T16:00:00Z", False),
        ("9999-12-30", "9999-12-30T00:00:00Z", "9999-12-31T23:00:00Z", True),
    ],
    ids=[
        "spring-day",
        "fall-day",
        "fall-last-minute",
        "before",
        "after",
        "empty",
        "last-supported-day",
    ],
)
async def test_calendar_windows_use_local_dates_and_exclusive_bounds(
    hass: HomeAssistant,
    hass_client,
    entry,
    backend: Backend,
    due: str,
    start: str,
    end: str,
    matches: bool,
) -> None:
    """All-day dates span 23 or 25 hours at HA-local DST boundaries."""
    await hass.config.async_set_time_zone("America/New_York")
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": [item_record(due=due)], "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    client = await hass_client()
    response = await client.get(
        "/api/calendars/calendar.maintenance", params={"start": start, "end": end}
    )
    assert response.status == 200
    events = await response.json()
    assert len(events) == int(matches)
    if matches:
        assert events[0]["start"] == {"date": due}


async def test_calendar_sorts_ties_and_omits_archived_tasks(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    """API order is deterministic even when multiple tasks share a date."""
    records = [
        item_record(
            item_id="zeta-task",
            occurrence_id=NEXT_OCCURRENCE_ID,
            summary="Example zeta",
        ),
        item_record(item_id="alpha-task", summary="Example alpha"),
        item_record(
            item_id="archived-task",
            occurrence_id="0193c69f-e5c4-7e93-b6cb-8172dacb6170",
            archived=True,
        ),
    ]
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": records, "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    result = await hass.services.async_call(
        "calendar",
        "get_events",
        {
            "entity_id": "calendar.maintenance",
            "start_date_time": "2026-10-15T00:00:00Z",
            "end_date_time": "2026-10-17T00:00:00Z",
        },
        blocking=True,
        return_response=True,
    )
    assert [event["summary"] for event in result["calendar.maintenance"]["events"]] == [
        "Example alpha",
        "Example zeta",
    ]
    with pytest.raises(HomeAssistantError, match="does not support"):
        await hass.services.async_call(
            "calendar",
            "create_event",
            {
                "entity_id": "calendar.maintenance",
                "summary": "Example unsupported edit",
                "start_date": "2026-10-15",
                "end_date": "2026-10-16",
            },
            blocking=True,
        )


@pytest.mark.freeze_time("2026-11-01T03:30:00Z")
async def test_current_event_advances_at_local_midnight(
    hass: HomeAssistant, entry, backend: Backend, freezer
) -> None:
    """Past due dates stay in the past; HA advances its current/next event."""
    await hass.config.async_set_time_zone("America/New_York")
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps(
            {
                "items": [
                    item_record(summary="Example overdue task", due="2026-10-30"),
                    item_record(
                        item_id="current-task",
                        occurrence_id=NEXT_OCCURRENCE_ID,
                        summary="Example current task",
                        due="2026-10-31",
                    ),
                    item_record(
                        item_id="next-task",
                        occurrence_id="0193c69f-e5c4-7e93-b6cb-8172dacb6170",
                        summary="Example next task",
                        due="2026-11-01",
                    ),
                ],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    state = hass.states.get("calendar.maintenance")
    assert state.state == "on"
    assert state.attributes["message"] == "Example current task"
    for instant, message in [
        ("2026-11-01T04:00:00Z", "Example next task"),
        ("2026-11-02T04:59:00Z", "Example next task"),
        ("2026-11-02T05:00:00Z", None),
    ]:
        freezer.move_to(instant)
        async_fire_time_changed(hass, dt_util.utcnow())
        await hass.async_block_till_done(wait_background_tasks=True)
        state = hass.states.get("calendar.maintenance")
        assert state.state == ("on" if message else "off")
        assert state.attributes.get("message") == message


@pytest.mark.freeze_time("2026-10-15T12:00:00Z")
async def test_calendar_subscription_updates_when_a_later_task_changes(
    hass: HomeAssistant, hass_ws_client, entry, backend: Backend
) -> None:
    """Calendar cards refresh even when the first event's state stays identical."""
    current = item_record()
    later = item_record(
        item_id="later-task", occurrence_id=NEXT_OCCURRENCE_ID, due="2026-10-20"
    )
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": [current, later], "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    previous = hass.states.get("calendar.maintenance")
    client = await hass_ws_client(hass)
    await client.send_json(
        {
            "id": 1,
            "type": "calendar/event/subscribe",
            "entity_id": "calendar.maintenance",
            "start": "2026-10-19T00:00:00Z",
            "end": "2026-10-22T00:00:00Z",
        }
    )
    assert (await client.receive_json())["success"]
    initial = await client.receive_json()
    assert initial["event"]["events"][0]["summary"] == "Replace filter"
    later = item_record(
        item_id="later-task",
        occurrence_id=NEXT_OCCURRENCE_ID,
        due="2026-10-20",
        summary="Service the example filter",
        description="Use the example replacement part.",
        location="Example utility room",
    )
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": [current, later], "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    async with asyncio.timeout(5):
        changed = await client.receive_json()
    event = changed["event"]["events"][0]
    assert event["uid"] == NEXT_OCCURRENCE_ID
    assert event["summary"] == "Service the example filter"
    assert event["location"] == "Example utility room"
    assert "Use the example replacement part." in event["description"]
    state = hass.states.get("calendar.maintenance")
    assert state.state == previous.state
    assert state.attributes == previous.attributes


async def test_calendar_outage_recovery_empty_inventory_and_unload(
    hass: HomeAssistant, hass_client, entry, backend: Backend
) -> None:
    """An outage cannot masquerade as a successful empty or stale schedule."""
    client = await hass_client()
    params = {"start": "2026-10-15T00:00:00Z", "end": "2026-10-17T00:00:00Z"}
    backend.responses[("GET", "/v1/items")] = (503, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert hass.states.get("calendar.maintenance").state == "unavailable"
    response = await client.get("/api/calendars/calendar.maintenance", params=params)
    assert response.status == 500
    del backend.responses[("GET", "/v1/items")]
    await entry.runtime_data.async_refresh()
    assert hass.states.get("calendar.maintenance").state != "unavailable"
    response = await client.get("/api/calendars/calendar.maintenance", params=params)
    assert len(await response.json()) == 1
    backend.responses[("GET", "/v1/items")] = (
        200,
        b'{"items": [], "next_page": null}',
        {},
    )
    await entry.runtime_data.async_refresh()
    response = await client.get("/api/calendars/calendar.maintenance", params=params)
    assert await response.json() == []
    assert hass.states.get("calendar.maintenance").state == "off"
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert hass.states.get("calendar.maintenance").state == "unavailable"
    response = await client.get("/api/calendars/calendar.maintenance", params=params)
    assert response.status == 400
