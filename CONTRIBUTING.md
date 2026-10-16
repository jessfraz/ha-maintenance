# Contributing

Keep the service and Home Assistant adapter small and typed. Rust owns item
validation, recurrence, persistence, authentication, and the API contract.
Python projects that contract into Home Assistant. Use the existing routes,
entity patterns, and test fixtures before adding new abstractions.

Use the Nix dev shell and the declared Cargo and uv lockfiles. Run `just check`
and the checks appropriate to your change. Container changes also need
`just container`; CI tests native amd64 and arm64 images. New database queries
need representative SQLite `EXPLAIN` evidence. Completion changes need real
HTTP/SQLite tests for retries, concurrent mutations, and durable history.
Pairing changes need Home Assistant tests against the compiled Rust binary.

Examples and tests must be fictional. Never add addresses, property names,
household inventory, credentials, real URLs, NFC payloads, or maintenance
history. Run `just public-tree` before publication. Its output omits matched
values; keep that property when improving the scanner. See [SECURITY.md](SECURITY.md).

Keep version numbers consistent across Cargo, Python, the integration manifest,
and app configuration. `scripts/check-release-version vVERSION` checks a
release tag before publishing. Dependabot maintains weekly Cargo, uv, Docker,
Actions, and Nix updates. Actions are pinned by commit SHA. CodeQL and dependency
auditing run on pushes, pull requests, and weekly scans.
