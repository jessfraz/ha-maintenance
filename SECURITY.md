# Security

Report vulnerabilities privately through
[GitHub security advisories](https://github.com/jessfraz/ha-maintenance/security/advisories/new).
Do not put a real catalog, Home Assistant URL, access token, address, NFC payload,
or completion history into a report or reproduction. Use fictional examples.

The bearer-authenticated service API and the Supervisor ingress listener are
separate boundaries. Only the configured Supervisor peer may authenticate a
request using ingress user headers. The packaged dashboard is administrator-only;
Supervisor enforces that permission before proxying it. The Python integration uses a separate
credential; the proxy permits only its explicit route allowlist. Browser
requests do not receive server credentials.

Keep the API on a trusted network or behind TLS. Do not expose the internal
ingress listener. Generated credentials are owner-readable files, and the
container drops root before opening the service state. Backups contain
credentials and personal item data and require the same access controls.

`scripts/check-public-tree` scans the working tree and all Git history reachable
from local refs, plus commit and tag metadata. It excludes known build environments in the current tree,
checks their tracked copies through history, and reports paths/categories
without printing possible secrets. Optional private-catalog comparison must
point outside the repository. Automated checks do not replace review for
personal or household information.
