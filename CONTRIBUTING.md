# Contributing

Home Maintenance is private for now, but the tree is kept ready to publish.

- Put no addresses, household inventory, credentials, NFC payloads, Home
  Assistant URLs, or real maintenance history in this repository.
- Keep example data fictional and use `example.com` URLs.
- Keep the JSON schema and example catalog in sync with catalog behavior.
- Run `just test` before proposing a change.
- Preserve the authenticated Home Assistant ingress boundary. Do not add an
  unauthenticated completion endpoint.

Changes to reconciliation or completion behavior need tests covering duplicate
tasks, failed Home Assistant writes, retries, and restore behavior. UI changes
need narrow and wide viewport checks in both color schemes.
