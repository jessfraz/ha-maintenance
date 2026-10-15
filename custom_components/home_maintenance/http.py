"""Authenticated, fixed-destination REST bridge for Home Assistant clients."""

import re
from urllib.parse import urlencode

from aiohttp import ClientError, web
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers.http import HomeAssistantView

from .client import Actor, ResponseTooLarge
from .const import DOMAIN, MAX_REQUEST_BYTES
from .coordinator import MaintenanceCoordinator
from .models import ITEM_ID

_UUID = r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
_CHECKLIST_ROUTE = re.compile(rf"items/{ITEM_ID.pattern}/checklist/{_UUID}")
_ROUTES = (
    (re.compile(r"items"), frozenset({"GET", "POST"})),
    (re.compile(r"summary|system"), frozenset({"GET"})),
    (re.compile(rf"items/{ITEM_ID.pattern}"), frozenset({"GET", "PATCH"})),
    (re.compile(rf"items/{ITEM_ID.pattern}/events"), frozenset({"GET"})),
    (re.compile(rf"items/{ITEM_ID.pattern}/completions"), frozenset({"POST"})),
    (_CHECKLIST_ROUTE, frozenset({"PUT"})),
    (
        re.compile(rf"items/{ITEM_ID.pattern}/archive"),
        frozenset({"PUT", "DELETE"}),
    ),
)
_LIST_QUERY = {"archived", "limit", "page_token"}
_EVENT_QUERY = {"limit", "page_token"}


def loaded_coordinator(hass: HomeAssistant) -> MaintenanceCoordinator | None:
    """Return the sole loaded backend without retaining an unloaded entry."""
    for entry in hass.config_entries.async_entries(DOMAIN):
        if entry.state is ConfigEntryState.LOADED:
            return entry.runtime_data
    return None


class MaintenanceView(HomeAssistantView):
    """Expose only the service's documented routes through HA authentication."""

    url = "/api/home_maintenance/v1/{path:.*}"
    name = "api:home_maintenance:v1"
    requires_auth = True

    def __init__(self, hass: HomeAssistant) -> None:
        self._hass = hass

    async def _proxy(self, request: web.Request, path: str) -> web.Response:
        route = next((route for route in _ROUTES if route[0].fullmatch(path)), None)
        if route is None:
            raise web.HTTPNotFound
        if request.method not in route[1]:
            raise web.HTTPMethodNotAllowed(request.method, route[1])
        allowed_query: set[str] = set()
        if request.method == "GET":
            if path == "items":
                allowed_query = _LIST_QUERY
            elif path.endswith("/events"):
                allowed_query = _EVENT_QUERY
        if len(request.query_string) > 4096 or request.query.keys() - allowed_query:
            raise web.HTTPBadRequest(reason="Unsupported query parameters")
        user = request["hass_user"]
        admin_only = request.method != "GET" and not (
            path.endswith("/completions") or _CHECKLIST_ROUTE.fullmatch(path)
        )
        if admin_only and not user.is_admin:
            raise web.HTTPForbidden
        coordinator = loaded_coordinator(self._hass)
        if coordinator is None:
            raise web.HTTPServiceUnavailable
        if (request.content_length or 0) > MAX_REQUEST_BYTES:
            raise web.HTTPRequestEntityTooLarge(
                max_size=MAX_REQUEST_BYTES, actual_size=request.content_length
            )
        payload = bytearray()
        async for chunk in request.content.iter_chunked(16 * 1024):
            payload.extend(chunk)
            if len(payload) > MAX_REQUEST_BYTES:
                raise web.HTTPRequestEntityTooLarge(
                    max_size=MAX_REQUEST_BYTES, actual_size=len(payload)
                )
        headers = {
            key: request.headers[key]
            for key in ("If-Match", "If-None-Match", "Idempotency-Key")
            if key in request.headers
        }
        try:
            response = await coordinator.client.request(
                request.method,
                f"/v1/{path}"
                + (
                    f"?{urlencode(list(request.query.items()))}"
                    if request.query
                    else ""
                ),
                body=bytes(payload) or None,
                actor=Actor(user.id, user.name or "Home Assistant user", user.is_admin),
                headers=headers,
            )
        except TimeoutError:
            raise web.HTTPGatewayTimeout from None
        except ClientError, ResponseTooLarge:
            raise web.HTTPBadGateway from None
        if request.method != "GET" and response.status < 300:
            # Let polling reconcile native counts without delaying the REST response.
            entry = coordinator.config_entry
            if entry is not None:
                entry.async_create_background_task(
                    self._hass,
                    coordinator.async_request_refresh(),
                    "Maintenance refresh after action",
                )
        return web.Response(
            status=response.status, body=response.body, headers=response.headers
        )

    get = _proxy
    post = _proxy
    patch = _proxy
    put = _proxy
    delete = _proxy
