# 10. Operations

- **O10.1** `serve` exposes `/health` (liveness) and `/ready` (DB
  connectivity) endpoints off the FHIR base paths, and Prometheus metrics on
  a separate configurable port (request counts/latencies by route,
  pool stats, per-resource-type row counts).
- **O10.2** Structured logging via `tracing` (JSON in production); every
  request gets a request id, echoed in `X-Request-Id`. Logs MUST NOT contain
  resource content (PHI) at default level.
- **O10.3** Connection pooling via deadpool; pool exhaustion returns 503
  with `Retry-After`, never queues unboundedly. Statement timeouts are set
  per pool connection.
- **O10.4** Schema migrations: `fhirpg_meta` records artifact versions;
  `fhirpg init --upgrade` applies generated migration DDL between artifact
  versions transactionally where possible, and refuses destructive changes
  without `--allow-destructive`. Every release documents its migration.
- **O10.5** TLS: production deployments terminate TLS at a fronting proxy,
  or in-process behind the `tls` feature (rustls). The server binds
  localhost by default; binding non-loopback requires explicit
  `--bind` acknowledgement. Authentication/authorization (SMART on FHIR,
  OAuth) is explicitly out of scope for the server core and delegated to the
  deployment perimeter; the spec requires documenting this boundary.
- **O10.6** Backup/restore is plain PostgreSQL (`pg_dump`/PITR); the book
  documents point-in-time recovery and the invariant that a consistent
  snapshot is always a valid fhirpg store.
- **O10.7** **The database connection carries PHI and MUST be encrypted.**
  fhirpg connects with rustls, honoring `sslmode` (`disable`, `prefer`,
  `require`, `verify-ca`, `verify-full`) from the DSN or `PGSSLMODE`, with
  `PGSSLROOTCERT` for the trust anchor. `verify-full` is the documented
  production setting. Starting with a non-loopback `--bind` over an
  unencrypted database connection MUST refuse unless
  `--allow-insecure-db` is passed: the two halves of the trust boundary are
  decided together or not at all.
- **O10.8** Resource limits are enforced at the edge, not only at the pool:
  a per-request timeout, a bounded concurrency limit, and a maximum
  in-flight request count, all configurable, all shedding load as 503 with
  `Retry-After` rather than queueing. Pool size is configurable
  (`FHIRPG_POOL_SIZE`), not a compiled-in constant.
- **O10.9** `/metrics` and `/health`/`/ready` MUST be servable on a separate
  bind address from the FHIR API (`--admin-bind`), so operational endpoints
  are not exposed to the same network as clinical data. Latency MUST be
  reported as a histogram, not a running total, so p99 is answerable.
- **O10.10** Releases ship supply-chain evidence: `cargo deny` (advisories,
  licenses, bans) and `cargo audit` in CI, a CycloneDX SBOM per release
  artifact, and checksums for every published binary. This is the IEC 62304
  / FDA cybersecurity expectation for a component handling clinical data,
  and it is cheap to keep green from the start.
- **O10.11** A published version MUST match the source that claims it. A
  crates.io version is immutable, so a tree carrying an already-published
  version number MUST be byte-identical to what was published, and CI MUST
  fail otherwise. Without the check the divergence is invisible: every local
  build resolves the path dependency and never fetches the registry copy, so
  the tree stays green while the artifact someone downloads is different
  code. It surfaces only when a third party packages a dependent, as an
  error about code they did not write. For a component handling clinical
  data, "the released artifact is the reviewed source" is the claim the whole
  audit trail rests on — O10.10's SBOM describes the artifact, and it is
  worth nothing if the artifact is not the source.

---

Part of the [fhirpg specification](index.md).
