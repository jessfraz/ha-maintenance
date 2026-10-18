# Maintenance

A local maintenance tracker for Home Assistant. Schedule work, keep checklists
and completion history, and link tasks to the devices and entities they belong to.

The Rust app stores data in SQLite. A Python integration adds a dashboard card,
to-do lists, sensors, actions, and a calendar to Home Assistant.

## Install

Install both the app and integration from this repository:

```text
https://github.com/jessfraz/ha-maintenance
```

1. Add the repository to the **Home Assistant App Store**, then install and start
   **Maintenance**.
2. Add the same repository to **HACS** as an **Integration**, download
   **Maintenance**, and restart Home Assistant.
3. Confirm the discovered connection in **Settings → Devices & services**.

See the [installation guide](home-maintenance/DOCS.md) for pairing, updates, and
backups. The app also runs [standalone with Docker](home-maintenance/DOCS.md#standalone-docker)
on amd64 and arm64.

## Tasks

Tasks can include a description, checklist, instructions, resource links, and
links to multiple Home Assistant devices or entities. Associations survive
entity renames. Completion records retain the checklist as it was when the work
was done; archiving a task preserves its history. Archived tasks can also be
permanently deleted, including their completion history.

| Type | Behavior |
| --- | --- |
| One-off | Closes when completed. The due date is optional. |
| Standing task | Stays active after completion, without a new due date. |
| Recurring | Calculates the next due date from when the work was completed. |
| Standing instruction | Cannot be checked off. Only the owner can edit or remove it. |

Recurring tasks require a first due date. The form defaults to today in the
task's time zone. Checklists reset for each new occurrence; completed history
stays unchanged.

## Home Assistant

Use the included [task-list card](home-maintenance/DOCS.md#dashboard-card) to open
tasks directly in Maintenance. Each row shows the task's current status and due date.

```yaml
type: custom:maintenance-task-list
title: Maintenance
view: active
limit: 100
```

The integration registers the card automatically. It also provides:

- A native task list and a read-only Instructions list when active instructions exist.
- A [read-only calendar](home-maintenance/DOCS.md#calendar) of current due dates.
- Sensors for active, overdue, and upcoming tasks, plus completion counts.
- Actions to check steps and complete work.
- [Local Assist commands](home-maintenance/DOCS.md#assist) for due dates and named task completion.

The app opens through authenticated ingress and requires a Home Assistant
administrator. Other users can complete work through the native to-do list and
integration actions. [Stable item links](home-maintenance/DOCS.md#nfc-tags) also
work with NFC tags.

## API

The service exposes a bearer-authenticated JSON API for tasks, checklists,
completion history, imports, and reorder requests. Migration endpoints provide
a dry run and preserve source IDs, known completion timestamps, and history.

- [API reference](docs/api.md)
- [OpenAPI schema](openapi/api.json)
- [Example catalog](config/items.example.json) and [migration](config/migration.example.json)

## Development

Use the Nix dev shell:

```sh
nix develop
just check
```

`just check` runs formatting, Clippy, Ruff, ty, privacy and version checks, and
Rust and Home Assistant tests. The integration tests use the compiled Rust
service. `just security` audits dependencies; `just container` tests the native
container and backup/restore flow with Docker. CI tests both amd64 and arm64.

To run the service locally, supply an owner token of at least 32 characters through
`MAINTENANCE_API_TOKEN` and run `cargo run --locked`. It binds to
`127.0.0.1:8080` by default; `MAINTENANCE_DATABASE` selects the SQLite path.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development conventions and
[SECURITY.md](SECURITY.md) for vulnerability reporting and data handling.
Examples are fictional; keep credentials and personal catalogs out of the checkout.

## License

[MIT](LICENSE).
