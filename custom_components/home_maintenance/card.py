"""Authenticated task projection for the native Maintenance dashboard card."""

import re
from datetime import date
from enum import StrEnum
from typing import TypedDict

from aiohttp import web
from homeassistant.core import HomeAssistant
from homeassistant.helpers.http import HomeAssistantView
from homeassistant.util import dt as dt_util

from .const import CONF_ADDON_SLUG
from .http import loaded_coordinator
from .models import ItemStatus, ScheduleKind

_APP_SLUG = re.compile(r"[a-z0-9_-]{1,128}")


class CardView(StrEnum):
    ACTIVE = "active"
    UPCOMING = "upcoming"
    OVERDUE = "overdue"
    COMPLETED = "completed"


class CardItem(TypedDict):
    id: str
    name: str
    summary: str
    status: ItemStatus
    due: str | None
    timezone: str


class CardResponse(TypedDict):
    app_path: str | None
    can_open: bool
    items: list[CardItem]
    total: int


def _bounded_number(value: str, maximum: int) -> int:
    if not value.isascii() or not value.isdigit():
        raise web.HTTPBadRequest(reason="Invalid card query")
    number = int(value)
    if not 1 <= number <= maximum:
        raise web.HTTPBadRequest(reason="Invalid card query")
    return number


class MaintenanceCardView(HomeAssistantView):
    """Read current task status without exposing credentials or mutable native todos."""

    url = "/api/home_maintenance/card"
    name = "api:home_maintenance:card"
    requires_auth = True

    def __init__(self, hass: HomeAssistant) -> None:
        self._hass = hass

    async def get(self, request: web.Request) -> web.Response:
        user = request["hass_user"]
        if not user.is_active:
            raise web.HTTPForbidden
        if (
            len(request.query_string) > 512
            or request.query.keys() - {"view", "days", "limit"}
            or any(len(request.query.getall(key)) != 1 for key in request.query)
        ):
            raise web.HTTPBadRequest(reason="Unsupported card query")
        try:
            view = CardView(request.query.get("view", "upcoming"))
        except ValueError:
            raise web.HTTPBadRequest(reason="Invalid card view") from None
        days = _bounded_number(request.query.get("days", "14"), 365)
        limit = _bounded_number(request.query.get("limit", "20"), 100)
        coordinator = loaded_coordinator(self._hass)
        if coordinator is None:
            raise web.HTTPServiceUnavailable(reason="Maintenance is not loaded")
        await coordinator.async_refresh()
        if (
            loaded_coordinator(self._hass) is not coordinator
            or not coordinator.last_update_success
        ):
            raise web.HTTPServiceUnavailable(reason="Maintenance is unavailable")
        now = dt_util.utcnow()
        matches = []
        for item in coordinator.data.items:
            if item.archived or item.schedule_kind is ScheduleKind.INSTRUCTION:
                continue
            if (item.status is ItemStatus.COMPLETED) != (view is CardView.COMPLETED):
                continue
            if view in {CardView.UPCOMING, CardView.OVERDUE}:
                if item.due is None:
                    continue
                days_until_due = (item.due - now.astimezone(item.timezone).date()).days
                if view is CardView.UPCOMING and not 0 <= days_until_due <= days:
                    continue
                if view is CardView.OVERDUE and days_until_due >= 0:
                    continue
            matches.append(item)
        matches.sort(
            key=lambda item: (
                item.due is None,
                item.due or date.max,
                item.name.casefold(),
                item.id,
            )
        )
        rows: list[CardItem] = [
            {
                "id": item.id,
                "name": item.name,
                "summary": item.summary,
                "status": item.status,
                "due": item.due.isoformat() if item.due is not None else None,
                "timezone": item.timezone.key,
            }
            for item in matches[:limit]
        ]
        entry = coordinator.config_entry
        slug = entry.data.get(CONF_ADDON_SLUG) if entry is not None else None
        app_path = (
            f"/app/{slug}/"
            if isinstance(slug, str) and _APP_SLUG.fullmatch(slug)
            else None
        )
        report: CardResponse = {
            "app_path": app_path,
            "can_open": user.is_admin and app_path is not None,
            "items": rows,
            "total": len(matches),
        }
        return web.json_response(report, headers={"Cache-Control": "no-store"})
