# Maintenance

Maintenance is a local maintenance tracker with a Rust service, SQLite, and a
thin Home Assistant integration. Items, schedules, current occurrences, and
completion history live in the service. Home Assistant exposes the same state
through sensors, actions, a native to-do list, and a calendar.

The repository follows [Shipments](https://github.com/jessfraz/ha-packages):
Dropshot routes and typed OpenAPI, SQLx migrations, an embedded dashboard,
authenticated Supervisor pairing, and a HACS integration. It runs on amd64 and
arm64 as an unprivileged user.

## Install

Install both the **Maintenance app** and the **Maintenance integration** using
`https://github.com/jessfraz/ha-maintenance` in the Home Assistant App Store and
HACS. Start the app, install the integration through HACS, restart Home
Assistant, and confirm the discovered connection in **Settings → Devices &
services**. The app generates separate owner and integration credentials;
Supervisor discovery supplies the integration credential without copying it
through the browser.

See [installation and pairing](home-maintenance/DOCS.md) for the full setup,
standalone deployment, and backups. Release images are published only from
matching `vVERSION` tags. A repository checkout or passing build does not mean
an image has been published.

## Maintenance items

Each item has a stable ID, a name for the thing being maintained, an action
summary, and an IANA time zone. One-off and standing tasks can have an unknown
due date; recurring tasks require a concrete first due date. For example,
an item named "Example air filter" has the summary "Replace the example air
filter". A description, checklist, instructions, specifications, resource
links, group, and location are optional.

Choose a task type:

- **One-off:** completion closes the task and retains its occurrence and history.
- **Standing task:** completion records the work and starts a fresh, undated occurrence.
- **Recurring:** completion starts a new occurrence using a positive calendar
  interval, measured from the actual completion date in the item's time zone.
- **Standing instruction:** permanent reference information, shown separately
  from actionable work. It has no due date, checklist, reorder, or completion.
  Only the Home Assistant owner can edit or remove it. A standalone owner
  credential has the same authority.

The API represents these as `{"kind":"once"}`, `{"kind":"standing"}`,
`{"kind":"recurring","interval":{"months":3}}`, or `{"kind":"instruction"}`
in `schedule`. A newly created item defaults to one-off when its schedule is
omitted. The form supplies today's date in the selected time zone when choosing
recurring work. API clients must supply a concrete `due`; missing or null dates
return **400**, including catalog imports and edits that would leave a recurring
task undated. One-off and standing tasks retain an omitted or null due date.

Standing instructions appear in a separate dashboard section and a read-only
Home Assistant Instructions list. They cannot be completed by a person or an
automation, and they are excluded from active-task counts and the calendar.
Other administrators cannot edit, archive, or change their type. Removal uses
the existing reversible archive; only the owner can restore it. The integration
announces the authenticated HA owner identity for ingress authorization. Missing
or expired identity evidence denies protected changes until a fresh check-in.

Checklist steps belong to the current occurrence. Checking a step records its
actor and time without completing the item. Explicitly completing the item
saves a snapshot of its checklist, including unchecked steps, and creates the
next occurrence, when the task repeats, with every step unchecked in the same
SQLite transaction.
Keeping a template step's ID and wording preserves its current check; changing
its wording clears that check. Completed history stays unchanged. Calendar
months and years retain calendar behavior at month boundaries.

The dashboard orders items by due date. Open `/items/<item-id>` for the current
occurrence, description, checklist, instructions, links, and history. NFC tags
should use the stable, authenticated Home Assistant app route, for example:

```text
https://home-assistant.example/app/<repository-app-slug>/items/example-air-filter
```

Copy the stable app route, rather than a temporary ingress session URL. Keep
item IDs stable after writing tags. Archiving an item retains its history.

The integration adds `calendar.maintenance` for calendar cards and calendar
automations. Each active, dated task appears as an all-day event on its current due
date, with its summary, description, instructions, checklist progress, and
location. Dates stay on the same calendar day in Home Assistant's time zone,
including daylight-saving changes. Undated, completed, and archived tasks are
excluded. The native to-do list retains undated tasks and completed tasks,
including a completion timestamp when one is known.
Due dates range from `0001-01-01` through `9999-12-30`, leaving a representable
following day for each event's exclusive end.

The calendar is read-only. Complete work through the to-do list, dashboard,
or integration actions. A one-off event disappears after completion; a dated
recurring successor replaces it. Standing tasks remain undated after completion.
Only the current occurrence appears because recurrence starts from actual
completion. Overdue dates stay in the past, while the to-do list and overdue
sensor continue to show unfinished work. The calendar shares the integration's
60-second refresh and updates immediately after integration actions.

## API

The service embeds its UI and exposes a versioned JSON API. Generate the exact
OpenAPI document with `just openapi`.

| Purpose | Route |
| --- | --- |
| Readiness | `GET /health` |
| List or create items | `GET /v1/items`, `POST /v1/items` |
| Import a catalog atomically | `POST /v1/imports` |
| Preview a migration mapping and conflicts | `POST /v1/migrations/preview` |
| Apply a migration atomically | `POST /v1/migrations` |
| Read or update an item | `GET /v1/items/{id}`, `PATCH /v1/items/{id}` |
| Check or uncheck a current step | `PUT /v1/items/{id}/checklist/{step_id}` |
| Complete the current occurrence | `POST /v1/items/{id}/completions` |
| Request a reorder | `POST /v1/items/{id}/reorders` |
| Read reorder requests | `GET /v1/reorders`, `GET /v1/reorders/{id}` |
| Fulfill or dismiss a reorder | `PUT /v1/reorders/{id}/resolution` |
| Read completion history | `GET /v1/items/{id}/events` |
| Archive or restore an item | `PUT /v1/items/{id}/archive`, `DELETE /v1/items/{id}/archive` |
| Counts and authenticated service status | `GET /v1/summary`, `GET /v1/system` |

Use bearer authentication for `/v1` routes. Mutations of existing items require
`If-Match` with the item's ETag. Creation, step changes, and completion require
a stable `Idempotency-Key`. Repeating an accepted request returns its saved
result; reusing a key for a different request is a conflict. Completion also
names the exact current occurrence, so a second tap cannot complete the next
occurrence. A step change sends `{ "occurrence_id": "UUID", "completed": true }`
or `false` to undo it. Authenticated users can change steps and complete items;
editing definitions requires administrator access. Archived items and stale
occurrences cannot be checked or completed. Lists use bounded cursor pagination.

Import accepts `{ "items": [...] }` with the same item shape as creation.
[`config/items.example.json`](config/items.example.json) is entirely fictional;
[`config/items.schema.json`](config/items.schema.json) describes that format.
The import validates every item and commits the whole catalog atomically.
Existing IDs conflict. Omitting an item never deletes or archives it.
The required `summary` and each checklist step's summary must be nonblank and
fit within 200 UTF-8 bytes. An optional description fits within 16,000 UTF-8
bytes and must be nonblank when supplied. A checklist defaults to empty and
supports up to 100 steps, each with a unique UUID that remains stable when
editing its text.

### Migrating existing tasks

Catalog admission through `/v1/imports` creates new definitions only. Use the
separate migration endpoints to retain existing task state and history.
[`config/migration.example.json`](config/migration.example.json) is a fictional
example of that contract.

Each migration entry supplies a `source` identity with `system`, `collection`,
and an opaque `id`, plus an `item` in the creation format. Supply the source's
`status` (`needs_action` or `completed`), optional `archived`, and `history`
from oldest to newest. Source IDs retain their exact case and contents; they
are independent of the target item's URL-safe ID. Supplied historical event
UUIDs and source identities are retained. The service allocates an internal
event UUID only when none was supplied.

History supports `completed_at`, `actor`, `occurrence_id`, `source`,
`previous_due`, `next_due`, and checklist snapshots. Unknown timestamps, actors,
occurrence IDs, source identities, and due dates remain null; checklist snapshots
default to empty.
An already-completed task may have no known completion time or historical
event; its status is preserved without fabricating either. Import never runs
the recurrence engine or recomputes dates from old completions.

Send the complete proposed mapping to `POST /v1/migrations/preview` using the
owner bearer credential. The response contains `applicable` and a per-item
mapping with `source`, `target_id`, `action` (`created`, `unchanged`, or
`conflict`), `conflicts`, and `history_events`. Preview reads one database
snapshot and writes no items, history, mappings, or retry receipts. It reports
duplicate destination IDs, conflicting source mappings, and historical event
identities that collide with stored history or active occurrences.

After reviewing the mapping, send the same body to `POST /v1/migrations` with
a stable `Idempotency-Key`. The service checks conflicts again inside the
write transaction. Any conflict rejects the whole batch with **409**; a
successful **201** saves the complete mapping, history, and retry receipt
atomically. Exact retries return the saved report. Repeating an identical
source mapping under a new key reports `unchanged` and does not overwrite
later edits or duplicate history. Neither endpoint deletes source tasks.
These endpoints use the backend API directly; the HA REST bridge does not
expose administrative migrations.

Review task types explicitly when the source has no recurrence information.
Recurring mappings require an explicit first due date; an unknown source date
is a preview conflict, not permission to manufacture a historical date.
Existing undated recurring records from 0.3 remain readable for correction;
edits must supply a real date. Completing actual work can establish its next
due date without inventing a previous one.
Matching titles alone do not establish source identity. Do not merge records,
invent dates, or infer old completion events from current state. This API
accepts all-day dates; a source containing a due timestamp needs an explicit
mapping decision before import, rather than truncating its time or time zone.
Export all available source history separately when the provider's current
to-do response contains only its latest completion timestamp.

### Reorder requests

`POST /v1/items/{id}/reorders` accepts only `{}`. Send the captured item ETag
in `If-Match` and a unique `Idempotency-Key` for each deliberate press. An
integration client also sends `X-Maintenance-System-Actor: ha:integration`
with its integration bearer credential. Home Assistant clients use
`/api/home_maintenance/v1` with HA authentication; the bridge derives the actor
from the authenticated user.

The service returns **201 Created** after atomically saving the request and its
receipt in SQLite. The receipt contains `id` (UUID), `item_id`, `item_revision`,
`requested_at` (UTC), `actor`, and an `item` snapshot with `name`, `summary`,
`description`, `specs`, `resources`, and the required `reorder_url`. All content
comes from the stored item. A reorder neither completes nor reschedules work,
and the service does not fetch vendor URLs or make purchases.

Keep the same key, body, item ID, and captured revision when retrying an
uncertain response. The original **201** receipt replays before checking the
current item, including after edits, archival, source deletion, fulfillment,
or dismissal. Keys are scoped to the authenticated actor and shared across
operations; reusing one for a different request returns **409 Conflict**.
Different keys create distinct requests. A new request needs an active item
with a reorder URL (**409** otherwise), the current revision (**412** if stale),
and a present `If-Match` (**428** if omitted). A missing source item returns
**404**. Invalid bodies, keys, and revision headers return **400**.

`GET /v1/reorders` defaults to `status=pending`; it also accepts `fulfilled` or
`dismissed`, `limit` (1–200, default 100), and an opaque `page_token`. Each record
contains the immutable `request` receipt alongside `status`, `revision`,
`resolved_at`, and `resolved_by`. `GET /v1/reorders/{id}` returns that current
record and its ETag. Administrators resolve a pending request with
`PUT /v1/reorders/{id}/resolution`, the record's `If-Match`, and
`{"status":"fulfilled"}` or `{"status":"dismissed"}`. Resolution is terminal;
neither action deletes the request or its replay receipt. Reorder responses
use `Cache-Control: no-store`.

## Development

Use the supplied Nix dev shell, which includes Rust, Python, uv, SQLite, and the
repository validators. Entering or updating that environment is a separate
local operation. With the tools available:

```console
just check
just security
just container
```

`just check` runs privacy and version checks, formatting, strict Clippy, Rust
tests, Ruff, ty, and Home Assistant tests against the actual compiled Rust service.
`just container` builds and tests the native image, authentication, process
ownership, persisted credentials and checklists, completion history,
recurrence, restart, and a cold-volume backup/restore. CI runs the container
checks natively on amd64 and arm64.

For a standalone development service, create an owner token in your password
manager and supply it through `MAINTENANCE_API_TOKEN`, then run:

```console
cargo run --locked
```

The default bind is `127.0.0.1:8080`; `MAINTENANCE_DATABASE` selects the SQLite
path. Do not put tokens, real catalogs, or generated credentials in the
checkout. The API is independent of Home Assistant availability.

## Privacy and security

This repository contains fictional examples only. Keep credentials in a
password manager and personal records outside the repository. The
[public-data gate](scripts/check-public-tree) examines the current tree,
including hidden and untracked files, and every Git blob reachable from any
local ref, plus commit and tag metadata. It rejects common credentials and runtime files and reports only
paths and categories. Known build environments are excluded from the current
tree scan. Tracked files remain covered through history.

For a private comparison, set `HOME_MAINTENANCE_PRIVATE_CATALOG` to a catalog
outside this repository before running `just public-tree`. The gate looks for
matching strings without printing their values. Automated scanning complements
review; it cannot recognize every address, property nickname, or personal
fact. See [SECURITY.md](SECURITY.md).

MIT.
