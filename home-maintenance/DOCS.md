# Home Maintenance

## Configuration

```yaml
config_file: items.json
```

`config_file` is resolved inside this app's `addon_config` directory. Copy the
repository's `config/items.example.json` to that directory as `items.json`, then
replace the examples with private household data.

On the Home Assistant host, the file is
`/addon_configs/<repository>_home_maintenance/items.json`. `<repository>` is
`local` for a local app or the generated identifier for the repository that
provided the app. Keep the source catalog somewhere private, deploy an exact
copy, verify the two checksums, and restart this app after every catalog change.
The catalog is loaded once when the process starts.

The configured `todo_entity` must already exist. Create a Local To-do list in
Home Assistant, or use another to-do integration that can create and update
items with due dates and descriptions, before starting this app.

The catalog is the source of truth. At startup and every 15 seconds, the app:

1. adopts an existing active todo with the exact configured `ha_summary`;
2. adds a hidden stable item marker to the todo description;
3. repairs its summary, due date, and description from catalog/history state;
4. creates a missing current occurrence when an initial or computed due date is
   available.

Completing a retained managed item directly in Home Assistant is detected and
scheduled on the next sync. Home Assistant does not expose the responsible user
through the todo API, so those events are attributed to `Home Assistant` rather
than inventing a person. The NFC app records the actual ingress user.

Unrelated todo items are not changed. Multiple active matches stop the sync and
surface an error in the UI so the app never chooses or deletes one silently.
Changing an item's cadence recalculates its current due date from the most
recent completion, so the JSON catalog remains authoritative after edits too.

## NFC tags

Open the item from this app and copy the browser address to the NFC tag. The
address must use Home Assistant's `/app/.../items/<item-id>` route, not an
ingress session URL. The former is stable and authenticated; the latter is
temporary.

The repository URL participates in Home Assistant's app slug. Finalize that URL
before programming tags, and treat each JSON `item.id` as immutable afterward.

## Migrating existing tasks

Take a full backup and disable old recurrence automations before the first
start. Split any combined legacy task into the independently replaceable items
represented by the catalog. Rename or retire legacy tasks so each configured
`ha_summary` has no more than one active match. Start the app, verify one marked
active todo per item, and only then remove obsolete tasks.

To retire a managed item later, first complete or delete its current todo and
remove its NFC tag, then remove it from JSON. The app intentionally never
auto-deletes a task merely because an item disappeared from the catalog.

## Completion history

History is stored in `/data/history.db` and included in cold app backups.
Existing Home Assistant todos do not reliably preserve who completed older
occurrences, so the app reports history beginning with the first completion
recorded here.

The private catalog uses the app-specific `addon_config` mount, which Home
Assistant also includes when that app is selected in a backup. Restoring the app
therefore restores both inputs needed to reconstruct the system: `items.json`
and `history.db`. The app uses `backup: cold`, so Supervisor stops it before
copying SQLite and starts it again afterward.

A full Home Assistant backup also restores the Core-side to-do entity. An
app-only restore reconstructs current occurrences from `items.json` and
`history.db`, but the configured to-do entity must exist on the destination
system first.
