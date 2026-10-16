"""A native to-do list of backend-owned maintenance occurrences."""

from collections.abc import Coroutine
from contextvars import ContextVar
from typing import override

from homeassistant.components.todo import (
    TodoItem,
    TodoItemStatus,
    TodoListEntity,
    TodoListEntityFeature,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import Context, HomeAssistant, callback
from homeassistant.exceptions import ServiceValidationError
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN
from .coordinator import MaintenanceCoordinator
from .services import async_actor, async_complete


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[MaintenanceCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    """Expose a single list while the Rust service owns recurrence and history."""
    async_add_entities([MaintenanceTodo(entry.runtime_data, entry.entry_id)])


class MaintenanceTodo(CoordinatorEntity[MaintenanceCoordinator], TodoListEntity):
    """Complete occurrences through HA without editing task definitions."""

    _attr_has_entity_name = True
    _attr_name = None
    _attr_supported_features = TodoListEntityFeature.UPDATE_TODO_ITEM
    _attr_icon = "mdi:clipboard-check-outline"

    def __init__(self, coordinator: MaintenanceCoordinator, entry_id: str) -> None:
        super().__init__(coordinator)
        self._service_context = ContextVar[Context | None](
            "maintenance_service_context", default=None
        )
        self._attr_unique_id = f"{entry_id}_tasks"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry_id)},
            name="Maintenance",
            manufacturer="Maintenance",
            model="Maintenance registry",
        )

    @override
    async def async_request_call[T](self, coro: Coroutine[object, object, T]) -> T:
        """Scope identity to Core's actual native entity-service invocation."""
        token = self._service_context.set(Context())
        try:
            # Core sets context inside this call and creates its child task.
            # ContextVar keeps that identity local across concurrent services.
            return await super().async_request_call(coro)
        finally:
            self._service_context.reset(token)

    @callback
    @override
    def async_set_context(self, context: Context) -> None:
        """Preserve Core's state context and capture the current service actor."""
        super().async_set_context(context)
        if self._service_context.get() is not None:
            self._service_context.set(context)

    @override
    async def async_added_to_hass(self) -> None:
        """Initialize from the latest snapshot after registering the listener."""
        await super().async_added_to_hass()
        self._update_todo_items()

    @callback
    @override
    def _handle_coordinator_update(self) -> None:
        """Publish the coordinated items through HA's native attribute."""
        self._update_todo_items()
        super()._handle_coordinator_update()

    def _update_todo_items(self) -> None:
        """An occurrence UID changes when the backend schedules its successor."""
        self._attr_todo_items = [
            TodoItem(
                uid=str(item.occurrence_id),
                summary=item.summary,
                status=TodoItemStatus.NEEDS_ACTION,
                due=item.due,
                description=item.task_description,
            )
            for item in self.coordinator.data.items
            if not item.archived
        ]

    async def async_update_todo_item(self, item: TodoItem) -> None:
        """Accept completion only; recurrence and definitions stay in Rust."""
        # Assist calls the entity directly without forwarding a user context.
        # Entity._context persists after reads too, so it is not actor evidence.
        context = self._service_context.get() or Context()
        current = next(
            (
                current
                for current in self.coordinator.data.items
                if not current.archived and str(current.occurrence_id) == item.uid
            ),
            None,
        )
        if current is None:
            raise ServiceValidationError(
                "This maintenance occurrence is no longer active"
            )
        if (
            item.status != TodoItemStatus.COMPLETED
            or (item.summary is not None and item.summary != current.summary)
            or (item.due is not None and item.due != current.due)
            or (
                item.description is not None
                and item.description != current.task_description
            )
        ):
            raise ServiceValidationError(
                "Only completion is supported. Edit task definitions in Maintenance."
            )
        actor = await async_actor(self.hass, context)
        await async_complete(
            self.coordinator,
            current.id,
            current.occurrence_id,
            actor,
            if_match=f'"{current.revision}"',
        )
