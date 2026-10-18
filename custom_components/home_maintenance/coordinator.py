"""Coordinate maintenance entities and authenticated installation check-ins."""

import asyncio
import json
import logging
from collections.abc import Mapping
from datetime import timedelta
from typing import Literal, NotRequired, TypedDict
from urllib.parse import urlencode
from uuid import UUID

from aiohttp import ClientError
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.const import EVENT_CORE_CONFIG_UPDATE
from homeassistant.core import Event, HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryAuthFailed, HomeAssistantError
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import instance_id
from homeassistant.helpers.debounce import Debouncer
from homeassistant.helpers.hassio import is_hassio
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .client import Actor, MaintenanceClient, ResponseTooLarge
from .const import CONF_ADDON_SLUG, DOMAIN, SUMMARY_KEYS
from .models import MaintenanceData, MaintenanceItem
from .registry import describe_references, registry_payload, unavailable_payload

_LOGGER = logging.getLogger(__name__)
_BACKUP_TIMEOUT_SECONDS = 10


class BackupReport(TypedDict):
    """Only backup coverage and date leave Home Assistant."""

    status: Literal[
        "complete", "app_only", "missing", "outdated", "unavailable", "standalone"
    ]
    last_backup: int | None


class PresenceReport(TypedDict):
    """A loaded integration's minimal authenticated installation evidence."""

    version: str
    backup: BackupReport
    owner_id: NotRequired[str]


class MaintenanceCoordinator(DataUpdateCoordinator[MaintenanceData]):
    """Poll the summary and complete item inventory for native entities."""

    def __init__(
        self,
        hass: HomeAssistant,
        entry: ConfigEntry,
        client: MaintenanceClient,
        version: str,
    ) -> None:
        super().__init__(
            hass,
            _LOGGER,
            name=DOMAIN,
            config_entry=entry,
            update_interval=timedelta(seconds=60),
            always_update=False,
        )
        self.client = client
        self._entry = entry
        self._version = version
        self._instance_id: str | None = None
        self._registry_lock = asyncio.Lock()
        self._registry_debouncer = Debouncer(
            hass,
            _LOGGER,
            cooldown=0.5,
            immediate=False,
            function=self._async_registry_changed,
        )

    @callback
    def async_listen_to_registries(self) -> None:
        """Own registry listeners and their debounce timer for this entry's lifetime."""
        self._entry.async_on_unload(
            self.hass.bus.async_listen(
                er.EVENT_ENTITY_REGISTRY_UPDATED, self._registry_updated
            )
        )
        self._entry.async_on_unload(
            self.hass.bus.async_listen(
                dr.EVENT_DEVICE_REGISTRY_UPDATED, self._registry_updated
            )
        )
        self._entry.async_on_unload(
            self.hass.bus.async_listen(EVENT_CORE_CONFIG_UPDATE, self._registry_updated)
        )
        self._entry.async_on_unload(self._registry_debouncer.async_shutdown)

    @callback
    def _registry_updated[T: Mapping[str, object]](self, event: Event[T]) -> None:
        if self._entry.state is ConfigEntryState.LOADED:
            self._registry_debouncer.async_schedule_call()

    @callback
    def _async_registry_changed(self) -> None:
        if self._entry.state is not ConfigEntryState.LOADED:
            return
        # Registry names are projected locally, even when backend items are unchanged.
        self.async_update_listeners()
        self._entry.async_create_background_task(
            self.hass, self.async_report_registry(), "Maintenance registry update"
        )

    def item_description(self, item: MaintenanceItem) -> str | None:
        """Use the same description for native display and native completion checks."""
        links = describe_references(self.hass, self._instance_id, item.home_assistant)
        return (
            "\n\n".join(
                section for section in (item.task_description, links) if section
            )
            or None
        )

    async def _async_update_data(self) -> MaintenanceData:
        try:
            response = await self.client.request("GET", "/v1/summary")
            if response.status in {401, 403}:
                raise ConfigEntryAuthFailed("The backend rejected its credential")
            if response.status != 200:
                raise UpdateFailed("The backend could not return its summary")
            data = response.json()
            if not isinstance(data, dict) or any(
                type(data.get(key)) is not int or data[key] < 0 for key in SUMMARY_KEYS
            ):
                raise UpdateFailed("The backend returned an invalid summary")
            summary = {key: data[key] for key in SUMMARY_KEYS}
            items = await self._async_items()
            if self._instance_id is None:
                self._instance_id = await instance_id.async_get(self.hass)
        except (ClientError, TimeoutError, ResponseTooLarge, ValueError) as err:
            raise UpdateFailed("Cannot read the backend task inventory") from err
        await self._async_report_presence()
        await self.async_report_registry()
        return MaintenanceData(summary, items)

    async def async_report_registry(self) -> None:
        """Publish optional HA metadata without affecting task availability."""
        async with self._registry_lock:
            if (
                self._entry.state is not ConfigEntryState.LOADED
                or self._instance_id is None
            ):
                return
            try:
                payload = registry_payload(self.hass, self._instance_id)
            except HomeAssistantError, ValueError:
                payload = unavailable_payload(self._instance_id, "unavailable")
            try:
                response = await self.client.request(
                    "PUT",
                    "/v1/integration/registry",
                    body=payload,
                    actor=Actor(system_actor="ha:integration"),
                )
            except ClientError, TimeoutError, ResponseTooLarge:
                _LOGGER.debug("Cannot send the Home Assistant registry catalog")
                return
            if response.status != 204:
                _LOGGER.debug("The backend could not accept its registry catalog")

    async def _async_items(self) -> tuple[MaintenanceItem, ...]:
        """Follow the service's cursor without silently dropping later tasks."""
        items: list[MaintenanceItem] = []
        seen_tokens: set[str] = set()
        seen_ids: set[str] = set()
        seen_occurrences: set[UUID] = set()
        page_token: str | None = None
        page_limit = 200
        while True:
            query = {"limit": str(page_limit)}
            if page_token is not None:
                query["page_token"] = page_token
            try:
                response = await self.client.request(
                    "GET", f"/v1/items?{urlencode(query)}"
                )
            except ResponseTooLarge:
                if page_limit == 1:
                    raise
                # Retry the same cursor: no partial page has been consumed.
                page_limit = max(1, page_limit // 2)
                continue
            if response.status in {401, 403}:
                raise ConfigEntryAuthFailed("The backend rejected its credential")
            if response.status != 200:
                raise UpdateFailed("The backend could not return its items")
            data = response.json()
            if not isinstance(data, dict) or not isinstance(data.get("items"), list):
                raise ValueError("Invalid item page")
            for record in data["items"]:
                item = MaintenanceItem.parse(record)
                if item.id in seen_ids or item.occurrence_id in seen_occurrences:
                    raise ValueError("Duplicate item or occurrence in backend pages")
                seen_ids.add(item.id)
                seen_occurrences.add(item.occurrence_id)
                items.append(item)
            page_token = data.get("next_page")
            if page_token is None:
                return tuple(items)
            if (
                not isinstance(page_token, str)
                or not page_token
                or page_token in seen_tokens
            ):
                raise ValueError("Invalid item page cursor")
            seen_tokens.add(page_token)

    async def _async_report_presence(self) -> None:
        """Check in only after summary authentication and complete entry setup."""
        if self._entry.state is not ConfigEntryState.LOADED or not self._entry.data.get(
            CONF_ADDON_SLUG
        ):
            return
        backup = await self._async_backup_report()
        # Unloading may begin while Supervisor is reading its backup inventory.
        if self._entry.state is not ConfigEntryState.LOADED:
            return
        report: PresenceReport = {"version": self._version, "backup": backup}
        owner = await self.hass.auth.async_get_owner()
        if self._entry.state is not ConfigEntryState.LOADED:
            return
        if owner is not None and owner.is_active and owner.is_admin:
            report["owner_id"] = owner.id
        try:
            response = await self.client.request(
                "POST",
                "/v1/integration/presence",
                body=json.dumps(report).encode(),
                actor=Actor(system_actor="ha:integration"),
            )
        except ClientError, TimeoutError, ResponseTooLarge:
            _LOGGER.debug("Cannot send the integration check-in")
            return
        if response.status in {401, 403}:
            raise ConfigEntryAuthFailed(
                "The backend rejected its integration credential"
            )
        if response.status != 204:
            _LOGGER.debug("The backend could not accept its integration check-in")

    async def _async_backup_report(self) -> BackupReport:
        slug = self._entry.data.get(CONF_ADDON_SLUG)
        if not slug:
            return {"status": "standalone", "last_backup": None}
        if not is_hassio(self.hass):
            return {"status": "unavailable", "last_backup": None}
        from aiohasupervisor import SupervisorError
        from homeassistant.components.hassio import get_supervisor_client

        try:
            async with asyncio.timeout(_BACKUP_TIMEOUT_SECONDS):
                info = await get_supervisor_client(self.hass).backups.info()
        except SupervisorError, ClientError, TimeoutError, ValueError, LookupError:
            return {"status": "unavailable", "last_backup": None}
        backups = [backup for backup in info.backups if slug in backup.content.addons]
        if not backups:
            return {"status": "missing", "last_backup": None}
        current = [
            backup for backup in backups if backup.date >= self._entry.modified_at
        ]
        if not current:
            return {
                "status": "outdated",
                "last_backup": int(max(backup.date for backup in backups).timestamp()),
            }
        backups = current
        complete = [backup for backup in backups if backup.content.homeassistant]
        latest = max(complete or backups, key=lambda backup: backup.date)
        return {
            "status": "complete" if complete else "app_only",
            "last_backup": int(latest.date.timestamp()),
        }
