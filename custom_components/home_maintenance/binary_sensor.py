"""Publish service health independently of whether any maintenance is due."""

from datetime import datetime, timedelta
from typing import override

from homeassistant.components.binary_sensor import (
    BinarySensorDeviceClass,
    BinarySensorEntity,
)
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity import DeviceInfo, EntityCategory
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.event import async_track_time_interval
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import MaintenanceConfigEntry
from .const import DOMAIN
from .coordinator import MaintenanceCoordinator
from .health import health_snapshot


async def async_setup_entry(
    hass: HomeAssistant,
    entry: MaintenanceConfigEntry,
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    async_add_entities([MaintenanceConnectionProblem(entry)])


class MaintenanceConnectionProblem(
    CoordinatorEntity[MaintenanceCoordinator], BinarySensorEntity
):
    _attr_has_entity_name = True
    _attr_translation_key = "connection_problem"
    _attr_device_class = BinarySensorDeviceClass.PROBLEM
    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(self, entry: MaintenanceConfigEntry) -> None:
        super().__init__(entry.runtime_data)
        self._attr_unique_id = f"{entry.entry_id}_connection_problem"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry.entry_id)},
            name="Maintenance",
            manufacturer="Maintenance",
            model="Maintenance registry",
        )
        self._update_health()

    @property
    def available(self) -> bool:
        """A failed poll is a reported problem, not a missing problem sensor."""
        return True

    def _update_health(self) -> None:
        report = health_snapshot(self.coordinator)
        self._attr_is_on = report["available"] is not True
        self._attr_extra_state_attributes = report

    @override
    async def async_added_to_hass(self) -> None:
        await super().async_added_to_hass()
        self.async_on_remove(
            async_track_time_interval(
                self.hass, self._refresh_freshness, timedelta(seconds=60)
            )
        )

    @callback
    def _refresh_freshness(self, _now: datetime) -> None:
        self._update_health()
        self.async_write_ha_state()

    @callback
    @override
    def _handle_coordinator_update(self) -> None:
        self._update_health()
        super()._handle_coordinator_update()
