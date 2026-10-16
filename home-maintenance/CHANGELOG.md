# Changelog

## 0.2.0

- Durable reorder requests with frozen item details and actor-scoped retry receipts.
- Pending, fulfilled, and dismissed request views, with administrator resolution.
- Reorder receipts survive source changes, process restarts, and backup restore.
- Home Assistant REST bridge support and native amd64/arm64 container checks for reorder retention.

## 0.1.1

- Enforce Python annotations, Ruff, and ty in local checks and CI.
- Refresh native sensor and to-do state consistently after coordinator updates.

## 0.1.0

- Rust service with Dropshot, SQLite, and typed OpenAPI routes.
- Thin Home Assistant integration with pairing, sensors, actions, and a native to-do list.
- Atomic, revision-checked completion with durable retry handling and completion history.
- Separate item names and action summaries, optional descriptions, and occurrence checklists with actor and time records.
- Immutable checklist history and unchecked steps for each new occurrence, with revision-checked, retry-safe step changes.
- Authenticated item pages, calendar-aware recurrence, and NFC deep links.
