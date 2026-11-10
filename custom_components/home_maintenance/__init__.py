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

from ._app_kit.version import LOADED_VERSION
from .card import MaintenanceCardView
from .client import MaintenanceClient
from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from .coordinator import MaintenanceCoordinator
from .http import MaintenanceView
from .services import async_register_services

PLATFORMS = [
    Platform.BINARY_SENSOR,
    Platform.CALENDAR,
    Platform.SENSOR,
    Platform.TODO,
]
type MaintenanceConfigEntry = ConfigEntry[MaintenanceCoordinator]


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Install the authenticated view once, independent of reloads."""
    hass.http.register_view(MaintenanceView(hass))
    hass.http.register_view(MaintenanceCardView(hass))
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
        hass, resource_path + "?" + urlencode({"v": LOADED_VERSION})
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
    coordinator = MaintenanceCoordinator(hass, entry, client, LOADED_VERSION)
    await coordinator.async_config_entry_first_refresh()
    entry.runtime_data = coordinator
    coordinator.repairs.async_start()
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    coordinator.async_listen_to_registries()

    async def async_check_in() -> None:
        if entry.data.get(CONF_ADDON_SLUG):
            await coordinator.async_refresh()
        else:
            await coordinator.async_report_registry()
        if entry.state is ConfigEntryState.LOADED:
            coordinator.repairs.async_start_version_checks()

    @callback
    def report_loaded() -> None:
        if entry.state is ConfigEntryState.LOADED:
            # Complete the existing installation check-in before version checks.
            entry.async_create_background_task(
                hass, async_check_in(), "Maintenance loaded check-in"
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
