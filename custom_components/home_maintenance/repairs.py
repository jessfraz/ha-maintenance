"""Native backend Repairs are separate from scheduled maintenance tasks."""

import re
from datetime import datetime, timedelta
from typing import TYPE_CHECKING

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryAuthFailed
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.event import async_track_time_interval

from .const import CONF_ADDON_SLUG, DOMAIN
from .health import health_snapshot

if TYPE_CHECKING:
    from .coordinator import MaintenanceCoordinator


class MaintenanceRepairs:
    """Only a fresh, complete backend response clears an availability incident."""

    def __init__(
        self,
        hass: HomeAssistant,
        entry: ConfigEntry,
        coordinator: MaintenanceCoordinator,
    ) -> None:
        self.hass = hass
        self.entry = entry
        self.coordinator = coordinator
        self._issue_id = f"{entry.entry_id}_backend"
        slug = entry.data.get(CONF_ADDON_SLUG)
        self._link = (
            f"homeassistant://config/app/{slug}/logs"
            if isinstance(slug, str) and re.fullmatch(r"[a-z0-9_]+", slug)
            else f"homeassistant://config/integrations/integration/{DOMAIN}"
        )

    @callback
    def async_start(self) -> None:
        """Own a polling consumer and freshness timer even with entities disabled."""
        self.entry.async_on_unload(
            self.coordinator.async_add_listener(self.async_reconcile)
        )
        self.entry.async_on_unload(
            async_track_time_interval(
                self.hass, self._async_tick, timedelta(seconds=60)
            )
        )
        self.async_reconcile()

    @callback
    def _async_tick(self, _now: datetime) -> None:
        self.async_reconcile()

    @callback
    def async_connection_failed(self) -> None:
        """Publish initial setup and transport failures without backend details."""
        if self.coordinator._shutdown_requested:
            return
        ir.async_create_issue(
            self.hass,
            DOMAIN,
            self._issue_id,
            config_entry_id=self.entry.entry_id,
            is_fixable=False,
            is_persistent=True,
            severity=ir.IssueSeverity.WARNING,
            translation_key="backend_unavailable",
            learn_more_url=self._link,
        )

    @callback
    def async_auth_failed(self) -> None:
        """Core already owns the credential Repair for this availability gap."""
        if not self.coordinator._shutdown_requested:
            ir.async_delete_issue(self.hass, DOMAIN, self._issue_id)

    @callback
    def async_reconcile(self) -> None:
        # Shutdown unsubscribes future polls without cancelling an in-flight read.
        if self.coordinator._shutdown_requested:
            return
        if not self.coordinator.last_update_success and isinstance(
            self.coordinator.last_exception, ConfigEntryAuthFailed
        ):
            self.async_auth_failed()
            return
        if health_snapshot(self.coordinator)["available"]:
            ir.async_delete_issue(self.hass, DOMAIN, self._issue_id)
        else:
            self.async_connection_failed()
