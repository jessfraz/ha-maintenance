"""Maintenance tools through Core's actual Assist API and Rust transport."""

import asyncio
import json
from uuid import NAMESPACE_URL, uuid5

import probatio
import pytest
from homeassistant.components.homeassistant.exposed_entities import async_expose_entity
from homeassistant.core import Context, HomeAssistant
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import llm
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.setup import async_setup_component
from homeassistant.util.json import JsonObjectType
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import (
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)
from custom_components.home_maintenance.models import (
    ActorKind,
    CompletionActor,
    MaintenanceItem,
)

from .conftest import ITEM_ID, NEXT_OCCURRENCE_ID, OCCURRENCE_ID, Backend, item_record
from .test_rust_backend import RustBackend
from .test_rust_backend import rust_backend as rust_backend

QUERY = "home_maintenance__query"
COMPLETE = "home_maintenance__complete"
COMPLETE_ARGUMENTS: JsonObjectType = {
    "item_id": ITEM_ID,
    "occurrence_id": OCCURRENCE_ID,
    "revision": 1,
}


async def api(
    hass: HomeAssistant, context: Context | None, assistant: str = "conversation"
) -> llm.APIInstance:
    assert await async_setup_component(hass, "llm", {})
    return await llm.async_get_api(
        hass,
        "assist",
        llm.LLMContext(
            platform="test",
            context=context,
            language="en",
            assistant=assistant,
            device_id=None,
        ),
    )


async def call(
    api: llm.APIInstance,
    name: str = QUERY,
    arguments: JsonObjectType | None = None,
    *,
    tool_id: str | None = None,
) -> llm.ToolResult:
    request = llm.ToolInput(tool_name=name, tool_args=arguments or {})
    if tool_id is not None:
        request.id = tool_id
    return await api.async_call_tool(request)


def items(result: llm.ToolResult) -> list[JsonObjectType]:
    assert not result.error, result.data
    records = result.data["items"]
    assert isinstance(records, list)
    parsed: list[JsonObjectType] = []
    for record in records:
        assert isinstance(record, dict)
        parsed.append(record)
    return parsed


def snapshot(backend: Backend, records: list[dict[str, object]]) -> None:
    backend.responses[("GET", "/v1/items")] = (
        200,
        json.dumps({"items": records, "next_page": None}).encode(),
        {},
    )


def task(
    identity: str, summary: str, due: str | None, timezone: str = "UTC"
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


@pytest.fixture
async def assist_api(
    hass: HomeAssistant, entry: MaintenanceConfigEntry, hass_user: MockUser
) -> llm.APIInstance:
    """Discover integration tools using the existing native Assist API."""
    async_expose_entity(hass, "conversation", "todo.maintenance", True)
    return await api(hass, Context(user_id=hass_user.id))


async def test_native_assist_api_discovers_maintenance_tools(
    assist_api: llm.APIInstance,
) -> None:
    assert {
        tool.name for tool in assist_api.tools if tool.integration == "home_maintenance"
    } == {"home_maintenance__query", "home_maintenance__complete"}


async def test_tool_parameters_serialize_with_the_native_provider_converter(
    assist_api: llm.APIInstance,
) -> None:
    """The provider receives usable closed JSON schemas, not Python validators."""
    for tool in assist_api.tools:
        if tool.integration != DOMAIN:
            continue
        schema = probatio.to_openapi(
            tool.parameters,
            custom_serializer=assist_api.custom_serializer,
            openapi_version="3.1.0",
        )
        assert schema["type"] == "object"
        assert schema["additionalProperties"] is False
        properties = schema["properties"]
        if tool.name == QUERY:
            assert properties["limit"]["type"] == "integer"
            assert properties["limit"]["maximum"] == 20
        else:
            assert set(schema["required"]) == {"item_id", "occurrence_id", "revision"}
            assert properties["revision"]["type"] == "integer"


async def test_query_reads_fresh_minimal_structured_projection(
    assist_api: llm.APIInstance,
    backend: Backend,
) -> None:
    snapshot(backend, [item_record(summary="Inspect example appliance")])
    result = await call(assist_api)
    assert result.data == {
        "query": "active",
        "total": 1,
        "truncated": False,
        "items": [
            {
                "id": ITEM_ID,
                "name": "Replace filter",
                "summary": "Inspect example appliance",
                "due": "2026-10-15",
                "timezone": "UTC",
                "occurrence_id": OCCURRENCE_ID,
                "revision": 1,
                "status": "needs_action",
            }
        ],
    }
    assert not result.error
    assert not any(request.method == "POST" for request in backend.requests)


async def test_completion_uses_authenticated_context_and_exact_snapshot(
    assist_api: llm.APIInstance,
    backend: Backend,
    hass_user: MockUser,
) -> None:
    backend.responses[("POST", f"/v1/items/{ITEM_ID}/completions")] = (
        200,
        json.dumps(
            item_record(occurrence_id=NEXT_OCCURRENCE_ID, revision=2, due="2027-01-15")
        ).encode(),
        {"ETag": '"2"'},
    )
    result = await call(assist_api, COMPLETE, COMPLETE_ARGUMENTS)
    assert not result.error
    assert result.data["completed"] is True
    returned = result.data["item"]
    assert isinstance(returned, dict)
    assert returned["occurrence_id"] == NEXT_OCCURRENCE_ID
    assert returned["revision"] == 2
    assert returned["due"] == "2027-01-15"
    writes = [request for request in backend.requests if request.method == "POST"]
    assert len(writes) == 1
    assert json.loads(writes[0].body) == {"occurrence_id": OCCURRENCE_ID}
    assert writes[0].headers["If-Match"] == '"1"'
    assert writes[0].headers["X-Maintenance-Actor-Id"] == f"ha:{hass_user.id}"
    assert "X-Maintenance-System-Actor" not in writes[0].headers


@pytest.mark.parametrize(
    ("query", "expected"),
    [
        ("today", {"Western today", "Eastern today"}),
        ("tomorrow", {"Western tomorrow", "Eastern tomorrow"}),
        (
            "this_week",
            {"Western today", "Eastern today", "Eastern tomorrow", "Western overdue"},
        ),
        ("next_week", {"Western tomorrow", "Eastern next week"}),
        ("overdue", {"Western overdue", "Eastern overdue"}),
        ("next", {"Western overdue"}),
    ],
)
@pytest.mark.freeze_time("2026-10-19T00:30:00Z")
async def test_date_queries_use_each_tasks_timezone(
    assist_api: llm.APIInstance,
    backend: Backend,
    query: str,
    expected: set[str],
) -> None:
    snapshot(
        backend,
        [
            task("west-today", "Western today", "2026-10-18", "America/Vancouver"),
            task("east-today", "Eastern today", "2026-10-19", "Asia/Tokyo"),
            task(
                "west-tomorrow", "Western tomorrow", "2026-10-19", "America/Vancouver"
            ),
            task("east-tomorrow", "Eastern tomorrow", "2026-10-20", "Asia/Tokyo"),
            task("west-overdue", "Western overdue", "2026-10-17", "America/Vancouver"),
            task("east-overdue", "Eastern overdue", "2026-10-18", "Asia/Tokyo"),
            task("east-next-week", "Eastern next week", "2026-10-26", "Asia/Tokyo"),
        ],
    )
    result = await call(assist_api, arguments={"query": query, "limit": 20})
    assert {record["summary"] for record in items(result)} == expected
    assert not any(request.method == "POST" for request in backend.requests)


@pytest.mark.parametrize("limit", [1, 5, 20])
@pytest.mark.freeze_time("2026-10-17T12:00:00Z")
async def test_upcoming_is_deterministic_bounded_and_not_limited_to_fourteen_days(
    assist_api: llm.APIInstance,
    backend: Backend,
    limit: int,
) -> None:
    records = [
        task(f"task-{number:02}", f"Example task {number}", "2027-01-15")
        for number in range(21)
    ]
    snapshot(backend, list(reversed(records)))
    result = await call(assist_api, arguments={"query": "upcoming", "limit": limit})
    assert [record["id"] for record in items(result)] == [
        f"task-{number:02}" for number in range(limit)
    ]
    assert result.data["total"] == 21
    assert result.data["truncated"] is True
    next_result = await call(assist_api, arguments={"query": "next", "limit": 20})
    assert len(items(next_result)) == 1
    assert next_result.data["total"] == 21
    assert next_result.data["truncated"] is True


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("archived", True),
        ("status", "completed"),
        ("schedule", {"kind": "instruction"}),
    ],
)
async def test_tools_exclude_inactive_and_instruction_references(
    assist_api: llm.APIInstance,
    backend: Backend,
    field: str,
    value: object,
) -> None:
    record = item_record()
    record[field] = value
    snapshot(backend, [record])
    assert items(await call(assist_api)) == []
    result = await call(assist_api, COMPLETE, COMPLETE_ARGUMENTS)
    assert result.error and result.data["error"] == "not_found"
    assert not any(request.method == "POST" for request in backend.requests)


async def test_undated_work_is_active_and_can_be_completed(
    assist_api: llm.APIInstance,
    backend: Backend,
) -> None:
    snapshot(backend, [item_record(due=None, schedule_kind="standing")])
    assert len(items(await call(assist_api))) == 1
    for query in (
        "upcoming",
        "overdue",
        "next",
        "today",
        "tomorrow",
        "this_week",
        "next_week",
    ):
        assert items(await call(assist_api, arguments={"query": query})) == []
    assert not (await call(assist_api, COMPLETE, COMPLETE_ARGUMENTS)).error


async def test_ambiguous_name_query_returns_candidates_without_mutating(
    assist_api: llm.APIInstance,
    backend: Backend,
) -> None:
    snapshot(
        backend,
        [
            task("inspect-main-filter", "Inspect main filter", "2026-10-15"),
            task("inspect-spare-filter", "Inspect spare filter", "2026-10-16"),
        ],
    )
    result = await call(assist_api, arguments={"name": "inspect filter"})
    candidates = items(result)
    assert [record["id"] for record in candidates] == [
        "inspect-main-filter",
        "inspect-spare-filter",
    ]
    assert result.data["total"] == 2
    assert not any(request.method == "POST" for request in backend.requests)
    selected = candidates[1]
    completed = await call(
        assist_api,
        COMPLETE,
        {
            "item_id": selected["id"],
            "occurrence_id": selected["occurrence_id"],
            "revision": selected["revision"],
        },
    )
    assert not completed.error
    writes = [request for request in backend.requests if request.method == "POST"]
    assert len(writes) == 1
    assert writes[0].path == "/v1/items/inspect-spare-filter/completions"


@pytest.mark.parametrize(
    ("name", "expected"),
    [
        ("REPLACE FILTER", [ITEM_ID]),
        ("replace-filter", [ITEM_ID]),
        ("filter", [ITEM_ID, "replace-filter-cover"]),
        ("fil", []),
        ("replce filter", []),
    ],
)
async def test_name_filter_prefers_exact_then_complete_word_matches(
    assist_api: llm.APIInstance,
    backend: Backend,
    name: str,
    expected: list[str],
) -> None:
    snapshot(
        backend,
        [
            item_record(),
            task("replace-filter-cover", "Replace filter cover", "2026-10-16"),
        ],
    )
    assert [
        record["id"]
        for record in items(await call(assist_api, arguments={"name": name}))
    ] == expected


@pytest.mark.parametrize(
    ("tool", "arguments"),
    [
        (QUERY, {"limit": True}),
        (QUERY, {"limit": 1.0}),
        (QUERY, {"limit": "1"}),
        (QUERY, {"limit": 0}),
        (QUERY, {"limit": 21}),
        (QUERY, {"query": "all"}),
        (QUERY, {"name": " "}),
        (QUERY, {"name": 4}),
        (QUERY, {"unknown": "value"}),
        (COMPLETE, {}),
        (COMPLETE, {**COMPLETE_ARGUMENTS, "revision": True}),
        (COMPLETE, {**COMPLETE_ARGUMENTS, "revision": 1.0}),
        (COMPLETE, {**COMPLETE_ARGUMENTS, "revision": "1"}),
        (COMPLETE, {**COMPLETE_ARGUMENTS, "revision": 0}),
        (COMPLETE, {**COMPLETE_ARGUMENTS, "occurrence_id": "not-a-uuid"}),
        (COMPLETE, {**COMPLETE_ARGUMENTS, "occurrence_id": "g" * 36}),
        (COMPLETE, {**COMPLETE_ARGUMENTS, "item_id": "../other-item"}),
        (COMPLETE, {**COMPLETE_ARGUMENTS, "item_id": "replace-filter\n"}),
        (COMPLETE, {**COMPLETE_ARGUMENTS, "actor_id": "not-trusted"}),
    ],
)
async def test_native_api_invalid_arguments_are_rejected_before_backend_io(
    assist_api: llm.APIInstance,
    backend: Backend,
    tool: str,
    arguments: JsonObjectType,
) -> None:
    before = len(backend.requests)
    result = await call(assist_api, tool, arguments)
    assert result.error and result.data["error"] == "invalid_arguments"
    assert len(backend.requests) == before


@pytest.mark.parametrize("status", [409, 412])
async def test_conflicts_are_structured_redacted_and_never_retried(
    assist_api: llm.APIInstance,
    backend: Backend,
    status: int,
) -> None:
    backend.responses[("POST", f"/v1/items/{ITEM_ID}/completions")] = (
        status,
        b"private-backend-body",
        {},
    )
    result = await call(assist_api, COMPLETE, COMPLETE_ARGUMENTS)
    assert result.error and result.data["error"] == "conflict"
    assert "private-backend-body" not in str(result.data)
    assert (
        len([request for request in backend.requests if request.method == "POST"]) == 1
    )


async def test_stale_occurrence_or_revision_cannot_complete_a_successor(
    assist_api: llm.APIInstance,
    backend: Backend,
) -> None:
    snapshot(backend, [item_record(occurrence_id=NEXT_OCCURRENCE_ID, revision=2)])
    for arguments in (
        COMPLETE_ARGUMENTS,
        {**COMPLETE_ARGUMENTS, "occurrence_id": NEXT_OCCURRENCE_ID},
    ):
        result = await call(assist_api, COMPLETE, arguments)
        assert result.error and result.data["error"] == "conflict"
    assert not any(request.method == "POST" for request in backend.requests)


async def test_backend_refresh_and_mutation_failures_are_redacted(
    assist_api: llm.APIInstance,
    backend: Backend,
) -> None:
    backend.responses[("GET", "/v1/summary")] = (503, b"private-backend-body", {})
    result = await call(assist_api, COMPLETE, COMPLETE_ARGUMENTS)
    assert result.error and result.data["error"] == "unavailable"
    assert not any(request.method == "POST" for request in backend.requests)
    del backend.responses[("GET", "/v1/summary")]
    backend.responses[("POST", f"/v1/items/{ITEM_ID}/completions")] = (
        500,
        b"private-backend-body",
        {},
    )
    result = await call(assist_api, COMPLETE, COMPLETE_ARGUMENTS)
    assert result.error and result.data["error"] == "backend_error"
    assert "private-backend-body" not in str(result.data)


async def test_renamed_entity_and_changed_exposure_affect_cached_tools(
    hass: HomeAssistant,
    assist_api: llm.APIInstance,
    backend: Backend,
    entity_registry: er.EntityRegistry,
) -> None:
    entity_registry.async_update_entity(
        "todo.maintenance", new_entity_id="todo.example_tasks"
    )
    await hass.async_block_till_done()
    assert len(items(await call(assist_api))) == 1
    async_expose_entity(hass, "conversation", "todo.example_tasks", False)
    before = len(backend.requests)
    result = await call(assist_api)
    assert result.error and result.data["error"] == "not_exposed"
    assert len(backend.requests) == before
    hidden = await llm.async_get_api(hass, "assist", assist_api.llm_context)
    assert not any(tool.integration == DOMAIN for tool in hidden.tools)


async def test_exposure_uses_the_actual_llm_assistant(
    hass: HomeAssistant,
    assist_api: llm.APIInstance,
) -> None:
    hidden = await api(hass, None, "example_assistant")
    assert not any(tool.integration == DOMAIN for tool in hidden.tools)
    async_expose_entity(hass, "example_assistant", "todo.maintenance", True)
    exposed = await api(hass, None, "example_assistant")
    assert len(items(await call(exposed))) == 1


@pytest.mark.parametrize("tool", [QUERY, COMPLETE])
async def test_inactive_unknown_and_denied_users_cannot_access_backend(
    hass: HomeAssistant,
    assist_api: llm.APIInstance,
    backend: Backend,
    tool: str,
) -> None:
    denied = MockUser().add_to_hass(hass)
    inactive = MockUser(is_active=False).add_to_hass(hass)
    for user_id in ("removed-example-user", denied.id, inactive.id):
        caller = await api(hass, Context(user_id=user_id))
        before = len(backend.requests)
        result = await call(
            caller, tool, COMPLETE_ARGUMENTS if tool == COMPLETE else {}
        )
        assert result.error and result.data["error"] == "permission_denied"
        assert len(backend.requests) == before


async def test_read_only_user_can_query_but_cannot_complete(
    hass: HomeAssistant,
    assist_api: llm.APIInstance,
    backend: Backend,
) -> None:
    user = MockUser().add_to_hass(hass)
    user.mock_policy({"entities": {"entity_ids": {"todo.maintenance": {"read": True}}}})
    caller = await api(hass, Context(user_id=user.id))
    assert len(items(await call(caller))) == 1
    before = len(backend.requests)
    result = await call(caller, COMPLETE, COMPLETE_ARGUMENTS)
    assert result.error and result.data["error"] == "permission_denied"
    assert len(backend.requests) == before


async def test_cached_api_is_invalid_after_unload_and_reload(
    hass: HomeAssistant,
    assist_api: llm.APIInstance,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    assert await hass.config_entries.async_unload(entry.entry_id)
    before = len(backend.requests)
    result = await call(assist_api)
    assert result.error and result.data["error"] == "unavailable"
    assert len(backend.requests) == before
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    before = len(backend.requests)
    result = await call(assist_api)
    assert result.error and result.data["error"] == "unavailable"
    assert len(backend.requests) == before
    fresh = await llm.async_get_api(hass, "assist", assist_api.llm_context)
    assert len(items(await call(fresh))) == 1


@pytest.mark.parametrize("unload", [False, True])
async def test_access_is_rechecked_after_inflight_refresh(
    hass: HomeAssistant,
    assist_api: llm.APIInstance,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    unload: bool,
) -> None:
    backend.delay_seconds = 0.03
    backend.request_received.clear()
    request = hass.async_create_task(call(assist_api, COMPLETE, COMPLETE_ARGUMENTS))
    async with asyncio.timeout(5):
        await backend.request_received.wait()
    if unload:
        assert await hass.config_entries.async_unload(entry.entry_id)
    else:
        async_expose_entity(hass, "conversation", "todo.maintenance", False)
    result = await request
    assert result.error
    assert result.data["error"] == ("unavailable" if unload else "not_exposed")
    assert not any(incoming.method == "POST" for incoming in backend.requests)


@pytest.mark.parametrize("context_present", [False, True])
async def test_tool_ids_and_trusted_context_make_retries_idempotent(
    hass: HomeAssistant,
    assist_api: llm.APIInstance,
    backend: Backend,
    context_present: bool,
) -> None:
    caller = await api(hass, Context() if context_present else None)
    for _ in range(2):
        assert not (
            await call(caller, COMPLETE, COMPLETE_ARGUMENTS, tool_id="same-invocation")
        ).error
    writes = [request for request in backend.requests if request.method == "POST"]
    assert len(writes) == 2
    assert writes[0].headers["Idempotency-Key"] == writes[1].headers["Idempotency-Key"]
    assert writes[0].headers["X-Maintenance-System-Actor"] == "ha:automation"
    await call(caller, COMPLETE, COMPLETE_ARGUMENTS, tool_id="different-invocation")
    final = next(
        request for request in reversed(backend.requests) if request.method == "POST"
    )
    assert final.headers["Idempotency-Key"] != writes[0].headers["Idempotency-Key"]


@pytest.mark.parametrize("human", [True, False])
async def test_actual_rust_completion_history_and_successor_retry(
    hass: HomeAssistant,
    hass_user: MockUser,
    rust_backend: RustBackend,
    human: bool,
) -> None:
    session = async_get_clientsession(hass)
    headers = {"Authorization": f"Bearer {rust_backend.owner_token}"}
    async with session.post(
        f"{rust_backend.origin}/v1/items",
        headers={**headers, "Idempotency-Key": "create-example-tool-task"},
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
    caller = await api(hass, Context(user_id=hass_user.id) if human else None)
    before = items(await call(caller, arguments={"name": "inspect example filter"}))[0]
    arguments: JsonObjectType = {
        "item_id": before["id"],
        "occurrence_id": before["occurrence_id"],
        "revision": before["revision"],
    }
    assert not (
        await call(caller, COMPLETE, arguments, tool_id="single-tool-call")
    ).error
    after = items(await call(caller, arguments={"name": "inspect example filter"}))[0]
    assert after["revision"] == 2 and after["occurrence_id"] != before["occurrence_id"]
    stale = await call(caller, COMPLETE, arguments, tool_id="single-tool-call")
    assert stale.error and stale.data["error"] == "conflict"
    # Even rewritten successor arguments retain the same invocation's idempotency key.
    advanced = await call(
        caller,
        COMPLETE,
        {
            "item_id": after["id"],
            "occurrence_id": after["occurrence_id"],
            "revision": after["revision"],
        },
        tool_id="single-tool-call",
    )
    assert advanced.error and advanced.data["error"] == "conflict"
    async with session.get(
        f"{rust_backend.origin}/v1/items/{original.id}", headers=headers
    ) as response:
        assert response.status == 200
        record = await response.json()
    current = MaintenanceItem.parse(record)
    assert current.revision == 2
    assert isinstance(record, dict)
    history = record["last_completion"]
    assert isinstance(history, dict)
    actor = CompletionActor.parse(history["actor"])
    assert actor.kind is (ActorKind.USER if human else ActorKind.SYSTEM)
    assert actor.id == (f"ha:{hass_user.id}" if human else "ha:automation")
    assert history["occurrence_id"] == str(original.occurrence_id)
    assert current.due is not None
    assert history["next_due"] == current.due.isoformat()
    async with session.get(
        f"{rust_backend.origin}/v1/items/{original.id}/events", headers=headers
    ) as response:
        assert response.status == 200
        events = await response.json()
    assert isinstance(events, dict)
    assert len(events["items"]) == 1
