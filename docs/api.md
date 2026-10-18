# API reference

The service embeds its UI and exposes a versioned JSON API. See the
[OpenAPI schema](../openapi/api.json) for request and response types, or generate
a fresh copy with `just openapi`.

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
| Permanently delete an archived item and its history | `DELETE /v1/items/{id}` |
| Counts and authenticated service status | `GET /v1/summary`, `GET /v1/system` |
| HA device/entity catalog (administrators) | `GET /v1/home-assistant/registry` |

Use bearer authentication for `/v1` routes. Mutations of existing items require
`If-Match` with the item's ETag. Creation, step changes, and completion require
a stable `Idempotency-Key`. Repeating an accepted request returns its saved
result; reusing a key for a different request is a conflict. Completion also
names the exact current occurrence, so a second tap cannot complete the next
occurrence. A step change sends `{ "occurrence_id": "UUID", "completed": true }`
or `false` to undo it. Authenticated users can change steps and complete items;
editing definitions requires administrator access. Archived items and stale
occurrences cannot be checked or completed. Lists use bounded cursor pagination.

## Items and imports

The API represents task types as `{"kind":"once"}`, `{"kind":"standing"}`,
`{"kind":"recurring","interval":{"months":3}}`, or `{"kind":"instruction"}`
in `schedule`. A newly created item defaults to one-off when its schedule is
omitted. The form supplies today's date in the selected time zone when choosing
recurring work. API clients must supply a concrete `due`; missing or null dates
return **400**, including catalog imports and edits that would leave a recurring
task undated. One-off and standing tasks retain an omitted or null due date.

Standing instructions have no due date, checklist, reorder, or completion.
Creating, editing, archiving, restoring, permanently deleting, or changing their
type requires the owner. A standalone owner credential has the same authority
as the Home Assistant owner.

Due dates range from `0001-01-01` through `9999-12-30`, leaving a representable
following day for each calendar event's exclusive end.

Import accepts `{ "items": [...] }` with the same item shape as creation.
[`config/items.example.json`](../config/items.example.json) is entirely fictional;
[`config/items.schema.json`](../config/items.schema.json) describes that format.
The import validates every item and commits the whole catalog atomically.
Existing IDs conflict. Omitting an item never deletes or archives it.
The required `summary` and each checklist step's summary must be nonblank and
fit within 200 UTF-8 bytes. An optional description fits within 16,000 UTF-8
bytes and must be nonblank when supplied. A checklist defaults to empty and
supports up to 100 steps, each with a unique UUID that remains stable when
editing its text.

## Permanent deletion

Archive an item before calling `DELETE /v1/items/{id}`. This requires
administrator access and the archived item's quoted revision in `If-Match`.
Active items return **409**; missing, malformed, and stale revisions return
**428**, **400**, and **412**. Standing instructions require owner access.

A successful **204** removes the item and its entire completion history in one
transaction. Repeating the delete with the same revision returns **204**, so a
lost response can be retried safely. A different revision returns **412**;
an unknown ID returns **404**. Reading a deleted item or its history returns
**404**. Reorder requests and their receipts remain available.

Deleted IDs stay reserved. Creating or importing that ID returns **410**.
Saved creation, checklist, and completion responses for the deleted item have
their payloads removed; exact retries return **410** and changed requests
using the same key still return **409**. Other items and receipts are unchanged.

Migration source mappings remain reserved too. A new migration preview reports
a conflict for a deleted target or source, and applying it returns **409**.
Replaying an accepted migration key returns its original mapping report as a
historical acknowledgement; it does not recreate the deleted item.

## Home Assistant links

Selected references use stable registry IDs scoped to the Home Assistant
instance. Entity renames retain the association; missing or disabled objects
retain their saved references.

Only selected references are stored in SQLite. The integration supplies a
bounded registry catalog in memory, refreshed every minute and after registry
changes. It contains display names, IDs, device relationships, and disabled
flags; entity states and hardware attributes stay in Home Assistant. A stale
or unavailable catalog prevents choosing new links while existing references
remain editable.

## Migrating existing tasks

Catalog admission through `/v1/imports` creates new definitions only. Use the
separate migration endpoints to retain existing task state and history.
[`config/migration.example.json`](../config/migration.example.json) is a fictional
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

## Reorder requests

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
