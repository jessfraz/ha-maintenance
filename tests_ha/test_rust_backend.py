"""Cross-language contract through real HA and a compiled Rust service."""

import asyncio
import json
import os
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import UTC, date, datetime, timedelta
from pathlib import Path

import pytest
from aiohttp import ClientError, ClientTimeout
from homeassistant.config_entries import SOURCE_HASSIO, SOURCE_USER, ConfigEntryState
from homeassistant.core import Context, HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import instance_id
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.service_info.hassio import HassioServiceInfo
from homeassistant.util.json import JsonObjectType
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from custom_components.home_maintenance.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
    MAX_RESPONSE_BYTES,
    SUMMARY_KEYS,
)
from custom_components.home_maintenance.models import (
    ActorKind,
    ChecklistStep,
    ItemStatus,
    MaintenanceItem,
)

from .conftest import SECOND_STEP_ID, STEP_ID, RegisteredObjects, entity_items

PREFIX = "/api/home_maintenance/v1"
OWNER_TOKEN = "test-owner-credential-at-least-32-characters"
INTEGRATION_TOKEN = "test-integration-credential-at-least-32-characters"


@dataclass
class RustBackend:
    """A disposable real service with credentials excluded from test output."""

    origin: str
    owner_token: str = field(repr=False)
    integration_token: str = field(repr=False)


@pytest.fixture
async def rust_backend(
    hass: HomeAssistant,
    tmp_path: Path,
    unused_tcp_port: int,
    request: pytest.FixtureRequest,
    socket_enabled: None,
) -> AsyncIterator[RustBackend]:
    """Launch the actual binary against disposable SQLite and private options."""
    binary = Path(
        os.environ.get("MAINTENANCE_TEST_BINARY", "target/debug/home-maintenance")
    )
    if not await asyncio.to_thread(binary.is_file):
        pytest.fail(
            "Build the backend first: cargo build --locked --bin home-maintenance"
        )
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("MAINTENANCE_", "SUPERVISOR"))
    }
    arguments = [
        "--bind",
        f"127.0.0.1:{unused_tcp_port}",
        "--database",
        str(tmp_path / "maintenance.sqlite3"),
    ]
    app_mode = getattr(request, "param", False)
    owner_token = None if app_mode else OWNER_TOKEN
    integration_token = INTEGRATION_TOKEN
    if app_mode:
        options = tmp_path / "options.json"
        await asyncio.to_thread(options.write_text, "{}")
        arguments.extend(("--options", str(options)))
    else:
        environment["MAINTENANCE_API_TOKEN"] = OWNER_TOKEN
        environment["MAINTENANCE_INTEGRATION_TOKEN"] = INTEGRATION_TOKEN
    origin = f"http://127.0.0.1:{unused_tcp_port}"
    process = await asyncio.create_subprocess_exec(
        str(await asyncio.to_thread(binary.resolve)),
        *arguments,
        env=environment,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(5):
            while True:
                if process.returncode is not None:
                    output = (
                        (await process.stderr.read()).decode(errors="replace")
                        if process.stderr is not None
                        else ""
                    )
                    for token in (owner_token, integration_token):
                        if token:
                            output = output.replace(token, "[credential]")
                    pytest.fail(
                        f"The Rust service exited during startup: {output[:2000]}"
                    )
                if owner_token is None:
                    try:
                        credentials = json.loads(
                            await asyncio.to_thread(
                                (tmp_path / "credentials.json").read_text
                            )
                        )
                    except FileNotFoundError:
                        await asyncio.sleep(0.01)
                        continue
                    owner_token = credentials["owner"]
                    integration_token = credentials["integration"]
                try:
                    async with async_get_clientsession(hass).get(
                        f"{origin}/v1/system",
                        headers={"Authorization": f"Bearer {owner_token}"},
                        timeout=ClientTimeout(total=1),
                    ) as response:
                        if response.status == 200:
                            break
                except ClientError, TimeoutError:
                    pass
                await asyncio.sleep(0.01)
        yield RustBackend(origin, owner_token, integration_token)
    finally:
        if process.returncode is None:
            process.terminate()
            try:
                async with asyncio.timeout(5):
                    await process.wait()
            except TimeoutError:
                process.kill()
                await process.wait()


async def test_ha_to_rust_maintenance_contract(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    """HA owns identity; Rust owns revision, recurrence and completion history."""
    configured = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"], {"next_step_id": "standalone"}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"],
        {
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    assert configured["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done()
    entry = configured["result"]
    assert entry.state is ConfigEntryState.LOADED
    admin = await hass_client()
    user = await hass_client(hass_user_token)
    definition = {
        "id": "replace-filter",
        "name": "Replace filter",
        "summary": "Replace filter",
        "timezone": "UTC",
        "due": "2025-01-01",
        "schedule": {
            "kind": "recurring",
            "interval": {"days": 1, "months": 0, "years": 0},
        },
        "instructions": ["Turn off the appliance", "Replace the filter"],
    }
    assert (await user.post(f"{PREFIX}/items", json=definition)).status == 403
    created = await admin.post(
        f"{PREFIX}/items",
        json=definition,
        headers={"Idempotency-Key": "create-filter"},
    )
    assert created.status == 201
    first = await created.json()
    initial_etag = created.headers["ETag"]
    await entry.runtime_data.async_refresh()
    for entity_id in ("todo.maintenance", "sensor.maintenance_overdue"):
        state = hass.states.get(entity_id)
        assert state is not None
        assert state.state == "1"

    # Exercise HA's actual entity service rather than calling the adapter method.
    from homeassistant.core import Context

    await hass.services.async_call(
        "todo",
        "update_item",
        {
            "entity_id": "todo.maintenance",
            "item": first["occurrence"]["id"],
            "status": "completed",
        },
        context=Context(user_id=hass_user.id),
        blocking=True,
    )
    response = await user.get(f"{PREFIX}/items/replace-filter")
    assert response.status == 200
    second = await response.json()
    second_etag = response.headers["ETag"]
    assert second["revision"] == 2
    assert second["occurrence"]["id"] != first["occurrence"]["id"]
    completion = second["last_completion"]
    assert completion["actor"] == {
        "kind": "user",
        "id": f"ha:{hass_user.id}",
        "name": hass_user.name,
    }
    assert completion["occurrence_id"] == first["occurrence"]["id"]
    assert completion["previous_due"] == "2025-01-01"
    completed_date = datetime.fromisoformat(completion["completed_at"]).date()
    assert date.fromisoformat(completion["next_due"]) == completed_date + timedelta(
        days=1
    )
    assert second["occurrence"]["due"] == completion["next_due"]
    await entry.runtime_data.async_refresh()
    todo = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    items = entity_items(todo, "todo.maintenance")
    assert len(items) == 1
    assert items[0]["uid"] == second["occurrence"]["id"]
    state = hass.states.get("sensor.maintenance_completed")
    assert state is not None
    assert state.state == "1"

    # Both an old occurrence and a stale version fail without a second event.
    rejected = await user.post(
        f"{PREFIX}/items/replace-filter/completions",
        json={"occurrence_id": first["occurrence"]["id"]},
        headers={"If-Match": initial_etag, "Idempotency-Key": "stale-completion"},
    )
    assert rejected.status == 412
    events = await (await user.get(f"{PREFIX}/items/replace-filter/events")).json()
    assert len(events["items"]) == 1
    assert events["items"][0] == completion
    assert events["next_page"] is None

    # A script without a user gets explicit system attribution in durable history.
    result = await hass.services.async_call(
        DOMAIN,
        "complete",
        {"item_id": "replace-filter", "occurrence_id": second["occurrence"]["id"]},
        blocking=True,
        return_response=True,
    )
    assert result is not None
    assert result["revision"] == 3
    events = await (await user.get(f"{PREFIX}/items/replace-filter/events")).json()
    assert len(events["items"]) == 2
    assert events["items"][0]["actor"]["kind"] == "system"
    assert events["items"][0]["actor"]["id"] == "ha:automation"
    forbidden = await user.put(
        f"{PREFIX}/items/replace-filter/archive", headers={"If-Match": second_etag}
    )
    assert forbidden.status == 403
    current = await admin.get(f"{PREFIX}/items/replace-filter")
    archived = await admin.put(
        f"{PREFIX}/items/replace-filter/archive",
        headers={"If-Match": current.headers["ETag"]},
    )
    assert archived.status == 200
    await entry.runtime_data.async_refresh()
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "0"
    state = hass.states.get("sensor.maintenance_completed")
    assert state is not None
    assert state.state == "2"


async def test_reorder_receipt_survives_resolution_and_item_changes(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    """HA derives identity; Rust retains a frozen request without completing work."""
    configured = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"], {"next_step_id": "standalone"}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"],
        {
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    assert configured["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done()
    entry = configured["result"]
    admin = await hass_client()
    user = await hass_client(hass_user_token)
    created = await admin.post(
        f"{PREFIX}/items",
        json={
            "id": "replace-filter",
            "name": "Example filter",
            "summary": "Replace the example filter",
            "timezone": "UTC",
            "due": "2025-01-01",
            "schedule": {
                "kind": "recurring",
                "interval": {"days": 0, "months": 3, "years": 0},
            },
            "reorder_url": "https://example.com/filter",
            "specs": [{"label": "Size", "value": "Example size"}],
        },
        headers={"Idempotency-Key": "create-reorder-item"},
    )
    assert created.status == 201
    original = MaintenanceItem.parse(await created.json())
    assert original.due is not None
    headers = {"If-Match": created.headers["ETag"], "Idempotency-Key": "press-one"}
    requested = await user.post(
        f"{PREFIX}/items/{original.id}/reorders", json={}, headers=headers
    )
    assert requested.status == 201
    assert requested.headers["Cache-Control"] == "no-store"
    receipt = await requested.json()
    assert receipt["item_id"] == original.id
    assert receipt["item_revision"] == original.revision
    assert receipt["item"]["name"] == original.name
    assert receipt["item"]["reorder_url"] == "https://example.com/filter"
    assert receipt["item"]["specs"] == [{"label": "Size", "value": "Example size"}]
    assert receipt["actor"] == {
        "kind": "user",
        "id": f"ha:{hass_user.id}",
        "name": hass_user.name,
    }
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.data.summary["completed"] == 0
    current = await user.get(f"{PREFIX}/items/{original.id}")
    assert MaintenanceItem.parse(await current.json()) == original

    pending = await (await user.get(f"{PREFIX}/reorders")).json()
    assert len(pending["items"]) == 1
    assert pending["items"][0]["request"] == receipt
    request_path = f"{PREFIX}/reorders/{receipt['id']}"
    record = await user.get(request_path)
    assert record.status == 200
    resolution_headers = {"If-Match": record.headers["ETag"]}
    assert (
        await user.put(
            f"{request_path}/resolution",
            json={"status": "fulfilled"},
            headers=resolution_headers,
        )
    ).status == 403
    resolved = await admin.put(
        f"{request_path}/resolution",
        json={"status": "fulfilled"},
        headers=resolution_headers,
    )
    assert resolved.status == 200
    resolved_record = await resolved.json()
    assert resolved_record["status"] == "fulfilled"
    assert resolved_record["request"] == receipt
    assert (await (await user.get(f"{PREFIX}/reorders")).json())["items"] == []
    fulfilled = await (await user.get(f"{PREFIX}/reorders?status=fulfilled")).json()
    assert fulfilled["items"] == [resolved_record]
    archived = await admin.put(
        f"{PREFIX}/items/{original.id}/archive",
        headers={"If-Match": headers["If-Match"]},
    )
    assert archived.status == 200
    replay = await user.post(
        f"{PREFIX}/items/{original.id}/reorders", json={}, headers=headers
    )
    assert replay.status == 201
    assert await replay.json() == receipt
    assert await (await user.get(request_path)).json() == resolved_record


@pytest.mark.parametrize("rust_backend", [True], indirect=True, ids=["app"])
async def test_generated_pairing_reaches_loaded_and_reports_presence(
    hass: HomeAssistant, rust_backend: RustBackend
) -> None:
    """Pair through Supervisor discovery with no manual credential setup."""
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_HASSIO},
        data=HassioServiceInfo(
            config={
                CONF_BACKEND_URL: rust_backend.origin,
                CONF_TOKEN: rust_backend.integration_token,
            },
            name="Maintenance",
            slug="testrepo_home_maintenance",
            uuid="019c6e27e55b73d187d84e01f1f75043",
        ),
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "hassio_confirm"
    assert not hass.config_entries.async_entries(DOMAIN)
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done(wait_background_tasks=True)
    entry = result["result"]
    assert entry.state is ConfigEntryState.LOADED
    assert entry.data[CONF_ADDON_SLUG] == "testrepo_home_maintenance"
    for key in SUMMARY_KEYS:
        state = hass.states.get(f"sensor.maintenance_{key}")
        assert state is not None
        assert state.state == "0"
    async with async_get_clientsession(hass).get(
        f"{rust_backend.origin}/v1/system",
        headers={"Authorization": f"Bearer {rust_backend.integration_token}"},
    ) as response:
        assert response.status == 200
        system = await response.json()
    assert system["credential_role"] == "integration"
    assert system["integration_connected"] is True


async def test_native_checklist_preserves_parent_and_archives_step_progress(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    """Real HA actions update current steps; explicit parent completion resets them."""
    configured = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"], {"next_step_id": "standalone"}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"],
        {
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    assert configured["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done(wait_background_tasks=True)
    entry = configured["result"]
    admin = await hass_client()
    user = await hass_client(hass_user_token)

    async def calendar_events(due: date) -> list[JsonObjectType]:
        response = await user.get(
            "/api/calendars/calendar.maintenance",
            params={
                "start": f"{due.isoformat()}T00:00:00Z",
                "end": f"{(due + timedelta(days=2)).isoformat()}T00:00:00Z",
            },
        )
        assert response.status == 200
        return await response.json()

    created = await admin.post(
        f"{PREFIX}/items",
        json={
            "id": "service-filter",
            "name": "Appliance filter",
            "summary": "Service the appliance filter",
            "description": "Use the specified replacement and keep the area dry.",
            "timezone": "UTC",
            "due": "2025-01-01",
            "schedule": {
                "kind": "recurring",
                "interval": {"days": 1, "months": 0, "years": 0},
            },
            "instructions": ["Follow the appliance instructions."],
            "checklist": [
                {"id": STEP_ID, "summary": "Turn off the appliance"},
                {"id": SECOND_STEP_ID, "summary": "Install the replacement"},
            ],
        },
        headers={"Idempotency-Key": "create-checklist-item"},
    )
    assert created.status == 201
    original_etag = created.headers["ETag"]
    original = MaintenanceItem.parse(await created.json())
    assert original.due is not None
    await entry.runtime_data.async_refresh()
    todo = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    items = entity_items(todo, "todo.maintenance")
    assert len(items) == 1
    assert items[0]["summary"] == original.summary
    description = items[0]["description"]
    assert isinstance(description, str)
    assert "Checklist (0/2 complete)" in description
    scheduled = await calendar_events(original.due)
    assert len(scheduled) == 1
    assert scheduled[0]["uid"] == str(original.occurrence_id)
    assert scheduled[0]["start"] == {"date": original.due.isoformat()}
    assert scheduled[0]["summary"] == original.summary
    description = scheduled[0]["description"]
    assert isinstance(description, str)
    assert "Checklist (0/2 complete)" in description

    first_step = {
        "item_id": original.id,
        "occurrence_id": str(original.occurrence_id),
        "step_id": STEP_ID,
        "completed": True,
        "if_match": original_etag,
        "idempotency_key": "complete-first-checklist-step",
    }
    for _ in range(2):
        result = await hass.services.async_call(
            DOMAIN,
            "set_checklist_step",
            first_step,
            context=Context(user_id=hass_user.id),
            blocking=True,
            return_response=True,
        )
        assert result is not None
        assert result["revision"] == original.revision + 1
        assert result["occurrence_id"] == str(original.occurrence_id)
    changed = await user.get(f"{PREFIX}/items/{original.id}")
    progress = MaintenanceItem.parse(await changed.json())
    assert progress.occurrence_id == original.occurrence_id
    assert progress.due == original.due
    assert progress.checklist[0].completion is not None
    assert progress.checklist[0].completion.actor.kind is ActorKind.USER
    assert progress.checklist[0].completion.actor.id == f"ha:{hass_user.id}"

    await hass.services.async_call(
        DOMAIN,
        "set_checklist_step",
        {
            "item_id": original.id,
            "occurrence_id": str(original.occurrence_id),
            "step_id": SECOND_STEP_ID,
            "completed": True,
        },
        blocking=True,
    )
    await entry.runtime_data.async_refresh()
    current = next(
        item for item in entry.runtime_data.data.items if item.id == original.id
    )
    assert current.occurrence_id == original.occurrence_id
    assert current.due == original.due
    assert current.checklist[1].completion is not None
    assert current.checklist[1].completion.actor.kind is ActorKind.SYSTEM
    assert current.checklist[1].completion.actor.id == "ha:automation"
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "1"
    assert entry.runtime_data.data.summary["completed"] == 0
    scheduled = await calendar_events(original.due)
    assert scheduled[0]["uid"] == str(original.occurrence_id)
    description = scheduled[0]["description"]
    assert isinstance(description, str)
    assert "Checklist (2/2 complete)" in description

    await hass.services.async_call(
        "todo",
        "update_item",
        {
            "entity_id": "todo.maintenance",
            "item": str(original.occurrence_id),
            "status": "completed",
        },
        context=Context(user_id=hass_user.id),
        blocking=True,
    )
    response = await user.get(f"{PREFIX}/items/{original.id}")
    successor = MaintenanceItem.parse(await response.json())
    assert successor.due is not None
    assert successor.occurrence_id != original.occurrence_id
    assert successor.due != original.due
    assert tuple(step.id for step in successor.checklist) == tuple(
        step.id for step in original.checklist
    )
    assert all(step.completion is None for step in successor.checklist)
    assert await calendar_events(original.due) == []
    scheduled = await calendar_events(successor.due)
    assert len(scheduled) == 1
    assert scheduled[0]["uid"] == str(successor.occurrence_id)
    assert scheduled[0]["start"] == {"date": successor.due.isoformat()}
    assert scheduled[0]["end"] == {
        "date": (successor.due + timedelta(days=1)).isoformat()
    }
    description = scheduled[0]["description"]
    assert isinstance(description, str)
    assert "Checklist (0/2 complete)" in description
    events_response = await user.get(f"{PREFIX}/items/{original.id}/events")
    events = await events_response.json()
    assert len(events["items"]) == 1
    snapshot = tuple(
        ChecklistStep.parse(step) for step in events["items"][0]["checklist"]
    )
    assert snapshot == current.checklist

    with pytest.raises(HomeAssistantError, match="task changed"):
        await hass.services.async_call(
            DOMAIN,
            "set_checklist_step",
            {
                "item_id": original.id,
                "occurrence_id": str(original.occurrence_id),
                "step_id": STEP_ID,
                "completed": True,
            },
            blocking=True,
        )
    after_stale = await user.get(f"{PREFIX}/items/{original.id}")
    assert MaintenanceItem.parse(await after_stale.json()) == successor


async def test_migrated_undated_completed_and_standing_tasks_remain_faithful_in_ha(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    """Imported source facts survive native todo actions without made-up dates."""
    source = {
        "system": "home_assistant",
        "collection": "todo.example",
        "id": "Original Case-Sensitive UID",
    }
    event_source = {**source, "id": "Original Completion UID"}
    payload = {
        "items": [
            {
                "source": source,
                "item": {
                    "id": "finished-example-task",
                    "name": "Finished example task",
                    "summary": "Finished example task",
                    "timezone": "UTC",
                    "due": None,
                    "schedule": {"kind": "once"},
                },
                "status": "completed",
                "history": [
                    {
                        "source": event_source,
                        "completed_at": "2025-02-03T04:05:06Z",
                        "actor": None,
                        "previous_due": None,
                        "next_due": None,
                    }
                ],
            },
            {
                "source": {**source, "id": "One-off UID"},
                "item": {
                    "id": "undated-example-task",
                    "name": "Undated example task",
                    "summary": "Undated example task",
                    "timezone": "UTC",
                    "due": None,
                    "schedule": {"kind": "once"},
                },
            },
            {
                "source": {**source, "id": "Standing UID"},
                "item": {
                    "id": "standing-example-task",
                    "name": "Standing example task",
                    "summary": "Standing example task",
                    "timezone": "UTC",
                    "due": None,
                    "schedule": {"kind": "standing"},
                },
            },
            {
                "source": {**source, "id": "Dated UID"},
                "item": {
                    "id": "dated-example-task",
                    "name": "Dated example task",
                    "summary": "Dated example task",
                    "timezone": "UTC",
                    "due": "2025-02-05",
                    "schedule": {"kind": "once"},
                },
            },
        ]
    }
    session = async_get_clientsession(hass)
    owner = {"Authorization": f"Bearer {rust_backend.owner_token}"}
    async with session.post(
        f"{rust_backend.origin}/v1/migrations/preview",
        json=payload,
        headers=owner,
    ) as response:
        assert response.status == 200
        report = await response.json()
    assert report["applicable"] is True
    assert [mapping["source"]["id"] for mapping in report["items"]] == [
        "Original Case-Sensitive UID",
        "One-off UID",
        "Standing UID",
        "Dated UID",
    ]
    async with session.get(
        f"{rust_backend.origin}/v1/items", headers=owner
    ) as response:
        assert (await response.json())["items"] == []
    async with session.post(
        f"{rust_backend.origin}/v1/migrations",
        json=payload,
        headers={**owner, "Idempotency-Key": "import-example-source"},
    ) as response:
        assert response.status == 201
        assert await response.json() == report
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
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "3"
    todo = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    items = {item["summary"]: item for item in entity_items(todo, "todo.maintenance")}
    finished = items["Finished example task"]
    assert finished["status"] == "completed"
    assert finished["completed"] == "2025-02-03T04:05:06+00:00"
    assert "due" not in finished
    for name in ("Undated example task", "Standing example task"):
        assert items[name]["status"] == "needs_action"
        assert "due" not in items[name]
    assert items["Dated example task"]["due"] == "2025-02-05"
    user = await hass_client(hass_user_token)
    response = await user.get(
        "/api/calendars/calendar.maintenance",
        params={"start": "2025-02-01T00:00:00Z", "end": "2025-02-10T00:00:00Z"},
    )
    assert response.status == 200
    events = await response.json()
    assert len(events) == 1
    assert events[0]["summary"] == "Dated example task"
    assert events[0]["start"] == {"date": "2025-02-05"}
    original = {
        item.id: item
        for item in entry.runtime_data.data.items
        if item.status is ItemStatus.NEEDS_ACTION
    }
    for item_id in ("undated-example-task", "standing-example-task"):
        item = original[item_id]
        result = await hass.services.async_call(
            DOMAIN,
            "complete",
            {
                "item_id": item.id,
                "occurrence_id": str(item.occurrence_id),
                "if_match": f'"{item.revision}"',
                "idempotency_key": f"complete-{item.id}",
            },
            context=Context(user_id=hass_user.id),
            blocking=True,
            return_response=True,
        )
        assert result is not None
        assert result["due"] is None
        assert (result["occurrence_id"] == str(item.occurrence_id)) == (
            item_id == "undated-example-task"
        )
    # Several writes share HA's refresh debouncer; poll the durable snapshot.
    await entry.runtime_data.async_refresh()
    current = {item.id: item for item in entry.runtime_data.data.items}
    assert current["undated-example-task"].status is ItemStatus.COMPLETED
    assert current["undated-example-task"].due is None
    assert current["standing-example-task"].status is ItemStatus.NEEDS_ACTION
    assert current["standing-example-task"].due is None
    once = original["undated-example-task"]
    replay = await hass.services.async_call(
        DOMAIN,
        "complete",
        {
            "item_id": once.id,
            "occurrence_id": str(once.occurrence_id),
            "if_match": f'"{once.revision}"',
            "idempotency_key": f"complete-{once.id}",
        },
        context=Context(user_id=hass_user.id),
        blocking=True,
        return_response=True,
    )
    assert replay is not None
    assert replay["revision"] == current[once.id].revision
    assert replay["occurrence_id"] == str(once.occurrence_id)
    assert replay["due"] is None
    with pytest.raises(HomeAssistantError, match="task changed"):
        await hass.services.async_call(
            DOMAIN,
            "complete",
            {"item_id": once.id, "occurrence_id": str(once.occurrence_id)},
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    response = await user.get(f"{PREFIX}/items/finished-example-task/events")
    assert response.status == 200
    history = await response.json()
    assert len(history["items"]) == 1
    imported = history["items"][0]
    assert imported["source"] == event_source
    assert imported["completed_at"] == "2025-02-03T04:05:06Z"
    assert imported["actor"] is None
    assert imported["occurrence_id"] is None
    assert imported["previous_due"] is None
    assert imported["next_due"] is None
    response = await user.get(f"{PREFIX}/items/finished-example-task")
    item = await response.json()
    assert item["source"] == source
    assert item["status"] == "completed"
    assert item["last_completion"] == imported
    todo = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    items = {item["summary"]: item for item in entity_items(todo, "todo.maintenance")}
    assert items["Undated example task"]["status"] == "completed"
    assert items["Undated example task"]["uid"] == str(
        original["undated-example-task"].occurrence_id
    )
    assert items["Standing example task"]["status"] == "needs_action"
    assert items["Standing example task"]["uid"] != str(
        original["standing-example-task"].occurrence_id
    )
    assert "due" not in items["Undated example task"]
    assert "due" not in items["Standing example task"]


async def test_instructions_stay_read_only_and_only_verified_owner_can_archive(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_owner_user: MockUser,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    """Actual HA identity and the Rust API preserve a household reference."""
    session = async_get_clientsession(hass)
    async with session.post(
        f"{rust_backend.origin}/v1/items",
        headers={
            "Authorization": f"Bearer {rust_backend.owner_token}",
            "Idempotency-Key": "create-example-instruction",
        },
        json={
            "id": "example-instruction",
            "name": "Example reference",
            "summary": "Keep the example reference available",
            "description": "A persistent reference for household members.",
            "timezone": "UTC",
            "schedule": {"kind": "instruction"},
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
    admin = await hass_client()
    household = await hass_client(hass_user_token)
    token = await hass.auth.async_create_refresh_token(
        hass_owner_user, client_id="https://example.invalid"
    )
    owner = await hass_client(hass.auth.async_create_access_token(token))
    system = await (await owner.get(f"{PREFIX}/system")).json()
    assert system["can_manage_instructions"] is True
    for client in (admin, household):
        system = await (await client.get(f"{PREFIX}/system")).json()
        assert system["can_manage_instructions"] is False
        response = await client.patch(
            f"{PREFIX}/items/{original.id}",
            headers={"If-Match": '"1"', "X-Maintenance-Owner": "true"},
            json={"summary": "Changed by another user"},
        )
        assert response.status == 403
        response = await client.put(
            f"{PREFIX}/items/{original.id}/archive",
            headers={"If-Match": '"1"', "X-Maintenance-Owner": "true"},
        )
        assert response.status == 403
    for number, client in enumerate((owner, admin, household)):
        for suffix, method, body in (
            (
                "completions",
                client.post,
                {"occurrence_id": str(original.occurrence_id)},
            ),
            (
                f"checklist/{STEP_ID}",
                client.put,
                {"occurrence_id": str(original.occurrence_id), "completed": True},
            ),
            ("reorders", client.post, {}),
        ):
            response = await method(
                f"{PREFIX}/items/{original.id}/{suffix}",
                headers={
                    "If-Match": '"1"',
                    "Idempotency-Key": f"instruction-{number}-{suffix}",
                },
                json=body,
            )
            assert response.status == 409
    response = await household.get(f"{PREFIX}/items/{original.id}")
    assert MaintenanceItem.parse(await response.json()) == original
    events = await (await owner.get(f"{PREFIX}/items/{original.id}/events")).json()
    assert events["items"] == []
    tasks = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    assert entity_items(tasks, "todo.maintenance") == []
    references = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance_instructions"},
        blocking=True,
        return_response=True,
    )
    reference = entity_items(references, "todo.maintenance_instructions")
    assert len(reference) == 1
    assert reference[0]["summary"] == original.summary
    assert "status" not in reference[0]
    response = await owner.patch(
        f"{PREFIX}/items/{original.id}",
        headers={"If-Match": '"1"'},
        json={"summary": "Updated example reference"},
    )
    assert response.status == 200
    updated = MaintenanceItem.parse(await response.json())
    response = await owner.put(
        f"{PREFIX}/items/{original.id}/archive",
        headers={"If-Match": f'"{updated.revision}"'},
    )
    assert response.status == 200
    await entry.runtime_data.async_refresh()
    references = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance_instructions"},
        blocking=True,
        return_response=True,
    )
    assert entity_items(references, "todo.maintenance_instructions") == []
    response = await household.get(f"{PREFIX}/items/{original.id}")
    archived = MaintenanceItem.parse(await response.json())
    assert archived.archived is True
    response = await admin.delete(
        f"{PREFIX}/items/{original.id}/archive",
        headers={"If-Match": f'"{archived.revision}"'},
    )
    assert response.status == 403


async def test_recurring_writes_require_a_due_date_through_the_ha_bridge(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    """The server rejects missing dates even when callers skip browser defaults."""
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
    admin = await hass_client()
    schedule = {"kind": "recurring", "interval": {"days": 30, "months": 0, "years": 0}}
    definition = {
        "id": "example-recurring-task",
        "name": "Example recurring task",
        "summary": "Example recurring task",
        "timezone": "UTC",
        "schedule": schedule,
    }
    for number, due in enumerate(({}, {"due": None})):
        response = await admin.post(
            f"{PREFIX}/items",
            json={**definition, **due},
            headers={"Idempotency-Key": f"missing-date-{number}"},
        )
        assert response.status == 400
    response = await admin.post(
        f"{PREFIX}/items",
        json={**definition, "schedule": {"kind": "once"}},
        headers={"Idempotency-Key": "create-undated-once"},
    )
    assert response.status == 201
    original = MaintenanceItem.parse(await response.json())
    response = await admin.patch(
        f"{PREFIX}/items/{original.id}",
        json={"schedule": schedule},
        headers={"If-Match": f'"{original.revision}"'},
    )
    assert response.status == 400
    due = date.today().isoformat()
    response = await admin.patch(
        f"{PREFIX}/items/{original.id}",
        json={"schedule": schedule, "due": due},
        headers={"If-Match": f'"{original.revision}"'},
    )
    assert response.status == 200
    recurring = MaintenanceItem.parse(await response.json())
    assert recurring.due is not None
    assert recurring.due.isoformat() == due
    response = await admin.patch(
        f"{PREFIX}/items/{original.id}",
        json={"due": None},
        headers={"If-Match": f'"{recurring.revision}"'},
    )
    assert response.status == 400
    response = await admin.get(f"{PREFIX}/items/{original.id}")
    assert MaintenanceItem.parse(await response.json()) == recurring


async def test_real_registry_links_survive_shared_tasks_renames_and_deletions(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    registered_objects: RegisteredObjects,
    entity_registry: er.EntityRegistry,
    device_registry: dr.DeviceRegistry,
    rust_backend: RustBackend,
) -> None:
    """Stable associations round-trip through real SQLite and native HA actions."""
    identity = await instance_id.async_get(hass)
    references = [
        {"kind": "device", "instance_id": identity, "id": registered_objects.device_id},
        {
            "kind": "entity",
            "instance_id": identity,
            "id": registered_objects.registry_id,
        },
    ]
    originals: list[MaintenanceItem] = []
    for item_id in ("first-linked-task", "second-linked-task"):
        async with async_get_clientsession(hass).post(
            f"{rust_backend.origin}/v1/items",
            headers={
                "Authorization": f"Bearer {rust_backend.owner_token}",
                "Idempotency-Key": f"create-{item_id}",
            },
            json={
                "id": item_id,
                "name": item_id,
                "summary": item_id,
                "timezone": "UTC",
                "due": "2026-10-15",
                "schedule": {"kind": "once"},
                "home_assistant": references,
            },
        ) as response:
            assert response.status == 201
            body = await response.json()
            assert body["home_assistant"] == references
            originals.append(MaintenanceItem.parse(body))
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    admin = await hass_client()
    household = await hass_client(hass_user_token)
    response = await household.get(f"{PREFIX}/home-assistant/registry")
    assert response.status == 403
    response = await admin.get(f"{PREFIX}/home-assistant/registry")
    assert response.status == 200
    catalog = await response.json()
    assert catalog["status"] == "ready"
    assert catalog["instance_id"] == identity
    entity = next(
        target
        for target in catalog["targets"]
        if target["id"] == registered_objects.registry_id
    )
    assert entity["entity_id"] == registered_objects.entity_id
    todo = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    tasks = entity_items(todo, "todo.maintenance")
    assert len(tasks) == 2
    assert all(
        f"/config/devices/device/{registered_objects.device_id}"
        in str(item["description"])
        for item in tasks
    )

    await hass.services.async_call(
        "todo",
        "update_item",
        {
            "entity_id": "todo.maintenance",
            "item": str(originals[0].occurrence_id),
            "status": "completed",
        },
        blocking=True,
    )
    response = await admin.get(f"{PREFIX}/items/{originals[0].id}")
    completed = await response.json()
    assert completed["status"] == "completed"
    assert completed["home_assistant"] == references
    entity_registry.async_update_entity(
        registered_objects.entity_id,
        new_entity_id="sensor.renamed_example",
        name="Renamed example sensor",
    )
    await asyncio.sleep(0.55)
    await hass.async_block_till_done(wait_background_tasks=True)
    catalog = await (await admin.get(f"{PREFIX}/home-assistant/registry")).json()
    entity = next(
        target
        for target in catalog["targets"]
        if target["id"] == registered_objects.registry_id
    )
    assert entity["entity_id"] == "sensor.renamed_example"
    assert entity["name"] == "Renamed example sensor"
    response = await admin.get(f"{PREFIX}/items/{originals[1].id}")
    assert MaintenanceItem.parse(await response.json()) == originals[1]
    entity_registry.async_remove("sensor.renamed_example")
    device_registry.async_remove_device(registered_objects.device_id)
    await asyncio.sleep(0.55)
    await hass.async_block_till_done(wait_background_tasks=True)
    catalog = await (await admin.get(f"{PREFIX}/home-assistant/registry")).json()
    assert all(
        target["id"]
        not in {registered_objects.registry_id, registered_objects.device_id}
        for target in catalog["targets"]
    )
    response = await admin.get(f"{PREFIX}/items/{originals[1].id}")
    assert MaintenanceItem.parse(await response.json()) == originals[1]
    todo = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    tasks = entity_items(todo, "todo.maintenance")
    assert all(
        str(item["description"]).count("removed from Home Assistant") == 2
        for item in tasks
    )


async def test_large_real_registry_catalog_round_trips_through_ha_bridge(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    """A valid catalog larger than ordinary API responses still reaches the picker."""
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    targets = [
        {
            "kind": "device",
            "id": f"example-device-{number}",
            "name": "Example device " + "x" * 180,
            "disabled": False,
        }
        for number in range(5_000)
    ]
    async with async_get_clientsession(hass).put(
        f"{rust_backend.origin}/v1/integration/registry",
        json={
            "status": "ready",
            "instance_id": await instance_id.async_get(hass),
            "base_url": None,
            "targets": targets,
        },
        headers={
            "Authorization": f"Bearer {rust_backend.integration_token}",
            "X-Maintenance-System-Actor": "ha:integration",
        },
    ) as response:
        assert response.status == 204
    admin = await hass_client()
    response = await admin.get(f"{PREFIX}/home-assistant/registry")
    assert response.status == 200
    assert len(await response.read()) > MAX_RESPONSE_BYTES
    catalog = await response.json()
    assert catalog["status"] == "ready"
    assert catalog["targets"] == targets


async def test_link_heavy_inventory_keeps_all_items_across_bounded_pages(
    hass: HomeAssistant, rust_backend: RustBackend
) -> None:
    """Adaptive pages preserve a cursor when later tasks have many shared links."""
    identity = await instance_id.async_get(hass)
    references = [
        {"kind": "device", "instance_id": identity, "id": f"{number + 1:032x}"}
        for number in range(100)
    ]
    definitions: list[dict[str, object]] = [
        {
            "id": f"first-small-task-{number:03}",
            "name": "Example task",
            "summary": "Example task",
            "timezone": "UTC",
            "schedule": {"kind": "once"},
        }
        for number in range(100)
    ]
    linked_ids = {f"second-linked-task-{number:03}" for number in range(120)}
    definitions.extend(
        {
            "id": item_id,
            "name": "Example linked task",
            "summary": "Example linked task",
            "timezone": "UTC",
            "schedule": {"kind": "once"},
            "home_assistant": references,
        }
        for item_id in sorted(linked_ids)
    )
    session = async_get_clientsession(hass)
    owner = {"Authorization": f"Bearer {rust_backend.owner_token}"}
    for start in range(0, len(definitions), 15):
        async with session.post(
            f"{rust_backend.origin}/v1/imports",
            json={"items": definitions[start : start + 15]},
            headers=owner,
        ) as response:
            assert response.status == 200
    # The first bounded retry can read the 100 small tasks. The next 100 linked
    # tasks still exceed the same cap, so a later retry must keep its cursor.
    async with session.get(
        f"{rust_backend.origin}/v1/items?limit=200", headers=owner
    ) as response:
        assert response.status == 200
        assert len(await response.read()) > MAX_RESPONSE_BYTES
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    snapshot = entry.runtime_data.data
    assert len(snapshot.items) == 220
    assert len({item.occurrence_id for item in snapshot.items}) == 220
    assert {item.id for item in snapshot.items} == {
        definition["id"] for definition in definitions
    }
    linked = [item for item in snapshot.items if item.id in linked_ids]
    assert len(linked) == 120
    assert all(len(item.home_assistant) == 100 for item in linked)
    assert all(item.home_assistant == linked[0].home_assistant for item in linked)
    state = hass.states.get("todo.maintenance")
    assert state is not None
    assert state.state == "220"


async def test_native_card_uses_stable_task_ids_and_refreshes_real_status(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    due = datetime.now(UTC).date().isoformat()
    async with async_get_clientsession(hass).post(
        f"{rust_backend.origin}/v1/items",
        headers={
            "Authorization": f"Bearer {rust_backend.owner_token}",
            "Idempotency-Key": "create-card-example",
        },
        json={
            "id": "example-card-task",
            "name": "Example equipment",
            "summary": "Check example equipment",
            "timezone": "UTC",
            "due": due,
            "schedule": {"kind": "once"},
        },
    ) as response:
        assert response.status == 201
        original = MaintenanceItem.parse(await response.json())
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
            CONF_ADDON_SLUG: "example_home_maintenance",
        },
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    admin = await hass_client()
    response = await admin.get("/api/home_maintenance/card")
    assert response.status == 200
    card = await response.json()
    assert card == {
        "app_path": "/app/example_home_maintenance/",
        "can_open": True,
        "total": 1,
        "items": [
            {
                "id": original.id,
                "name": original.name,
                "summary": original.summary,
                "status": "needs_action",
                "due": due,
                "timezone": "UTC",
            }
        ],
    }
    assert card["items"][0]["id"] != str(original.occurrence_id)
    response = await admin.post(
        f"{PREFIX}/items/{original.id}/completions",
        headers={
            "If-Match": f'"{original.revision}"',
            "Idempotency-Key": "complete-card-example",
        },
        json={"occurrence_id": str(original.occurrence_id)},
    )
    assert response.status == 200
    response = await admin.get("/api/home_maintenance/card")
    assert response.status == 200
    assert (await response.json())["total"] == 0
    response = await admin.get("/api/home_maintenance/card?view=completed")
    assert response.status == 200
    completed = await response.json()
    assert completed["total"] == 1
    assert completed["items"][0]["id"] == original.id
    assert completed["items"][0]["status"] == "completed"
