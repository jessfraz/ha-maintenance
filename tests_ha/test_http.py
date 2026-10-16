"""Exercise authentication, authorization and allowlisting through HA HTTP."""

import json

import pytest
from homeassistant.core import HomeAssistant
from pytest_homeassistant_custom_component.common import MockUser
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from custom_components.home_maintenance import MaintenanceConfigEntry
from custom_components.home_maintenance.const import (
    MAX_REQUEST_BYTES,
    MAX_RESPONSE_BYTES,
)

from .conftest import ITEM_ID, OCCURRENCE_ID, Backend

PREFIX = "/api/home_maintenance/v1"
REORDER_ID = "0193c69f-e5c4-7e93-b6cb-8172dacb6182"


async def test_authentication_and_human_attribution(
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    hass_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    anonymous = await hass_client(None)
    assert (await anonymous.get(f"{PREFIX}/summary")).status == 401
    client = await hass_client(hass_user_token)
    response = await client.post(
        f"{PREFIX}/items/{ITEM_ID}/completions",
        json={"occurrence_id": OCCURRENCE_ID},
        headers={
            "If-Match": '"1"',
            "Idempotency-Key": "complete-one",
            "X-Maintenance-Actor-Id": "forged-user",
            "X-Maintenance-Actor-Name": "Forged Name",
            "X-Maintenance-Admin": "true",
            "X-Maintenance-System-Actor": "forged-system",
            "Authorization": f"Bearer {hass_user_token}",
        },
    )
    assert response.status == 200
    forwarded = next(
        request for request in backend.requests if request.path.endswith("/completions")
    )
    assert forwarded.headers["Authorization"] == "Bearer test-integration-token"
    assert forwarded.headers["X-Maintenance-Actor-Id"] == f"ha:{hass_user.id}"
    assert forwarded.headers["X-Maintenance-Actor-Name"] == hass_user.name
    assert "X-Maintenance-Admin" not in forwarded.headers
    assert "X-Maintenance-System-Actor" not in forwarded.headers
    assert forwarded.headers["If-Match"] == '"1"'
    assert forwarded.headers["Idempotency-Key"] == "complete-one"
    assert json.loads(forwarded.body) == {"occurrence_id": OCCURRENCE_ID}


@pytest.mark.parametrize(
    "method,path",
    [
        ("post", "items"),
        ("patch", f"items/{ITEM_ID}"),
        ("put", f"items/{ITEM_ID}/archive"),
        ("delete", f"items/{ITEM_ID}/archive"),
        ("put", f"reorders/{REORDER_ID}/resolution"),
    ],
)
async def test_only_admin_can_change_definitions_or_resolve_reorders(
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    hass_admin_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    method: str,
    path: str,
) -> None:
    user = await hass_client(hass_user_token)
    before = len(backend.requests)
    assert (await getattr(user, method)(f"{PREFIX}/{path}", json={})).status == 403
    assert len(backend.requests) == before
    admin = await hass_client()
    assert (await getattr(admin, method)(f"{PREFIX}/{path}", json={})).status == 200
    forwarded = next(
        request
        for request in reversed(backend.requests)
        if request.method == method.upper()
    )
    assert forwarded.headers["X-Maintenance-Admin"] == "true"
    assert forwarded.headers["X-Maintenance-Actor-Id"] == f"ha:{hass_admin_user.id}"


@pytest.mark.parametrize(
    "method,path,status",
    [
        ("get", "credentials", 404),
        ("post", "integration/presence", 404),
        ("get", "installation", 404),
        ("get", "items/invalid_id", 404),
        ("delete", f"items/{ITEM_ID}", 405),
        ("post", f"items/{ITEM_ID}/events", 405),
        ("get", "items?url=http://elsewhere.invalid", 400),
        ("get", "summary?archived=true", 400),
        ("get", f"items/{ITEM_ID}?limit=20", 400),
        ("get", f"items/{ITEM_ID}/reorders", 405),
        ("post", "reorders", 405),
        ("delete", f"reorders/{REORDER_ID}", 405),
        ("get", "reorders/not-a-uuid", 404),
        ("get", "reorders?url=http://elsewhere.invalid", 400),
        ("get", f"reorders/{REORDER_ID}?status=pending", 400),
        ("put", f"reorders/{REORDER_ID}/resolution?status=fulfilled", 400),
    ],
)
async def test_route_and_query_allowlist(
    hass_client: ClientSessionGenerator,
    entry: MaintenanceConfigEntry,
    backend: Backend,
    method: str,
    path: str,
    status: int,
) -> None:
    client = await hass_client()
    before = len(backend.requests)
    response = await getattr(client, method)(f"{PREFIX}/{path}")
    assert response.status == status
    assert len(backend.requests) == before


async def test_query_response_headers_and_unload(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    backend.responses[("GET", f"/v1/items/{ITEM_ID}")] = (
        412,
        b'{"message":"changed"}',
        {
            "ETag": '"4"',
            "Retry-After": "5",
            "Set-Cookie": "private=value",
            "Content-Type": "application/json",
        },
    )
    client = await hass_client()
    response = await client.get(f"{PREFIX}/items/{ITEM_ID}")
    assert response.status == 412
    assert response.headers["ETag"] == '"4"'
    assert response.headers["Retry-After"] == "5"
    assert "Set-Cookie" not in response.headers
    assert await response.json() == {"message": "changed"}
    response = await client.get(
        f"{PREFIX}/items?limit=2&page_token=a%2Bb&archived=true"
    )
    assert response.status == 200
    assert backend.requests[-1].query == {
        "limit": "2",
        "page_token": "a+b",
        "archived": "true",
    }
    assert await hass.config_entries.async_unload(entry.entry_id)
    count = len(backend.requests)
    assert (await client.get(f"{PREFIX}/items")).status == 503
    assert len(backend.requests) == count


async def test_reorder_attribution_receipt_and_list_query(
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    hass_user: MockUser,
    entry: MaintenanceConfigEntry,
    backend: Backend,
) -> None:
    receipt = {"id": REORDER_ID}
    backend.responses[("POST", f"/v1/items/{ITEM_ID}/reorders")] = (
        201,
        json.dumps(receipt).encode(),
        {"Content-Type": "application/json", "Cache-Control": "no-store"},
    )
    client = await hass_client(hass_user_token)
    response = await client.post(
        f"{PREFIX}/items/{ITEM_ID}/reorders",
        json={},
        headers={
            "If-Match": '"1"',
            "Idempotency-Key": "press-one",
            "X-Maintenance-Actor-Id": "forged-user",
            "X-Maintenance-Admin": "true",
            "X-Maintenance-System-Actor": "ha:integration",
        },
    )
    assert response.status == 201
    assert response.headers["Cache-Control"] == "no-store"
    assert await response.json() == receipt
    forwarded = backend.requests[-1]
    assert forwarded.path == f"/v1/items/{ITEM_ID}/reorders"
    assert forwarded.headers["X-Maintenance-Actor-Id"] == f"ha:{hass_user.id}"
    assert "X-Maintenance-Admin" not in forwarded.headers
    assert "X-Maintenance-System-Actor" not in forwarded.headers
    assert forwarded.headers["If-Match"] == '"1"'
    assert forwarded.headers["Idempotency-Key"] == "press-one"
    assert json.loads(forwarded.body) == {}

    response = await client.get(
        f"{PREFIX}/reorders?status=dismissed&limit=2&page_token=a%2Bb"
    )
    assert response.status == 200
    assert backend.requests[-1].query == {
        "status": "dismissed",
        "limit": "2",
        "page_token": "a+b",
    }
    response = await client.get(f"{PREFIX}/reorders/{REORDER_ID}")
    assert response.status == 200
    assert backend.requests[-1].path == f"/v1/reorders/{REORDER_ID}"


async def test_body_and_response_limits(
    hass_client: ClientSessionGenerator, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    client = await hass_client()
    response = await client.post(f"{PREFIX}/items", data=b"x" * (MAX_REQUEST_BYTES + 1))
    assert response.status == 413
    backend.responses[("GET", f"/v1/items/{ITEM_ID}")] = (
        200,
        b"x" * (MAX_RESPONSE_BYTES + 1),
        {},
    )
    assert (await client.get(f"{PREFIX}/items/{ITEM_ID}")).status == 502


async def test_redirect_is_not_followed(
    hass_client: ClientSessionGenerator, entry: MaintenanceConfigEntry, backend: Backend
) -> None:
    backend.responses[("GET", f"/v1/items/{ITEM_ID}")] = (
        302,
        b"",
        {"Location": "https://elsewhere.invalid"},
    )
    client = await hass_client()
    response = await client.get(f"{PREFIX}/items/{ITEM_ID}", allow_redirects=False)
    assert response.status == 302
    assert "Location" not in response.headers
