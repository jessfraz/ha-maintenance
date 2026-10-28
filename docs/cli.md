# Native API CLI

`ha-maintenance` is a one-request JSON client for the Rust service API or its
existing authenticated Home Assistant bridge, not a scheduler. It uses the
same request and response types as the service. It never starts the server,
opens the database, follows redirects, retries requests, fetches another page,
or generates idempotency keys.

## Install and connect

The owning flake exports `packages.<system>.ha-maintenance` and the default
package and app for aarch64 Darwin and aarch64/x86_64 Linux, matching the
existing dev shell systems. The pinned nixpkgs no longer supports Intel Darwin.

```sh
nix build .#ha-maintenance
nix run .#ha-maintenance -- --help
nix profile add .#ha-maintenance
```

For development, use `nix develop` and
`cargo run --locked --bin ha-maintenance -- --help`. Plain `cargo run --locked`
still runs the existing `home-maintenance` service.

The default `--transport native` selects `MAINTENANCE_API_URL` and
`MAINTENANCE_API_TOKEN`. Select `--transport home-assistant` to use
`HOME_ASSISTANT_URL` and `HOME_ASSISTANT_TOKEN` with the existing
`/api/home_maintenance/v1` bridge. Each transport reads only its own environment
variables, with no fallback to the other's origin or credential. `--url`
overrides the selected origin; `--token-file` overrides the selected token and
may end with a newline. There is no token command-line flag. The URL must be
an HTTP(S) origin without credentials, a path, query, or fragment. Use TLS outside
a trusted local network. Keep credentials and private input/output files outside
this checkout; obtain tokens from the existing
credential store rather than pasting them into shell history.

```sh
# With MAINTENANCE_API_URL and MAINTENANCE_API_TOKEN set by a credential wrapper:
ha-maintenance system
ha-maintenance items list --limit 50
ha-maintenance items get example-filter
# With HOME_ASSISTANT_URL and HOME_ASSISTANT_TOKEN set instead:
ha-maintenance --transport home-assistant items list --limit 50
```

Native `health` needs an origin but no credential. `--timeout` sets a total HTTP
deadline of 1–300 seconds, default 30. Native dedicated integration credentials
can explicitly select `--system-actor ha:integration` or `ha:automation`; the
service still checks the credential's authority. The HA bridge derives actor
and authority from the authenticated HA user, so `--system-actor` is rejected
for that transport. The CLI never supplies fake ingress or HA user headers.

The HA bridge supports item and reorder operations plus `summary`, `system`,
and `registry`. Health, imports, migrations, installation checks/status, and
integration reports are native-only. Selecting any native-only command with
HA transport fails locally with `unsupported_transport_operation` and zero
requests; it never silently switches transports. The bridge is not extended.

## JSON and concurrency

Success writes one JSON object to stdout with `status`, `etag` (the exact quoted
ETag or null), and `body` (the typed API response, or null for HTTP 204). List
bodies retain `items` and `next_page`. Pass that cursor explicitly with
`--page-token`; `--limit` accepts 1–200. Each invocation fetches exactly one page.
Do not change filters while continuing a cursor.

Commands with a JSON body accept `--input /path/to/request.json`, or stdin with
`--input -` (the default). Inputs are parsed as server-owned types before any
request. Unknown fields and malformed types are rejected as those types
specify. The server remains responsible for domain validation and authority.
Patch bodies preserve omitted fields versus explicit nulls.

```sh
# Fictional request; replace the revision/key with the caller's captured values.
printf '%s\n' '{"description":null}' |
  ha-maintenance items update example-filter --if-match '"7"'
```

Existing-resource mutations require `--if-match` with the captured quoted
revision. Create, completion, checklist, reorder, and migration apply also
require a caller-owned `--idempotency-key`. Completion and checklist inputs name
the exact `occurrence_id`. The CLI never reads a fresh revision before a write,
chooses an occurrence, or invents a key. Preserve the key, body, revision,
identity, and authenticated actor together when deciding whether to retry an
uncertain outcome. A request failure may mean the server already applied it.

Failures exit nonzero and write only JSON `status` (null if no response was
received) and `error_code` to stderr. Known server error codes are preserved;
unknown or non-JSON errors become `http_error`, and redirects become
`redirect_refused`. Request failures become `request_failed`. Sensitive input,
URLs, credentials, server messages, and response bodies are not diagnostics.
Invalid command arguments exit 2 with `invalid_arguments`; use `--help` for
syntax. Successful bodies contain the requested data and need private handling.

## Operations

See [the API reference](api.md) for authority, request schemas, and semantics.
Each command's `--help` lists its required arguments.

| CLI command | API operation |
| --- | --- |
| `health`, `summary`, `system` | Readiness, counts, credential/service status |
| `registry` | Read the HA device/entity catalog |
| `installation`, `installation-check` | Read installation status; check storage |
| `integration-presence`, `integration-registry` | Explicit dedicated-adapter reports |
| `items list [--archived]` | One page of active or archived items |
| `items get ID` | Read an item and its ETag |
| `items create` | Create a definition and occurrence |
| `items update ID` | Patch a definition |
| `items checklist ID STEP_UUID` | Check or uncheck the specified occurrence's step |
| `items complete ID` | Complete the specified occurrence |
| `items reorder ID` | Record a reorder request, with an empty-object input |
| `items events ID` | One page of completion history |
| `items archive ID`, `items restore ID` | Archive or restore an item |
| `items delete ID` | Permanently delete an archived item and its history |
| `import` | Atomically import a catalog of new definitions |
| `migrations preview`, `migrations apply` | Preview or apply a source-preserving migration |
| `reorders list [--status STATUS]` | One page of pending, fulfilled, or dismissed requests |
| `reorders get UUID`, `reorders resolve UUID` | Read or resolve a reorder request |

Migration preview is read-only. Apply is a separate explicit command, never an
automatic follow-up. Reorder records intent only: the service and CLI do not
fetch vendor links or purchase anything.
