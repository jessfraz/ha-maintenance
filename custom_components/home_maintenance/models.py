"""Validate the backend fields consumed by native Home Assistant entities."""

import re
from dataclasses import dataclass
from datetime import date, datetime, timedelta
from enum import StrEnum
from uuid import UUID

ITEM_ID = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")


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
    """One backend-owned task and its current scheduled occurrence."""

    id: str
    name: str
    summary: str
    description: str | None
    revision: int
    archived: bool
    occurrence_id: UUID
    due: date
    instructions: tuple[str, ...]
    checklist: tuple[ChecklistStep, ...]

    @property
    def todo_description(self) -> str | None:
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
        revision = value.get("revision")
        archived = value.get("archived")
        occurrence = value.get("occurrence")
        instructions = value.get("instructions")
        if (
            not isinstance(item_id, str)
            or not ITEM_ID.fullmatch(item_id)
            or not isinstance(name, str)
            or not name.strip()
            or not isinstance(summary, str)
            or not summary.strip()
            or (description is not None and not isinstance(description, str))
            or type(revision) is not int
            or revision < 1
            or type(archived) is not bool
            or not isinstance(occurrence, dict)
            or not isinstance(instructions, list)
            or not all(isinstance(line, str) for line in instructions)
        ):
            raise ValueError("Invalid item")
        occurrence_id = occurrence.get("id")
        due = occurrence.get("due")
        checklist = occurrence.get("checklist")
        if not isinstance(occurrence_id, str) or not isinstance(due, str):
            raise ValueError("Invalid occurrence")
        if not isinstance(checklist, list):
            raise ValueError("Invalid occurrence checklist")
        steps = tuple(ChecklistStep.parse(record) for record in checklist)
        if len({step.id for step in steps}) != len(steps):
            raise ValueError("Duplicate occurrence checklist step")
        due_date = date.fromisoformat(due)
        if due_date.isoformat() != due:
            raise ValueError("Invalid occurrence date")
        return cls(
            item_id,
            name,
            summary,
            description,
            revision,
            archived,
            UUID(occurrence_id),
            due_date,
            tuple(instructions),
            steps,
        )


@dataclass(frozen=True, slots=True)
class MaintenanceData:
    """An atomic polling snapshot for all native entities."""

    summary: dict[str, int]
    items: tuple[MaintenanceItem, ...]
