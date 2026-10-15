"""Cross-language contract through real HA and a compiled Rust service."""

import asyncio
import json
import os
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta
from pathlib import Path

import pytest
from aiohttp import ClientError, ClientTimeout
from homeassistant.config_entries import SOURCE_HASSIO, SOURCE_USER, ConfigEntryState
from homeassistant.core import Context, HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.service_info.hassio import HassioServiceInfo

from custom_components.home_maintenance.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
    SUMMARY_KEYS,
)
from custom_components.home_maintenance.models import (
    ActorKind,
    ChecklistStep,
    MaintenanceItem,
)

from .conftest import SECOND_STEP_ID, STEP_ID

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
    socket_enabled,
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
    hass_client,
    hass_user,
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
        "schedule": {"days": 1, "months": 0, "years": 0},
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
    assert hass.states.get("todo.maintenance").state == "1"
    assert hass.states.get("sensor.maintenance_overdue").state == "1"

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
    assert todo["todo.maintenance"]["items"][0]["uid"] == second["occurrence"]["id"]
    assert hass.states.get("sensor.maintenance_completed").state == "1"

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
    assert hass.states.get("todo.maintenance").state == "0"
    assert hass.states.get("sensor.maintenance_completed").state == "2"


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
        assert hass.states.get(f"sensor.maintenance_{key}").state == "0"
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
    hass_client,
    hass_user,
    hass_user_token,
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

    async def calendar_events(due: date) -> list[dict[str, object]]:
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
            "schedule": {"days": 1, "months": 0, "years": 0},
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
    await entry.runtime_data.async_refresh()
    todo = await hass.services.async_call(
        "todo",
        "get_items",
        {"entity_id": "todo.maintenance"},
        blocking=True,
        return_response=True,
    )
    assert len(todo["todo.maintenance"]["items"]) == 1
    assert todo["todo.maintenance"]["items"][0]["summary"] == original.summary
    assert (
        "Checklist (0/2 complete)"
        in todo["todo.maintenance"]["items"][0]["description"]
    )
    scheduled = await calendar_events(original.due)
    assert len(scheduled) == 1
    assert scheduled[0]["uid"] == str(original.occurrence_id)
    assert scheduled[0]["start"] == {"date": original.due.isoformat()}
    assert scheduled[0]["summary"] == original.summary
    assert "Checklist (0/2 complete)" in scheduled[0]["description"]

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
    assert hass.states.get("todo.maintenance").state == "1"
    assert entry.runtime_data.data.summary["completed"] == 0
    scheduled = await calendar_events(original.due)
    assert scheduled[0]["uid"] == str(original.occurrence_id)
    assert "Checklist (2/2 complete)" in scheduled[0]["description"]

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
    assert "Checklist (0/2 complete)" in scheduled[0]["description"]
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
