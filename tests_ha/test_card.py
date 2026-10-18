"""The native card reads current tasks and opens stable Maintenance app routes."""

import json
from pathlib import Path

import pytest
from freezegun.api import FrozenDateTimeFactory
from homeassistant.components import frontend
from homeassistant.core import HomeAssistant
from homeassistant.loader import async_get_integration
from pytest_homeassistant_custom_component.common import MockUser
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import CONF_ADDON_SLUG, DOMAIN

from .conftest import Backend, item_record

CARD_PATH = "/api/home_maintenance/card"


def card_records() -> list[dict[str, object]]:
    records = [
        item_record(
            item_id="example-today-local",
            occurrence_id=f"{1:032x}",
            due="2026-10-17",
            timezone="America/Vancouver",
        ),
        item_record(
            item_id="example-overdue", occurrence_id=f"{2:032x}", due="2026-10-17"
        ),
        item_record(
            item_id="example-today-utc", occurrence_id=f"{3:032x}", due="2026-10-18"
        ),
        item_record(
            item_id="example-last-day", occurrence_id=f"{4:032x}", due="2026-11-01"
        ),
        item_record(
            item_id="example-later", occurrence_id=f"{5:032x}", due="2026-11-02"
        ),
        item_record(
            item_id="example-undated",
            occurrence_id=f"{6:032x}",
            due=None,
            schedule_kind="once",
        ),
        item_record(
            item_id="example-completed",
            occurrence_id=f"{7:032x}",
            due="2026-10-17",
            status="completed",
        ),
        item_record(
            item_id="example-archived",
            occurrence_id=f"{8:032x}",
            due="2026-10-18",
            archived=True,
        ),
        item_record(
            item_id="example-instruction",
            occurrence_id=f"{9:032x}",
            due=None,
            schedule_kind="instruction",
        ),
    ]
    return records


@pytest.mark.parametrize(
    ("query", "expected", "total"),
    [
        ("", ["example-today-local", "example-today-utc", "example-last-day"], 3),
        ("?view=upcoming&days=1", ["example-today-local", "example-today-utc"], 2),
        ("?view=upcoming&limit=1", ["example-today-local"], 3),
        ("?view=overdue", ["example-overdue"], 1),
        ("?view=completed", ["example-completed"], 1),
        (
            "?view=active",
            [
                "example-overdue",
                "example-today-local",
                "example-today-utc",
                "example-last-day",
                "example-later",
                "example-undated",
            ],
            6,
        ),
    ],
)
async def test_card_projects_current_status_and_item_local_dates(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_admin_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    freezer: FrozenDateTimeFactory,
    query: str,
    expected: list[str],
    total: int,
) -> None:
    freezer.move_to("2026-10-18T06:30:00Z")
    hass.config_entries.async_update_entry(
        entry, data={**entry.data, CONF_ADDON_SLUG: "example_home_maintenance"}
    )
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": card_records(), "next_page": None}).encode(),
        {},
    )
    refresh_token = await hass.auth.async_create_refresh_token(
        hass_admin_user, client_id="https://example.invalid"
    )
    client = await hass_client(hass.auth.async_create_access_token(refresh_token))
    response = await client.get(CARD_PATH + query)
    assert response.status == 200
    assert response.headers["Cache-Control"] == "no-store"
    payload = await response.json()
    assert payload["app_path"] == "/app/example_home_maintenance/"
    assert payload["can_open"] is True
    assert payload["total"] == total
    assert [item["id"] for item in payload["items"]] == expected
    assert all(
        set(item) == {"id", "name", "summary", "status", "due", "timezone"}
        for item in payload["items"]
    )
    assert "test-integration-token" not in await response.text()


async def test_thirty_day_upcoming_is_empty_despite_outside_and_undated_active_work(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_admin_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    freezer: FrozenDateTimeFactory,
) -> None:
    """An empty selected window is independent of the active inventory size."""
    freezer.move_to("2026-10-18T12:00:00Z")
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps(
            {
                "items": [
                    item_record(
                        item_id="example-outside-window",
                        occurrence_id=f"{1:032x}",
                        due="2026-11-18",
                    ),
                    item_record(
                        item_id="example-undated-active",
                        occurrence_id=f"{2:032x}",
                        due=None,
                        schedule_kind="standing",
                    ),
                ],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    refresh_token = await hass.auth.async_create_refresh_token(
        hass_admin_user, client_id="https://example.invalid"
    )
    client = await hass_client(hass.auth.async_create_access_token(refresh_token))
    upcoming = await client.get(CARD_PATH + "?view=upcoming&days=30&limit=1")
    assert upcoming.status == 200
    report = await upcoming.json()
    assert report["total"] == 0
    assert report["items"] == []
    active = await client.get(CARD_PATH + "?view=active&days=30")
    assert active.status == 200
    report = await active.json()
    assert report["total"] == 2
    assert [item["id"] for item in report["items"]] == [
        "example-outside-window",
        "example-undated-active",
    ]


@pytest.mark.parametrize(
    ("instant", "timezone", "day_thirty", "day_thirty_one"),
    [
        (
            "2026-10-18T06:30:00Z",
            "America/Vancouver",
            "2026-11-16",
            "2026-11-17",
        ),
        (
            "2026-10-18T23:30:00Z",
            "Asia/Tokyo",
            "2026-11-18",
            "2026-11-19",
        ),
        (
            "2026-02-07T08:30:00Z",
            "America/Vancouver",
            "2026-03-09",
            "2026-03-10",
        ),
        (
            "2026-10-03T06:30:00Z",
            "America/Vancouver",
            "2026-11-01",
            "2026-11-02",
        ),
    ],
    ids=["west-behind-utc", "east-ahead-of-utc", "spring-dst", "fall-dst"],
)
async def test_thirty_day_upcoming_includes_day_thirty_and_excludes_day_thirty_one(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_admin_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    freezer: FrozenDateTimeFactory,
    instant: str,
    timezone: str,
    day_thirty: str,
    day_thirty_one: str,
) -> None:
    """The inclusive cutoff follows the task's calendar, including DST changes."""
    freezer.move_to(instant)
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps(
            {
                "items": [
                    item_record(
                        item_id="example-day-thirty-one",
                        occurrence_id=f"{1:032x}",
                        due=day_thirty_one,
                        timezone=timezone,
                    ),
                    item_record(
                        item_id="example-day-thirty",
                        occurrence_id=f"{2:032x}",
                        due=day_thirty,
                        timezone=timezone,
                    ),
                ],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    refresh_token = await hass.auth.async_create_refresh_token(
        hass_admin_user, client_id="https://example.invalid"
    )
    client = await hass_client(hass.auth.async_create_access_token(refresh_token))
    response = await client.get(CARD_PATH + "?view=upcoming&days=30&limit=1")
    assert response.status == 200
    report = await response.json()
    assert report["total"] == 1
    assert [item["id"] for item in report["items"]] == ["example-day-thirty"]
    assert report["items"][0]["due"] == day_thirty
    assert report["items"][0]["timezone"] == timezone


@pytest.mark.parametrize(
    ("instant", "due"),
    [
        ("2026-10-18T06:30:00Z", "2026-10-17"),
        ("2026-10-18T23:30:00Z", "2026-10-18"),
    ],
    ids=["west-local-today-before-utc-date", "east-local-today-after-utc-date"],
)
async def test_thirty_day_upcoming_starts_on_each_tasks_local_today(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_admin_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    freezer: FrozenDateTimeFactory,
    instant: str,
    due: str,
) -> None:
    """The same due date can be current in the west and overdue in the east."""
    freezer.move_to(instant)
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps(
            {
                "items": [
                    item_record(
                        item_id="example-west-today",
                        occurrence_id=f"{1:032x}",
                        due=due,
                        timezone="America/Vancouver",
                    ),
                    item_record(
                        item_id="example-east-overdue",
                        occurrence_id=f"{2:032x}",
                        due=due,
                        timezone="Asia/Tokyo",
                    ),
                ],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    refresh_token = await hass.auth.async_create_refresh_token(
        hass_admin_user, client_id="https://example.invalid"
    )
    client = await hass_client(hass.auth.async_create_access_token(refresh_token))
    response = await client.get(CARD_PATH + "?view=upcoming&days=30")
    assert response.status == 200
    report = await response.json()
    assert report["total"] == 1
    assert [item["id"] for item in report["items"]] == ["example-west-today"]


async def test_card_authentication_open_permissions_and_unload(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
    hass_user_token: str,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    anonymous = await hass_client(None)
    assert (await anonymous.get(CARD_PATH)).status == 401
    household = await hass_client(hass_user_token)
    hass.config_entries.async_update_entry(
        entry, data={**entry.data, CONF_ADDON_SLUG: "example_home_maintenance"}
    )
    response = await household.get(CARD_PATH + "?view=active")
    assert response.status == 200
    payload = await response.json()
    assert payload["can_open"] is False
    assert payload["app_path"] == "/app/example_home_maintenance/"
    assert payload["items"][0]["id"] == "replace-filter"
    await hass.auth.async_deactivate_user(hass_user)
    assert (await household.get(CARD_PATH)).status in {401, 403}
    admin = await hass_client()
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert (await admin.get(CARD_PATH)).status == 503


@pytest.mark.parametrize("slug", [None, "../escape", "example/escape", "", "INVALID"])
async def test_standalone_or_invalid_app_routes_cannot_open_rows(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: MaintenanceConfigEntry,
    slug: str | None,
) -> None:
    data = {key: value for key, value in entry.data.items() if key != CONF_ADDON_SLUG}
    if slug is not None:
        data[CONF_ADDON_SLUG] = slug
    hass.config_entries.async_update_entry(entry, data=data)
    client = await hass_client()
    response = await client.get(CARD_PATH + "?view=active")
    assert response.status == 200
    payload = await response.json()
    assert payload["app_path"] is None
    assert payload["can_open"] is False


@pytest.mark.parametrize(
    "query",
    [
        "?view=unknown",
        "?days=0",
        "?days=366",
        "?days=no",
        "?limit=0",
        "?limit=101",
        "?limit=-1",
        "?days=1&days=2",
        "?url=https://example.invalid",
    ],
)
async def test_card_query_boundaries_reject_before_contacting_backend(
    hass_client: ClientSessionGenerator,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    query: str,
) -> None:
    client = await hass_client()
    before = len(backend.requests)
    assert (await client.get(CARD_PATH + query)).status == 400
    assert len(backend.requests) == before


async def test_card_refresh_does_not_offer_stale_status_after_backend_failure(
    hass_client: ClientSessionGenerator, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    client = await hass_client()
    first = await client.get(CARD_PATH + "?view=active")
    assert (await first.json())["total"] == 1
    backend.responses[("GET", "/v1/summary")] = (500, b"{}", {})
    response = await client.get(CARD_PATH + "?view=active")
    assert response.status == 503
    assert "replace-filter" not in await response.text()


async def test_card_static_resource_is_registered_once_and_served_without_task_data(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: MaintenanceConfigEntry,
) -> None:
    integration = await async_get_integration(hass, DOMAIN)
    resource = f"/{DOMAIN}/maintenance-task-list.js?v={integration.version}"
    assert resource in hass.data[frontend.DATA_EXTRA_MODULE_URL].urls
    anonymous = await hass_client(None)
    response = await anonymous.get(resource)
    assert response.status == 200
    assert response.content_type.endswith("javascript")
    path = (
        Path(__file__).parents[1]
        / "custom_components/home_maintenance/frontend/maintenance-task-list.js"
    )
    assert await response.read() == await hass.async_add_executor_job(path.read_bytes)
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert list(hass.data[frontend.DATA_EXTRA_MODULE_URL].urls).count(resource) == 1
