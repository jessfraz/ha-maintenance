"""Home Assistant adapter for the local Maintenance service."""

import asyncio
from contextlib import suppress
from pathlib import Path
from urllib.parse import urlencode

from homeassistant.components import frontend
from homeassistant.components.http import StaticPathConfig
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.typing import ConfigType
from homeassistant.loader import async_get_integration

from .card import MaintenanceCardView
from .client import MaintenanceClient
from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from .coordinator import MaintenanceCoordinator
from .http import MaintenanceView
from .services import async_register_services

PLATFORMS = [Platform.CALENDAR, Platform.SENSOR, Platform.TODO]
type MaintenanceConfigEntry = ConfigEntry[MaintenanceCoordinator]


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Install the authenticated view once, independent of reloads."""
    hass.http.register_view(MaintenanceView(hass))
    hass.http.register_view(MaintenanceCardView(hass))
    integration = await async_get_integration(hass, DOMAIN)
    resource_path = f"/{DOMAIN}/maintenance-task-list.js"
    await hass.http.async_register_static_paths(
        [
            StaticPathConfig(
                resource_path,
                str(Path(__file__).parent / "frontend" / "maintenance-task-list.js"),
                True,
            ),
        ]
    )
    frontend.add_extra_js_url(
        hass, resource_path + "?" + urlencode({"v": str(integration.version)})
    )
    async_register_services(hass)
    return True


async def async_setup_entry(hass: HomeAssistant, entry: MaintenanceConfigEntry) -> bool:
    """Connect one backend and expose its native entities."""
    client = MaintenanceClient(
        async_get_clientsession(hass),
        entry.data[CONF_BACKEND_URL],
        entry.data[CONF_TOKEN],
    )
    integration = await async_get_integration(hass, DOMAIN)
    coordinator = MaintenanceCoordinator(hass, entry, client, str(integration.version))
    await coordinator.async_config_entry_first_refresh()
    entry.runtime_data = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    coordinator.async_listen_to_registries()

    @callback
    def report_loaded() -> None:
        if entry.state is ConfigEntryState.LOADED:
            # The paired app also needs its loaded installation check-in.
            # Entry-owned work is cancelled if unloading starts during the report.
            entry.async_create_background_task(
                hass,
                coordinator.async_refresh()
                if entry.data.get(CONF_ADDON_SLUG)
                else coordinator.async_report_registry(),
                "Maintenance loaded check-in",
            )

    entry.async_on_unload(entry.async_on_state_change(report_loaded))
    return True


async def async_unload_entry(
    hass: HomeAssistant, entry: MaintenanceConfigEntry
) -> bool:
    """Release entities and polling while retaining HA's shared HTTP session."""
    pending = entry.runtime_data.instructions_task
    if pending is not None and not pending.done():
        # Finish a cancelled add's rollback before Core tears down its platform.
        pending.cancel()
        with suppress(asyncio.CancelledError):
            await pending
    entry.runtime_data.instructions_task = None
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
