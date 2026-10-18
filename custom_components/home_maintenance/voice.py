"""Local Maintenance sentences on Home Assistant's built-in conversation agent."""

import re
import unicodedata
from datetime import date, datetime
from enum import StrEnum
from functools import partial
from uuid import NAMESPACE_URL, uuid5

from hassil.recognize import RecognizeResult
from homeassistant.auth.permissions.const import POLICY_CONTROL, POLICY_READ
from homeassistant.components.conversation import ConversationInput
from homeassistant.components.conversation.agent_manager import get_agent_manager
from homeassistant.components.homeassistant.exposed_entities import async_should_expose
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import entity_registry as er
from homeassistant.util import dt as dt_util

from .const import DOMAIN
from .coordinator import MaintenanceCoordinator
from .models import ItemStatus, MaintenanceItem, ScheduleKind
from .services import async_actor, async_complete

_MAX_SPOKEN_TASKS = 5


class Query(StrEnum):
    """The English date windows understood by this adapter."""

    TODAY = "today"
    TOMORROW = "tomorrow"
    THIS_WEEK = "this week"
    NEXT_WEEK = "next week"
    OVERDUE = "overdue"
    UPCOMING = "upcoming"
    NEXT = "next"


def _normalize(text: str) -> str:
    return " ".join(
        re.findall(r"[^\W_]+", unicodedata.normalize("NFKC", text).casefold())
    )


def _label(item: MaintenanceItem) -> str:
    if _normalize(item.name) == _normalize(item.summary):
        return item.summary
    return f"{item.summary} ({item.name})"


def _spoken_date(value: date) -> str:
    return f"{value:%B} {value.day}, {value.year}"


def _matches_date(item: MaintenanceItem, query: Query, now: datetime) -> bool:
    if item.due is None:
        return False
    today = now.astimezone(item.timezone).date()
    days = (item.due - today).days
    match query:
        case Query.TODAY:
            return days == 0
        case Query.TOMORROW:
            return days == 1
        case Query.THIS_WEEK:
            return -today.weekday() <= days < 7 - today.weekday()
        case Query.NEXT_WEEK:
            return 7 - today.weekday() <= days < 14 - today.weekday()
        case Query.OVERDUE:
            return days < 0
        case Query.UPCOMING:
            return days >= 0
        case Query.NEXT:
            return True


@callback
def async_register_voice(
    hass: HomeAssistant, entry: ConfigEntry[MaintenanceCoordinator]
) -> None:
    """Own each runtime sentence registration for this loaded entry's lifetime."""
    voice = MaintenanceVoice(hass, entry)
    manager = get_agent_manager(hass)
    for query in Query:
        if query is Query.NEXT:
            sentences = [
                "what is [my|the] next maintenance task",
                "what maintenance [task] is next",
            ]
        elif query in {Query.UPCOMING, Query.OVERDUE}:
            sentences = [
                f"what maintenance [work] is {query}",
                f"what [are] [my|the] {query} maintenance tasks",
            ]
            if query is Query.UPCOMING:
                sentences.append("what maintenance [work] is coming up")
        else:
            sentences = [f"what maintenance [work] is due {query}"]
        entry.async_on_unload(
            manager.register_trigger(sentences, partial(voice.async_query, query))
        )
    entry.async_on_unload(
        manager.register_trigger(
            [
                "complete [my|the] maintenance task {task}",
                "mark [my|the] maintenance task {task} (done|complete|completed)",
                "[i] (completed|finished) [my|the] maintenance task {task}",
            ],
            voice.async_complete,
        )
    )


class MaintenanceVoice:
    """Resolve voice requests without keeping conversational or recurrence state."""

    def __init__(
        self, hass: HomeAssistant, entry: ConfigEntry[MaintenanceCoordinator]
    ) -> None:
        self.hass = hass
        self.entry = entry
        self.coordinator = entry.runtime_data

    async def _async_check_access(
        self, user_input: ConversationInput, *, control: bool
    ) -> None:
        if (
            self.entry.state is not ConfigEntryState.LOADED
            or self.entry.runtime_data is not self.coordinator
        ):
            raise HomeAssistantError("Maintenance is unavailable. Try again later.")
        registry = er.async_get(self.hass)
        entity_id = registry.async_get_entity_id(
            "todo", DOMAIN, f"{self.entry.entry_id}_tasks"
        )
        if entity_id is None or self.hass.states.get(entity_id) is None:
            raise HomeAssistantError("The Maintenance task list is unavailable.")
        if not async_should_expose(self.hass, "conversation", entity_id):
            raise HomeAssistantError(
                "The Maintenance task list is not exposed to Assist."
            )
        if user_input.context.user_id is not None:
            user = await self.hass.auth.async_get_user(user_input.context.user_id)
            permission = POLICY_CONTROL if control else POLICY_READ
            if (
                user is None
                or not user.is_active
                or not user.permissions.check_entity(entity_id, permission)
            ):
                raise HomeAssistantError(
                    "You do not have permission to access this Maintenance task list."
                )

    async def _async_tasks(
        self, user_input: ConversationInput, *, control: bool = False
    ) -> tuple[MaintenanceItem, ...]:
        await self._async_check_access(user_input, control=control)
        # A voice action needs a fresh occurrence and revision. async_refresh bypasses
        # the normal service debounce; recheck ownership and opt-in after awaiting IO.
        await self.coordinator.async_refresh()
        await self._async_check_access(user_input, control=control)
        if not self.coordinator.last_update_success:
            raise HomeAssistantError("Maintenance is unavailable. Try again later.")
        return tuple(
            item
            for item in self.coordinator.data.items
            if not item.archived
            and item.status is ItemStatus.NEEDS_ACTION
            and item.schedule_kind is not ScheduleKind.INSTRUCTION
        )

    async def async_query(
        self, query: Query, user_input: ConversationInput, result: RecognizeResult
    ) -> str:
        """Speak a bounded projection of canonical dates in each task's timezone."""
        if not user_input.language.lower().startswith("en"):
            return "Maintenance voice commands currently support English."
        try:
            tasks = await self._async_tasks(user_input)
        except HomeAssistantError as err:
            return str(err)
        now = dt_util.utcnow()
        matches = sorted(
            (item for item in tasks if _matches_date(item, query, now)),
            key=lambda item: (item.due or date.max, item.id),
        )
        if query is Query.NEXT:
            matches = matches[:1]
        if not matches:
            if query is Query.NEXT:
                return "No dated maintenance tasks."
            if query in {Query.OVERDUE, Query.UPCOMING}:
                return f"No {query} maintenance tasks."
            return f"No maintenance tasks are due {query}."
        descriptions = "; ".join(
            f"{_label(item)}, due {_spoken_date(item.due)}"
            for item in matches[:_MAX_SPOKEN_TASKS]
            if item.due is not None
        )
        count = len(matches)
        noun = "task" if count == 1 else "tasks"
        if query is Query.NEXT:
            return f"Next maintenance task: {descriptions}."
        if query in {Query.OVERDUE, Query.UPCOMING}:
            prefix = f"{count} {query} maintenance {noun}"
        else:
            prefix = f"{count} maintenance {noun} due {query}"
        limit = " Showing the first five." if count > _MAX_SPOKEN_TASKS else ""
        return f"{prefix}:{limit} {descriptions}."

    async def async_complete(
        self, user_input: ConversationInput, result: RecognizeResult
    ) -> str:
        """Only a unique recognizable task may complete its current occurrence."""
        if not user_input.language.lower().startswith("en"):
            return "Maintenance voice commands currently support English."
        captured = result.entities.get("task")
        if captured is None or not isinstance(captured.value, str):
            return "Say complete maintenance task followed by the task name."
        requested = _normalize(captured.value)
        if not requested:
            return "Say complete maintenance task followed by the task name."
        try:
            tasks = await self._async_tasks(user_input, control=True)
            exact: list[MaintenanceItem] = []
            partial_matches: list[MaintenanceItem] = []
            words = set(requested.split())
            for item in tasks:
                names = (_normalize(item.name), _normalize(item.summary))
                if requested in names:
                    exact.append(item)
                elif any(words <= set(name.split()) for name in names):
                    partial_matches.append(item)
            matches = sorted(exact or partial_matches, key=lambda item: item.id)
            if not matches:
                return "No matching active maintenance task. Say its full task name."
            if len(matches) != 1:
                names = "; ".join(_label(item) for item in matches[:_MAX_SPOKEN_TASKS])
                return (
                    f"I found {len(matches)} matching tasks: {names}. "
                    "Say complete maintenance task followed by the full task name."
                )
            item = matches[0]
            actor = await async_actor(self.hass, user_input.context)
            # A transport retry of this HA request must retain its mutation identity.
            key = str(
                uuid5(
                    NAMESPACE_URL,
                    f"{DOMAIN}:{user_input.context.id}:{item.id}",
                )
            )
            updated = await async_complete(
                self.coordinator,
                item.id,
                item.occurrence_id,
                actor,
                if_match=f'"{item.revision}"',
                idempotency_key=key,
            )
        except HomeAssistantError as err:
            return str(err)
        if updated.status is ItemStatus.NEEDS_ACTION and updated.due is not None:
            return f"Completed {item.summary}. Next due {_spoken_date(updated.due)}."
        return f"Completed {item.summary}."
