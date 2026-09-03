# Home Maintenance

Home Maintenance is a fast, private-by-default maintenance tracker for Home
Assistant. One JSON catalog drives the web interface and a Home Assistant to-do
list, so schedules, specifications, instructions, and reorder links cannot
drift between two systems.

The interface is designed for physical context. Open an item from Home
Assistant or tap an NFC tag, read its current state, then complete it in one
action. The home screen sorts every item by its next due date so the next useful
job is immediately visible.

## What it does

- Reconciles configured items into one Home Assistant to-do entity.
- Completes a task and schedules its next occurrence in one action.
- Records the authenticated Home Assistant user and completion time.
- Sorts overdue and upcoming work globally by due date.
- Shows exact and human-readable dates, cadence, interval progress,
  specifications, instructions, and reorder links.
- Uses server-rendered HTML, embedded CSS, and a tiny ingress routing bridge.
  There is no JavaScript framework or external font request between a tap and
  the page.

## Install as a Home Assistant App

The container image is not published yet. Once it is:

1. Add `https://github.com/jessfraz/home-maintenance` under **Settings -> Apps
   -> App store -> Repositories**.
2. Install **Home Maintenance**.
3. Create a Local To-do list for maintenance, or configure an existing to-do
   entity that supports creating and updating items with due dates and
   descriptions.
4. Put an `items.json` file at
   `/addon_configs/<repository>_home_maintenance/items.json`. Start from
   [`items.example.json`](config/items.example.json).
5. Set `config_file` to that filename in the app configuration and start it.
6. Open the app once and verify that each configured item has exactly one
   current occurrence.

The JSON catalog intentionally stays outside the repository and Home
Assistant's options form. After changing the canonical private file, copy it
byte-for-byte to the app's `addon_config` directory, verify its checksum, and
restart the app.

## NFC URLs

Program each tag with the authenticated Home Assistant panel URL shown when
you open that item:

```text
https://home-assistant.example/app/<repository-app-slug>/items/<item-id>
```

Do not copy a temporary ingress-session URL. Home Assistant handles
authentication and then opens the item directly, with no separate public
service or intermediate landing page.

Treat each `item.id` as permanent once a tag is programmed. Moving the
repository changes Home Assistant's generated app route, and renaming an item
ID changes that item's route.

## Data ownership

```text
items.json  -> identity, cadence, copy, specs, links
     |
     +-> web UI
     +-> Home Assistant to-do projection

history.db -> completion time, HA user, next due date
```

The reconciler manages only tasks carrying its hidden item marker or initially
matching an exact configured `ha_summary`. It does not edit unrelated tasks in
the same list. Duplicate matches stop reconciliation with an error instead of
deleting or guessing.

Before removing an item from JSON, complete or delete its current managed task
in Home Assistant and remove its NFC tag. Automatic deletion is deliberately
not implemented because a catalog typo must not erase household history or an
active task.

## Existing Home Assistant tasks

Do not start the app while old per-item recurrence automations are active:

1. Take a full Home Assistant backup.
2. Disable the old recurrence automations.
3. Resolve combined or renamed legacy tasks so each configured `ha_summary`
   has at most one matching active task.
4. Start the app and verify one marked active task per configured item.
5. Retire the obsolete legacy tasks.

The app stops on duplicate matches rather than guessing which occurrence owns
the maintenance history.

## Local development

Go 1.26 or newer is required.

```console
just test
cd home-maintenance
go run ./cmd/home-maintenance \
  -demo \
  -config ../config/items.example.json \
  -database /tmp/home-maintenance.db \
  -csrf-secret /tmp/home-maintenance-csrf.key \
  -listen 127.0.0.1:8099
```

Open `http://127.0.0.1:8099/`. Demo mode is in-memory and never connects to or
mutates Home Assistant.

For a local Home Assistant test, remove the published `image` key from
`home-maintenance/config.yaml` before copying the directory into `/addons`, so
Supervisor builds it locally instead of pulling GHCR. Published releases are
made only from a `v<config.version>` Git tag.

## Backups

Completion history lives in `/data/history.db`. The private catalog lives in
the app-specific `addon_config` mount. Home Assistant includes both when the
app is selected in a backup, and `backup: cold` stops the app before SQLite is
copied.

An app-only restore can reconstruct current occurrences from `items.json` and
`history.db`, but the configured Home Assistant to-do entity must already exist
on the destination system.

## Security

Production requests are accepted only from the Home Assistant Supervisor
ingress proxy. The app trusts ingress user headers only across that boundary
and uses the Supervisor token for Core API calls. Completion forms carry a
persistent HMAC token tied to the user, catalog item, and exact active task UID.

## License

MIT
