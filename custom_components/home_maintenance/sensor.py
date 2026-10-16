"""Maintenance counts exposed as native sensors."""

from typing import override

from homeassistant.components.sensor import SensorEntity, SensorEntityDescription
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN, SUMMARY_KEYS
from .coordinator import MaintenanceCoordinator


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[MaintenanceCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    """Add summary entities for the configured backend."""
    async_add_entities(
        MaintenanceCountSensor(entry.runtime_data, entry.entry_id, key)
        for key in SUMMARY_KEYS
    )


class MaintenanceCountSensor(CoordinatorEntity[MaintenanceCoordinator], SensorEntity):
    """A backend-owned count, with no local schedule interpretation."""

    _attr_has_entity_name = True

    def __init__(
        self, coordinator: MaintenanceCoordinator, entry_id: str, key: str
    ) -> None:
        super().__init__(coordinator)
        self.entity_description = SensorEntityDescription(
            key=key, translation_key=key, icon="mdi:clipboard-check-outline"
        )
        self._attr_unique_id = f"{entry_id}_{key}"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry_id)},
            name="Maintenance",
            manufacturer="Maintenance",
            model="Maintenance registry",
        )

    @override
    async def async_added_to_hass(self) -> None:
        """Initialize from the latest snapshot after registering the listener."""
        await super().async_added_to_hass()
        self._attr_native_value = self.coordinator.data.summary[
            self.entity_description.key
        ]

    @callback
    @override
    def _handle_coordinator_update(self) -> None:
        """Update HA's native value before publishing its state."""
        self._attr_native_value = self.coordinator.data.summary[
            self.entity_description.key
        ]
        super()._handle_coordinator_update()
