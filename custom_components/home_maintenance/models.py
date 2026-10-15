"""Validate the backend fields consumed by native Home Assistant entities."""

import re
from dataclasses import dataclass
from datetime import date
from uuid import UUID

ITEM_ID = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")


@dataclass(frozen=True, slots=True)
class MaintenanceItem:
    """One backend-owned task and its current scheduled occurrence."""

    id: str
    name: str
    revision: int
    archived: bool
    occurrence_id: UUID
    due: date
    instructions: tuple[str, ...]

    @classmethod
    def parse(cls, value: object) -> MaintenanceItem:
        """Reject incomplete or malformed records at the HTTP boundary."""
        if not isinstance(value, dict):
            raise ValueError("Invalid item")
        item_id = value.get("id")
        name = value.get("name")
        revision = value.get("revision")
        archived = value.get("archived")
        occurrence = value.get("occurrence")
        instructions = value.get("instructions")
        if (
            not isinstance(item_id, str)
            or not ITEM_ID.fullmatch(item_id)
            or not isinstance(name, str)
            or not name.strip()
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
        if not isinstance(occurrence_id, str) or not isinstance(due, str):
            raise ValueError("Invalid occurrence")
        due_date = date.fromisoformat(due)
        if due_date.isoformat() != due:
            raise ValueError("Invalid occurrence date")
        return cls(
            item_id,
            name,
            revision,
            archived,
            UUID(occurrence_id),
            due_date,
            tuple(instructions),
        )


@dataclass(frozen=True, slots=True)
class MaintenanceData:
    """An atomic polling snapshot for all native entities."""

    summary: dict[str, int]
    items: tuple[MaintenanceItem, ...]
