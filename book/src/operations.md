# Operations

## Deployment posture

fhirpg handles PHI. The server binds loopback by default and implements
**no authentication** by design — the deployment perimeter (reverse
proxy, service mesh, or SMART-on-FHIR gateway) owns identity and
authorization. TLS terminates either at that perimeter or in-process via
the `tls` build feature (`--tls-cert`/`--tls-key`, rustls).

## Health, metrics, logs

- `/health` — liveness; `/ready` — database connectivity.
- `/metrics` — Prometheus text: request totals, response classes,
  cumulative latency.
- Every request gets an `X-Request-Id` (propagated when supplied) and one
  tracing line with method, path, and status. Resource content is never
  logged.

## Timeouts and load shedding

Server-side `statement_timeout` defaults to 30 s
(`FHIRPG_STATEMENT_TIMEOUT_MS`); pool waits are bounded at 2 s, and
exhaustion answers **503 + Retry-After** instead of queueing unboundedly.
`fhirpg serve` shuts down gracefully on SIGINT/SIGTERM.

## Install and upgrade

`fhirpg init` installs under a staging schema in chunked transactions and
renames it into place atomically — no `max_locks_per_transaction` tuning
required. It records the map checksum and the map itself; re-running is a
no-op, and a mismatched artifact is refused.

`fhirpg init --upgrade` diffs the installed map against the current
assets: new tables, columns, and indexes apply automatically; anything
destructive (dropped tables or columns) is refused without
`--allow-destructive`; column type changes always demand a manual
migration. `fhirpg drop --yes` removes a version schema in lock-safe
chunks.

## Backup

A fhirpg store is plain PostgreSQL: `pg_dump`, physical replication, and
point-in-time recovery all apply unchanged, and any consistent snapshot
is a valid store.
