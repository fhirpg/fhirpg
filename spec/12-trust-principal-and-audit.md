# 12. Trust, principal, and audit

fhirpg does not authenticate users (plan.md D13) — but "authentication is
the perimeter's job" cannot mean "the record of who did what is nobody's
job". This section defines the seam: how an authenticated identity reaches
fhirpg, and what fhirpg guarantees about recording it.

- **PR12.1** The server accepts a **principal** from a configured trusted
  header (`--principal-header`, e.g. `X-Fhirpg-Principal`), and optionally a
  purpose of use (`--reason-header`) and an on-behalf-of patient
  (`--patient-header`). Values are length-capped and character-validated
  before use.
- **PR12.2** A principal header is trusted **only** when the request arrives
  from a configured trusted proxy (`--trust-proxy <cidr>…`). From anywhere
  else the header is ignored, not honored — otherwise any client could
  assert any identity.
- **PR12.3** `--require-principal` makes an unattributable request a 401.
  Deployments handling PHI are expected to set it; the book says so plainly.
  Without it, writes record `actor = 'unauthenticated'` and the server logs
  a startup warning.
- **PR12.4** Every state change records its principal in the history audit
  envelope (M3.15), in the same transaction as the change (never
  best-effort, never asynchronous).
- **PR12.5** Every **read** — read, vread, history, search, export — appends
  an access record to `<schema>.fhirpg_access_log(ts, request_id, actor,
  client, interaction, rtype, id, version_id, outcome, result_count,
  reason)`. Disclosure logging is the requirement regulators actually audit
  first, and a store that records only mutations cannot answer "who looked
  at this patient".
- **PR12.6** Access logging has three modes (`--audit-mode`):
  `sync` (the record commits before the response is sent — slowest,
  strongest, **the default**), `async` (batched through a bounded queue with
  a flush interval), and `off` (permitted only when `--allow-unaudited` is
  passed, and logged loudly at startup).

  `sync` is the default because the failure it prevents is the one that
  cannot be repaired afterwards: a disclosure with no record is
  indistinguishable, later, from a disclosure that never happened. A
  deployment that needs the throughput can opt into `async` knowingly; the
  reverse default would make every deployment silently accept a loss window
  it never chose. `async` MUST say at startup that records queued when the
  process dies are lost, and MUST drain its queue on graceful shutdown.

  In **every** mode a disclosure that cannot be recorded MUST fail closed —
  the read is refused, never served unlogged. A saturated queue is therefore
  a 503, not a dropped record.

  Four counters are exported per version, and the distinction between them
  is the point: `enqueued` and `written` describe a healthy path; `refused`
  counts reads turned away to keep the log honest; `lost` counts records the
  writer could not commit *after* the data was served. Non-zero `lost` is an
  incident — disclosures happened that the log does not show — while
  non-zero `refused` is the system working as designed under strain. Queue
  depth is derived from these rather than tracked separately, so it can
  never report a value the counters contradict.
- **PR12.7** fhirpg accepts the standard `X-Provenance` header on writes and
  stores the supplied `Provenance` resource, linking it to the version it
  describes. It MAY additionally synthesize `AuditEvent` resources from the
  access log on demand, so the audit trail is queryable as FHIR rather than
  only as SQL.
- **PR12.8** The trust boundary is stated in one place, in the book, as a
  table: what fhirpg guarantees, what the perimeter must provide
  (authentication, authorization, scope and compartment enforcement, consent,
  rate limiting per identity, TLS termination), and what neither provides
  yet. A boundary nobody can point at is not a boundary.

---

Part of the [fhirpg specification](index.md).
