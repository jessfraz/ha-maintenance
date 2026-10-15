# Installation

Install both parts:

- The **Maintenance app** runs the Rust service, SQLite database, and dashboard.
- The **Maintenance integration**, installed through HACS, adds sensors,
  actions, and a native to-do entity.

Use `https://github.com/jessfraz/ha-maintenance` for both repositories.

## Start the app

Add the repository in **Settings → Apps → App store → Repositories**, then
install and start **Maintenance**. No app options are required. The app saves
separate owner and integration credentials automatically.

Administrators can open the dashboard from the app page or enable **Show in
sidebar**. Supervisor enforces administrator access to the app dashboard.
Other Home Assistant users complete tasks through the native to-do entity or
integration actions. Home Assistant authenticates dashboard access through
ingress; server credentials stay out of the browser. The ingress listener
accepts only the Supervisor gateway.

## Pair the integration

1. Open **HACS → ⋮ → Custom repositories**.
2. Add `https://github.com/jessfraz/ha-maintenance` as an **Integration**.
3. Download **Maintenance**, then restart Home Assistant.
4. In **Settings → Devices & services**, confirm the discovered **Maintenance**
   integration.

Supervisor discovery supplies a separate integration credential. Internal app
networking requires no host port mapping. If discovery is absent, choose
**Add integration → Maintenance** and connect the Home Assistant app.
Reauthentication uses the same discovery path after credentials change.

Check the app's service status and the integration's availability after
pairing. The service reports a recent authenticated integration connection;
a running app alone does not establish that pairing succeeded.

## Add items

Use the dashboard or authenticated API to create an item. Give it a permanent
ID, a name, an action summary, time zone, due date, and recurrence. The name
identifies the thing, such as "Example air filter"; the summary describes the
work, such as "Replace the example air filter". A description, checklist,
instructions, specifications, resource links, group, location, and reorder link
are optional. Summaries must be nonblank and fit within 200 UTF-8 bytes.
Descriptions may contain up to 16,000 UTF-8 bytes and must be nonblank when
supplied.

Each checklist template has up to 100 steps with unique, stable UUIDs and
nonblank summaries of up to 200 UTF-8 bytes. Checking a step saves the actor
and time on the current occurrence. Unchecking it clears that step's
completion. Normal Home Assistant users can check steps and complete tasks;
only administrators edit item definitions.

An explicit catalog import uses `POST /v1/imports`. Start from the
repository's fictional `config/items.example.json`. Import is atomic and
rejects existing IDs; it is an admission operation rather than continuous
catalog reconciliation. Current schedules and history belong to SQLite.

Completing an item records the authenticated Home Assistant user when
available. Automations are recorded as system actions. The service schedules
the next occurrence atomically. Explicit completion is allowed with unchecked
steps: history records the checklist exactly as it was, and the new occurrence
starts with every step unchecked. A retry cannot silently complete another
occurrence. Concurrent edits report a conflict for review.

When editing a checklist template, keeping a step's UUID and exact summary
preserves its current completion. Renaming a step clears its current check;
new steps start unchecked. Removing or editing a template never rewrites
completed history. Archived items cannot be checked or completed.

API clients change a step with
`PUT /v1/items/{id}/checklist/{step_id}` and a body containing the current
`occurrence_id` and `completed` boolean. Supply the item's ETag in `If-Match`
and a stable `Idempotency-Key`, as for completing an occurrence. A stale
occurrence or revision is rejected rather than applying the change to new work.

## NFC tags

Open the item and copy its stable authenticated app route to the tag:

```text
https://home-assistant.example/app/<repository-app-slug>/items/example-air-filter
```

Temporary ingress session URLs expire. Keep the repository URL and item IDs
stable after programming tags. Archive retired items to preserve history.

## Standalone Docker

The same image runs without Home Assistant. Generate two distinct credentials
of at least 32 characters and save them in your password manager. Supply the
owner credential through `MAINTENANCE_API_TOKEN` and the dedicated Home
Assistant credential through `MAINTENANCE_INTEGRATION_TOKEN`:

```sh
docker run --detach --name maintenance --restart unless-stopped \
  --publish 127.0.0.1:8080:8080 \
  --mount type=volume,source=maintenance-data,target=/data \
  --env MAINTENANCE_API_TOKEN \
  --env MAINTENANCE_INTEGRATION_TOKEN \
  ghcr.io/jessfraz/ha-maintenance:0.1.0
```

The image supports amd64 and arm64. The server runs as uid 10001 and keeps its
database in `/data`. App mode also saves generated pairing credentials there. Use TLS when accessing the service across
an untrusted network.

To pair a standalone service, install the HACS integration, restart Home
Assistant, choose **Add integration → Maintenance**, then select the standalone
connection and supply its URL and the integration credential. Home Assistant must be able to
reach the service. The app's optional `8080/tcp` mapping exposes the
bearer-authenticated API; ingress port 8099 is internal.

## Backups and restore

The database is `/data/maintenance.sqlite3`; generated app credentials are in
`/data/credentials.json`. The app uses a cold backup so Supervisor stops it
before copying SQLite. Include both the app and Home Assistant configuration
to preserve the service state and paired integration together.

Treat backups as private. They contain item details, checklist state,
completion history, and credentials. For a manual backup, stop the container
cleanly before copying the complete `/data` volume. Restore that volume before starting the service.
CI verifies a cold-volume restore into a fresh container; this does not prove
that a particular Home Assistant backup has been restored successfully.

## Updates

A matching `vVERSION` release tag publishes the amd64 and arm64 image after
both native smoke tests pass. Update the app in the App Store and the
integration through HACS, then restart Home Assistant. Back up both parts
before upgrading.
