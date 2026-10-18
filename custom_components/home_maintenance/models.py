"""Validate the backend fields consumed by native Home Assistant entities."""

import re
from dataclasses import dataclass
from datetime import date, datetime, timedelta
from enum import StrEnum
from uuid import UUID
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

ITEM_ID = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
REGISTRY_ID = re.compile(r"[A-Za-z0-9_-]{1,128}")


class HomeAssistantKind(StrEnum):
    """The HA registry that owns a linked object."""

    DEVICE = "device"
    ENTITY = "entity"


@dataclass(frozen=True, slots=True)
class HomeAssistantReference:
    """An instance-scoped registry identity, independent of names and entity IDs."""

    kind: HomeAssistantKind
    instance_id: str
    id: str

    @classmethod
    def parse(cls, value: object) -> HomeAssistantReference:
        if not isinstance(value, dict):
            raise ValueError("Invalid Home Assistant reference")
        kind = value.get("kind")
        instance_id = value.get("instance_id")
        identity = value.get("id")
        if (
            not isinstance(kind, str)
            or not isinstance(instance_id, str)
            or not REGISTRY_ID.fullmatch(instance_id)
            or not isinstance(identity, str)
            or not REGISTRY_ID.fullmatch(identity)
        ):
            raise ValueError("Invalid Home Assistant reference")
        return cls(HomeAssistantKind(kind), instance_id, identity)


class ItemStatus(StrEnum):
    """Whether the backend-owned current occurrence still needs action."""

    NEEDS_ACTION = "needs_action"
    COMPLETED = "completed"


class ScheduleKind(StrEnum):
    """Task lifecycles owned and enforced by the backend."""

    ONCE = "once"
    STANDING = "standing"
    RECURRING = "recurring"
    INSTRUCTION = "instruction"


class ActorKind(StrEnum):
    """Backend-owned attribution kinds."""

    USER = "user"
    SYSTEM = "system"


@dataclass(frozen=True, slots=True)
class CompletionActor:
    """Identity recorded by the backend for an actual checklist change."""

    kind: ActorKind
    id: str
    name: str

    @classmethod
    def parse(cls, value: object) -> CompletionActor:
        if not isinstance(value, dict):
            raise ValueError("Invalid completion actor")
        kind = value.get("kind")
        identity = value.get("id")
        name = value.get("name")
        if (
            not isinstance(kind, str)
            or not isinstance(identity, str)
            or not identity
            or not isinstance(name, str)
            or not name
        ):
            raise ValueError("Invalid completion actor")
        return cls(ActorKind(kind), identity, name)


@dataclass(frozen=True, slots=True)
class ChecklistCompletion:
    """A checklist step's durable timestamp and actor."""

    completed_at: datetime
    actor: CompletionActor

    @classmethod
    def parse(cls, value: object) -> ChecklistCompletion:
        if not isinstance(value, dict):
            raise ValueError("Invalid checklist completion")
        timestamp = value.get("completed_at")
        if not isinstance(timestamp, str):
            raise ValueError("Invalid checklist completion timestamp")
        completed_at = datetime.fromisoformat(timestamp)
        if completed_at.utcoffset() != timedelta(0):
            raise ValueError("Checklist completion timestamp must be UTC")
        return cls(completed_at, CompletionActor.parse(value.get("actor")))


@dataclass(frozen=True, slots=True)
class ChecklistStep:
    """One backend-owned step within the current parent occurrence."""

    id: UUID
    summary: str
    completion: ChecklistCompletion | None

    @classmethod
    def parse(cls, value: object) -> ChecklistStep:
        if not isinstance(value, dict):
            raise ValueError("Invalid checklist step")
        identity = value.get("id")
        summary = value.get("summary")
        if (
            not isinstance(identity, str)
            or not isinstance(summary, str)
            or not summary.strip()
        ):
            raise ValueError("Invalid checklist step")
        completion = value.get("completion")
        return cls(
            UUID(identity),
            summary,
            ChecklistCompletion.parse(completion) if completion is not None else None,
        )


@dataclass(frozen=True, slots=True)
class MaintenanceItem:
    """One backend-owned task and its current occurrence."""

    id: str
    name: str
    summary: str
    description: str | None
    location: str | None
    timezone: ZoneInfo
    revision: int
    archived: bool
    status: ItemStatus
    schedule_kind: ScheduleKind
    completed_at: datetime | None
    occurrence_id: UUID
    due: date | None
    instructions: tuple[str, ...]
    checklist: tuple[ChecklistStep, ...]
    home_assistant: tuple[HomeAssistantReference, ...]

    @property
    def task_description(self) -> str | None:
        """Keep detailed notes and readable step progress on the parent task."""
        sections: list[str] = []
        if self.description:
            sections.append(self.description)
        if self.instructions:
            sections.append(
                "Instructions\n"
                + "\n".join(
                    f"{number}. {line}"
                    for number, line in enumerate(self.instructions, start=1)
                )
            )
        if self.checklist:
            completed = sum(step.completion is not None for step in self.checklist)
            sections.append(
                f"Checklist ({completed}/{len(self.checklist)} complete)\n"
                + "\n".join(
                    f"- [{'x' if step.completion is not None else ' '}] {step.summary}"
                    for step in self.checklist
                )
            )
        return "\n\n".join(sections) or None

    @classmethod
    def parse(cls, value: object) -> MaintenanceItem:
        """Reject incomplete or malformed records at the HTTP boundary."""
        if not isinstance(value, dict):
            raise ValueError("Invalid item")
        item_id = value.get("id")
        name = value.get("name")
        summary = value.get("summary")
        description = value.get("description")
        location = value.get("location")
        timezone_name = value.get("timezone")
        revision = value.get("revision")
        archived = value.get("archived")
        status = value.get("status")
        schedule = value.get("schedule")
        occurrence = value.get("occurrence")
        instructions = value.get("instructions")
        home_assistant = value.get("home_assistant", [])
        if (
            not isinstance(item_id, str)
            or not ITEM_ID.fullmatch(item_id)
            or not isinstance(name, str)
            or not name.strip()
            or not isinstance(summary, str)
            or not summary.strip()
            or (description is not None and not isinstance(description, str))
            or (location is not None and not isinstance(location, str))
            or not isinstance(timezone_name, str)
            or type(revision) is not int
            or revision < 1
            or type(archived) is not bool
            or not isinstance(status, str)
            or not isinstance(schedule, dict)
            or not isinstance(schedule.get("kind"), str)
            or not isinstance(occurrence, dict)
            or not isinstance(instructions, list)
            or not all(isinstance(line, str) for line in instructions)
            or not isinstance(home_assistant, list)
            or len(home_assistant) > 100
        ):
            raise ValueError("Invalid item")
        try:
            timezone = ZoneInfo(timezone_name)
        except (ZoneInfoNotFoundError, ValueError) as err:
            raise ValueError("Invalid item timezone") from err
        completed_at = None
        last_completion = value.get("last_completion")
        if last_completion is not None:
            if not isinstance(last_completion, dict):
                raise ValueError("Invalid last completion")
            timestamp = last_completion.get("completed_at")
            if timestamp is not None:
                if not isinstance(timestamp, str):
                    raise ValueError("Invalid completion timestamp")
                completed_at = datetime.fromisoformat(timestamp)
                if completed_at.utcoffset() != timedelta(0):
                    raise ValueError("Completion timestamp must be UTC")
        occurrence_id = occurrence.get("id")
        due = occurrence.get("due")
        checklist = occurrence.get("checklist")
        if (
            not isinstance(occurrence_id, str)
            or "due" not in occurrence
            or (due is not None and not isinstance(due, str))
        ):
            raise ValueError("Invalid occurrence")
        if not isinstance(checklist, list):
            raise ValueError("Invalid occurrence checklist")
        steps = tuple(ChecklistStep.parse(record) for record in checklist)
        if len({step.id for step in steps}) != len(steps):
            raise ValueError("Duplicate occurrence checklist step")
        references = tuple(
            HomeAssistantReference.parse(record) for record in home_assistant
        )
        if len(set(references)) != len(references):
            raise ValueError("Duplicate Home Assistant reference")
        due_date = date.fromisoformat(due) if due is not None else None
        # All-day calendar events need a following date for their exclusive end.
        if due_date is not None and (
            due_date.isoformat() != due or due_date == date.max
        ):
            raise ValueError("Invalid occurrence date")
        return cls(
            item_id,
            name,
            summary,
            description,
            location,
            timezone,
            revision,
            archived,
            ItemStatus(status),
            ScheduleKind(schedule["kind"]),
            completed_at,
            UUID(occurrence_id),
            due_date,
            tuple(instructions),
            steps,
            references,
        )


@dataclass(frozen=True, slots=True)
class MaintenanceData:
    """An atomic polling snapshot for all native entities."""

    summary: dict[str, int]
    items: tuple[MaintenanceItem, ...]
