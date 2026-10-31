# Changelog

## 0.10.4

- Add the existing maintenance icon beside the app name, matching FileVault's
  header sizing, spacing and alignment in both themes. Preserve task cards
  and the rest of the dashboard styling.

## 0.10.3

- Reject distrusted emSign certificate paths with refreshed trust roots and
  certificate verification.

## 0.10.2

- Keep task actions, editor buttons, and form fields at least 44 pixels tall, including small remove and reorder controls.
- Cover the native task and editor touch targets in Chromium at phone, tablet, and desktop widths.

## 0.10.1

- Give task details and search controls a single column on phones so dates and time zones stay readable.
- Make buttons and dialog close controls at least 44 pixels tall.

## 0.10.0

- Add an optional `hide_when_empty` setting to the task-list card. Empty queues release their layout space and keep refreshing.
- Keep loading, errors, invalid responses, and editor previews visible.
- Cover dashboard visibility transitions in Chromium and 30-day boundaries in the native card API.

## 0.9.0

- Add Home Assistant's native diagnostics download with safe operational metadata and aggregate counts.
- Omit credentials, endpoints, task contents, and task or registry identifiers from the integration's diagnostic payload.
- Use cached state so diagnostics remain available during service outages without refreshing or changing data.

## 0.8.0

- Expose task queries and completion through Home Assistant's Assist LLM API.
- Return bounded task snapshots with IDs, due dates, occurrences, and revisions so agents can clarify names and use conversational context.
- Require exact occurrence and revision values for completion, with entity exposure, user permissions, and retry checks enforced by the tools.
- Add optional blueprints for daily maintenance reminders and overdue alerts.

## 0.7.0

- Ask local Assist about upcoming, overdue, and next maintenance tasks, or work due today, tomorrow, this week, or next week.
- Complete a task by name or action summary. Ambiguous names prompt for a more specific command without changing any tasks.
- Respect Assist exposure and user permissions, refresh before answering, and protect completion with occurrence, revision, and retry checks.

## 0.6.1

- Close task and edit dialogs when clicking or tapping outside them.
- Keep edits open while saving and ignore drags that start or end inside a dialog.
- Keep linked Home Assistant devices and entities on the current HA address when opened through ingress.
- Add matching app and integration icons, including dark and high-resolution integration variants.

## 0.6.0

- Move summary filters below the Queue heading and use the same status colors as Shipments, including dark mode.
- Permanently delete an archived item and its completion history from its detail view, with explicit confirmation and revision checks.
- Keep deleted IDs and source mappings reserved so imports and retries cannot recreate deleted tasks. Reorder requests stay available.

## 0.5.1

- Show the Home Assistant Instructions list only while active instructions exist.
- Remove empty lists left by earlier versions on upgrade; restore saved entity names and IDs when instructions are added again.
- Finish pending entity additions before unloading the integration.

## 0.5.0

- Add a Home Assistant task-list card with current status and due dates. Each row opens the exact task in Maintenance through authenticated ingress and preserves it through login.
- Load the card automatically with the integration and support active, upcoming, overdue, and completed views.
- Link each maintenance item to multiple Home Assistant devices and entities, with the same objects shared across tasks.
- Choose links by name or entity ID in the editor. Stable registry IDs preserve associations through renames; missing and disabled objects retain their links.
- Open linked objects in HA and include links in native to-do descriptions. Calendar descriptions show names and page URLs in HA's plain-text event dialog.
- Keep the bounded registry catalog in memory and persist only explicitly selected references. Offline, unavailable, and foreign-instance links remain visible for repair.

## 0.4.0

- Standing instructions stay visible in a separate, read-only list and cannot be completed. Only the Home Assistant owner can edit or remove them.
- Recurring tasks require a concrete first due date. The form defaults to today in the selected time zone; the API rejects missing or cleared dates.
- Owner permissions are enforced in the backend for protected creation, edits, type changes, archival, restoration, and migration.
- Dashboard summary cards filter the queue with keyboard support and a visible selection. Completion history includes recurring and archived tasks with recorded work.

## 0.3.0

- Optional due dates and explicit one-off, standing, and recurring task types.
- Completed tasks retain their status and known completion timestamps in Home Assistant; calendars show active, dated work only.
- Migration preview reports source mappings and conflicts without writing data. Atomic import preserves source IDs, completion history, and unknown historical facts.
- Existing SQLite state and retry receipts survive the schema upgrade.
- Update the Docker example and check its version in CI.

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
