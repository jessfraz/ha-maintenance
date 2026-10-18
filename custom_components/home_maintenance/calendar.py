"""Read-only due dates for backend-owned maintenance occurrences."""

from datetime import datetime, timedelta

from homeassistant.components.calendar import CalendarEntity, CalendarEvent
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity
from homeassistant.util import dt as dt_util

from .const import DOMAIN
from .coordinator import MaintenanceCoordinator
from .models import ItemStatus, ScheduleKind


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[MaintenanceCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    """Expose the current schedule without creating another recurrence engine."""
    async_add_entities([MaintenanceCalendar(entry.runtime_data, entry.entry_id)])


class MaintenanceCalendar(CoordinatorEntity[MaintenanceCoordinator], CalendarEntity):
    """One all-day event per active occurrence, updated by the shared coordinator."""

    _attr_has_entity_name = True
    _attr_name = None
    _attr_supported_features = 0

    def __init__(self, coordinator: MaintenanceCoordinator, entry_id: str) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry_id}_schedule"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry_id)},
            name="Maintenance",
            manufacturer="Maintenance",
            model="Maintenance registry",
        )

    def _events(self) -> list[CalendarEvent]:
        """Project known dates for active occurrences without inventing a schedule."""
        events: list[CalendarEvent] = []
        for item in self.coordinator.data.items:
            if (
                item.archived
                or item.status is ItemStatus.COMPLETED
                or item.schedule_kind is ScheduleKind.INSTRUCTION
                or item.due is None
            ):
                continue
            events.append(
                CalendarEvent(
                    start=item.due,
                    end=item.due + timedelta(days=1),
                    summary=item.summary,
                    description=item.task_description,
                    location=item.location,
                    uid=str(item.occurrence_id),
                )
            )
        return sorted(
            events, key=lambda event: (event.start_datetime_local, event.uid or "")
        )

    @property
    def event(self) -> CalendarEvent | None:
        """Return today's or the next task, leaving overdue dates in the past."""
        now = dt_util.now()
        return next(
            (event for event in self._events() if event.end_datetime_local > now),
            None,
        )

    async def async_get_events(
        self, hass: HomeAssistant, start_date: datetime, end_date: datetime
    ) -> list[CalendarEvent]:
        """Use HA-local midnight and exclusive bounds, including across DST."""
        if not self.available:
            raise HomeAssistantError("The Maintenance service is unavailable")
        if start_date >= end_date:
            return []
        return [
            event
            for event in self._events()
            if event.start_datetime_local < end_date
            and event.end_datetime_local > start_date
        ]
