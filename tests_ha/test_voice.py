"""Maintenance sentences through Home Assistant's actual built-in agent."""

import asyncio
import json
from uuid import NAMESPACE_URL, uuid5

import pytest
from homeassistant.components import conversation
from homeassistant.components.homeassistant.exposed_entities import async_expose_entity
from homeassistant.core import Context, HomeAssistant
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.setup import async_setup_component
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import (
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)
from custom_components.home_maintenance.models import ActorKind, MaintenanceItem

from .conftest import (
    ITEM_ID,
    NEXT_OCCURRENCE_ID,
    OCCURRENCE_ID,
    Backend,
    item_record,
)
from .test_rust_backend import RustBackend
from .test_rust_backend import rust_backend as rust_backend


@pytest.fixture
async def init_core(hass: HomeAssistant) -> None:
    """Load Core's real exposure preferences before the built-in agent starts."""
    assert await async_setup_component(hass, "homeassistant", {})


@pytest.fixture
async def voice_entry(
    hass: HomeAssistant, init_core: None, entry: MaintenanceConfigEntry
) -> MaintenanceConfigEntry:
    """Use the user's existing built-in agent and explicit entity opt-in."""
    assert await async_setup_component(hass, "conversation", {})
    async_expose_entity(hass, "conversation", "todo.maintenance", True)
    await hass.async_block_till_done()
    return entry


async def say(hass: HomeAssistant, text: str, context: Context | None = None) -> str:
    result = await conversation.async_converse(
        hass, text, None, context or Context(), language="en"
    )
    assert not result.continue_conversation
    return result.response.speech.get("plain", {}).get("speech", "")


def snapshot(backend: Backend, records: list[dict[str, object]]) -> None:
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": records, "next_page": None}).encode(),
        {},
    )


def task(
    identity: str,
    summary: str,
    due: str | None,
    timezone: str = "UTC",
) -> dict[str, object]:
    record = item_record(
        item_id=identity,
        occurrence_id=str(uuid5(NAMESPACE_URL, identity)),
        summary=summary,
        due=due,
        timezone=timezone,
    )
    record["name"] = summary
    return record


async def test_queries_read_fresh_task_snapshot(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    """The voice answer refreshes data instead of reading the last entity state."""
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps(
            {
                "items": [item_record(summary="Inspect example appliance")],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    spoken = await say(hass, "what is my next maintenance task")
    assert "Inspect example appliance" in spoken
    assert "October 15, 2026" in spoken
    assert not any(request.method == "POST" for request in backend.requests)


async def test_named_completion_uses_authenticated_context(
    hass: HomeAssistant,
    hass_user: MockUser,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    backend.responses[("POST", f"/v1/items/{ITEM_ID}/completions")] = (
        200,
        json.dumps(
            item_record(occurrence_id=NEXT_OCCURRENCE_ID, revision=2, due="2027-01-15")
        ).encode(),
        {"ETag": '"2"'},
    )
    spoken = await say(
        hass,
        "complete maintenance task replace filter",
        Context(user_id=hass_user.id),
    )
    writes = [request for request in backend.requests if request.method == "POST"]
    assert len(writes) == 1
    assert writes[0].path == f"/v1/items/{ITEM_ID}/completions"
    assert json.loads(writes[0].body) == {"occurrence_id": OCCURRENCE_ID}
    assert writes[0].headers["If-Match"] == '"1"'
    assert writes[0].headers["X-Maintenance-Actor-Id"] == f"ha:{hass_user.id}"
    assert "Completed Replace filter" in spoken
    assert "January 15, 2027" in spoken


@pytest.mark.parametrize(
    ("phrase", "expected"),
    [
        ("what maintenance is due today", {"Western today", "Eastern today"}),
        ("what maintenance is due tomorrow", {"Western tomorrow", "Eastern tomorrow"}),
        (
            "what maintenance is due this week",
            {"Western today", "Eastern today", "Eastern tomorrow", "Western overdue"},
        ),
        (
            "what maintenance is due next week",
            {"Western tomorrow", "Eastern next week"},
        ),
        ("what maintenance is overdue", {"Western overdue", "Eastern overdue"}),
        ("what is my next maintenance task", {"Western overdue"}),
    ],
)
@pytest.mark.freeze_time("2026-10-19T00:30:00Z")
async def test_date_queries_use_each_tasks_timezone(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
    phrase: str,
    expected: set[str],
) -> None:
    records = [
        task("west-today", "Western today", "2026-10-18", "America/Vancouver"),
        task("east-today", "Eastern today", "2026-10-19", "Asia/Tokyo"),
        task("west-tomorrow", "Western tomorrow", "2026-10-19", "America/Vancouver"),
        task("east-tomorrow", "Eastern tomorrow", "2026-10-20", "Asia/Tokyo"),
        task("west-overdue", "Western overdue", "2026-10-17", "America/Vancouver"),
        task("east-overdue", "Eastern overdue", "2026-10-18", "Asia/Tokyo"),
        task("east-next-week", "Eastern next week", "2026-10-26", "Asia/Tokyo"),
    ]
    snapshot(backend, records)
    spoken = await say(hass, phrase)
    for record in records:
        summary = str(record["summary"])
        assert (summary in spoken) is (summary in expected)
    assert not any(request.method == "POST" for request in backend.requests)


@pytest.mark.freeze_time("2026-10-17T12:00:00Z")
async def test_upcoming_includes_distant_work_and_limits_spoken_items(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    records = [
        task(f"task-{number}", f"Example task {number}", "2027-01-15")
        for number in range(7)
    ]
    snapshot(backend, list(reversed(records)))
    spoken = await say(hass, "what maintenance is coming up")
    for number in range(7):
        assert (f"Example task {number}" in spoken) is (number < 5)
    assert "7" in spoken
    assert "January 15, 2027" in spoken


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("archived", True),
        ("status", "completed"),
        ("schedule", {"kind": "instruction"}),
    ],
)
async def test_queries_and_completions_exclude_inactive_references(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
    field: str,
    value: object,
) -> None:
    record = item_record()
    record[field] = value
    snapshot(backend, [record])
    assert "No " in await say(hass, "what is my next maintenance task")
    assert "No matching" in await say(hass, "complete maintenance task replace filter")
    assert not any(request.method == "POST" for request in backend.requests)


async def test_undated_task_can_be_completed_but_has_no_dated_query_result(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    snapshot(backend, [item_record(due=None, schedule_kind="standing")])
    assert "No " in await say(hass, "what is my next maintenance task")
    spoken = await say(hass, "complete maintenance task replace filter")
    assert "Completed Replace filter" in spoken
    write = next(request for request in backend.requests if request.method == "POST")
    assert write.headers["X-Maintenance-System-Actor"] == "ha:automation"


async def test_ambiguity_requires_a_new_qualified_command(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    snapshot(
        backend,
        [
            task("inspect-main-filter", "Inspect main filter", "2026-10-15"),
            task("inspect-spare-filter", "Inspect spare filter", "2026-10-16"),
        ],
    )
    spoken = await say(hass, "complete maintenance task inspect filter")
    assert "Inspect main filter" in spoken
    assert "Inspect spare filter" in spoken
    assert "complete maintenance task" in spoken
    assert not any(request.method == "POST" for request in backend.requests)
    await say(hass, "the first one")
    await say(hass, "yes")
    assert not any(request.method == "POST" for request in backend.requests)
    assert "Completed Inspect main filter" in await say(
        hass, "complete maintenance task inspect main filter"
    )
    writes = [request for request in backend.requests if request.method == "POST"]
    assert len(writes) == 1
    assert writes[0].path == "/v1/items/inspect-main-filter/completions"


@pytest.mark.parametrize("spoken_name", ["REPLACE FILTER", "replace-filter", "filter"])
async def test_exact_names_and_token_aware_partial_matching(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
    spoken_name: str,
) -> None:
    snapshot(
        backend,
        [
            item_record(),
            task("replace-filter-cover", "Replace filter cover", "2026-10-16"),
        ]
        if spoken_name != "filter"
        else [item_record()],
    )
    spoken = await say(hass, f"complete maintenance task {spoken_name}")
    assert "Completed Replace filter" in spoken
    assert (
        len([request for request in backend.requests if request.method == "POST"]) == 1
    )


@pytest.mark.parametrize(
    "spoken_name", ["fil", "replce filter", "replace refrigerator"]
)
async def test_completion_never_guesses_fuzzy_or_substring_matches(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
    spoken_name: str,
) -> None:
    assert "No matching" in await say(hass, f"complete maintenance task {spoken_name}")
    assert not any(request.method == "POST" for request in backend.requests)


async def test_renamed_entity_retains_explicit_assist_opt_in(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
    entity_registry: er.EntityRegistry,
) -> None:
    entity_registry.async_update_entity(
        "todo.maintenance", new_entity_id="todo.example_tasks"
    )
    await hass.async_block_till_done()
    assert "Replace filter" in await say(hass, "what is my next maintenance task")
    async_expose_entity(hass, "conversation", "todo.example_tasks", False)
    before = len(backend.requests)
    assert "not exposed" in await say(hass, "what is my next maintenance task")
    assert "not exposed" in await say(hass, "complete maintenance task replace filter")
    assert len(backend.requests) == before


@pytest.mark.parametrize(
    "phrase",
    ["what is my next maintenance task", "complete maintenance task replace filter"],
)
async def test_unknown_inactive_and_denied_users_fail_closed(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
    phrase: str,
) -> None:
    denied = MockUser().add_to_hass(hass)
    inactive = MockUser(is_active=False).add_to_hass(hass)
    for user_id in ("removed-example-user", denied.id, inactive.id):
        before = len(backend.requests)
        assert "permission" in await say(hass, phrase, Context(user_id=user_id))
        assert len(backend.requests) == before


async def test_read_only_user_can_query_but_cannot_complete(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    user = MockUser().add_to_hass(hass)
    user.mock_policy({"entities": {"entity_ids": {"todo.maintenance": {"read": True}}}})
    context = Context(user_id=user.id)
    assert "Replace filter" in await say(
        hass, "what is my next maintenance task", context
    )
    assert "permission" in await say(
        hass, "complete maintenance task replace filter", context
    )
    assert not any(request.method == "POST" for request in backend.requests)


async def test_refresh_failure_and_stale_completion_never_retry_writes(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    backend.responses[("GET", "/v1/summary")] = (503, b"", {})
    assert "unavailable" in await say(hass, "complete maintenance task replace filter")
    assert not any(request.method == "POST" for request in backend.requests)
    del backend.responses[("GET", "/v1/summary")]
    backend.responses[("POST", f"/v1/items/{ITEM_ID}/completions")] = (412, b"", {})
    assert "changed" in await say(hass, "complete maintenance task replace filter")
    assert (
        len([request for request in backend.requests if request.method == "POST"]) == 1
    )


async def test_same_request_retries_have_a_stable_idempotency_key(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    context = Context()
    await say(hass, "complete maintenance task replace filter", context)
    await say(hass, "complete maintenance task replace filter", context)
    writes = [request for request in backend.requests if request.method == "POST"]
    assert len(writes) == 2
    assert writes[0].headers["Idempotency-Key"] == writes[1].headers["Idempotency-Key"]
    await say(hass, "complete maintenance task replace filter")
    final = next(
        request for request in reversed(backend.requests) if request.method == "POST"
    )
    assert final.headers["Idempotency-Key"] != writes[0].headers["Idempotency-Key"]


async def test_unload_removes_sentences_and_reload_registers_once(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    assert await hass.config_entries.async_unload(voice_entry.entry_id)
    before = len(backend.requests)
    assert "Replace filter" not in await say(hass, "what is my next maintenance task")
    assert len(backend.requests) == before
    assert await hass.config_entries.async_setup(voice_entry.entry_id)
    await hass.async_block_till_done()
    assert "Completed Replace filter" in await say(
        hass, "complete maintenance task replace filter"
    )
    assert (
        len([request for request in backend.requests if request.method == "POST"]) == 1
    )


@pytest.mark.parametrize("unload", [False, True], ids=["revoke-exposure", "unload"])
async def test_access_is_rechecked_after_refresh(
    hass: HomeAssistant,
    voice_entry: MaintenanceConfigEntry,
    backend: Backend,
    unload: bool,
) -> None:
    backend.delay_seconds = 0.03
    backend.request_received.clear()
    request = hass.async_create_task(
        say(hass, "complete maintenance task replace filter")
    )
    async with asyncio.timeout(5):
        await backend.request_received.wait()
    if unload:
        assert await hass.config_entries.async_unload(voice_entry.entry_id)
    else:
        async_expose_entity(hass, "conversation", "todo.maintenance", False)
    spoken = await request
    assert ("unavailable" if unload else "not exposed") in spoken
    assert not any(incoming.method == "POST" for incoming in backend.requests)


async def test_voice_completion_and_retry_against_real_rust_backend(
    hass: HomeAssistant,
    hass_user: MockUser,
    init_core: None,
    rust_backend: RustBackend,
) -> None:
    """An actual Assist request completes once and preserves durable attribution."""
    session = async_get_clientsession(hass)
    headers = {"Authorization": f"Bearer {rust_backend.owner_token}"}
    async with session.post(
        f"{rust_backend.origin}/v1/items",
        headers={**headers, "Idempotency-Key": "create-example-voice-task"},
        json={
            "id": "inspect-example-filter",
            "name": "Inspect example filter",
            "summary": "Inspect the example filter",
            "timezone": "UTC",
            "due": "2025-01-01",
            "schedule": {
                "kind": "recurring",
                "interval": {"days": 1, "months": 0, "years": 0},
            },
        },
    ) as response:
        assert response.status == 201
        original = MaintenanceItem.parse(await response.json())
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    async_expose_entity(hass, "conversation", "todo.maintenance", True)
    assert "Inspect the example filter" in await say(
        hass, "what maintenance is overdue"
    )
    context = Context(user_id=hass_user.id)
    assert "Completed Inspect the example filter" in await say(
        hass, "complete maintenance task inspect example filter", context
    )
    async with session.get(
        f"{rust_backend.origin}/v1/items/{original.id}", headers=headers
    ) as response:
        assert response.status == 200
        completed = MaintenanceItem.parse(await response.json())
    assert completed.revision == 2
    assert completed.occurrence_id != original.occurrence_id
    assert "changed" in await say(
        hass, "complete maintenance task inspect example filter", context
    )
    async with session.get(
        f"{rust_backend.origin}/v1/items/{original.id}", headers=headers
    ) as response:
        assert response.status == 200
        retried = MaintenanceItem.parse(await response.json())
        # Check the Rust-produced completion record as well as the native snapshot.
        record = await response.json()
    assert retried == completed
    assert isinstance(record, dict)
    history = record["last_completion"]
    assert isinstance(history, dict)
    actor = history["actor"]
    assert isinstance(actor, dict)
    assert actor["kind"] == ActorKind.USER
    assert actor["id"] == f"ha:{hass_user.id}"
