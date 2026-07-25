# fhirpg specification

This is the normative specification for fhirpg. Requirements are numbered and
use RFC 2119 keywords. Sections: [Scope](#1-scope), [Schema
generation](#2-schema-generation), [Storage model](#3-storage-model),
[Shredding and reconstruction](#4-shredding-and-reconstruction),
[Versioning and history](#5-versioning-and-history), [Search](#6-search),
[REST API](#7-rest-api), [CLI](#8-cli), [Validation](#9-validation),
[Operations](#10-operations), [Conformance testing](#11-conformance-testing),
[Trust, principal, and audit](#12-trust-principal-and-audit),
[Compliance mapping](#13-compliance-mapping).

## 1. Scope

- **S1.1** fhirpg MUST support FHIR R5 (5.0.0), R4 (4.0.1), and R3 (3.0.2).
  R5 is the default everywhere a version is optional.
- **S1.2** Each FHIR version's data lives in its own PostgreSQL schema:
  `r5`, `r4`, `r3`. Versions are independent; a database MAY host any subset.
- **S1.3** All resource types defined by the version's specification MUST be
  supported — no unsupported-type errors for spec-defined types.
- **S1.4** Target database is PostgreSQL 18. Features requiring ≥18 MAY be
  used; older servers are unsupported.

## 2. Schema generation

- **G2.1** DDL and relational maps MUST be generated from the official FHIR
  specification packages (StructureDefinitions, SearchParameters) by
  `fhirpg gen`, and the generated artifacts MUST be committed under
  `assets/` so that builds and installs never require the spec packages.
- **G2.2** Generation MUST be deterministic: same spec input → byte-identical
  output. `assets/CHECKSUMS.txt` records SHA-256 of every artifact.
- **G2.3** Identifier naming: element paths convert to snake_case
  (`birthDate` → `birth_date`). Table names concatenate the resource name and
  element path (`Patient.name.given` → `patient_name_given`).
- **G2.4** PostgreSQL truncates identifiers at 63 bytes. Where a generated
  name would exceed 63 bytes, the generator MUST abbreviate deterministically
  and, on residual collision, suffix with a 6-hex-digit hash of the full
  path. The full-path → identifier mapping MUST be recorded in the relational
  map and in a generated `doc/` index; two different paths MUST never map to
  the same identifier.
- **G2.5** `fhirpg init` MUST be idempotent and effectively atomic. Because
  creating thousands of tables in one transaction exceeds default PostgreSQL
  lock budgets (`max_locks_per_transaction`), init stages the install under
  a temporary schema (`r5__init`) in chunked transactions and then renames
  it into place in a single statement; a failed init leaves only the staging
  schema, which the next init removes. Init records the applied artifact
  checksum in `fhirpg_meta`, no-ops when the installed checksum matches, and
  refuses to run against a schema created from a different artifact (see §10
  migrations). Schema drops are likewise chunked.

## 3. Storage model

### Base tables

- **M3.1** Every resource type gets a base table named for the resource
  (`r5.patient`). Its primary key is `id text`.
- **M3.2** Base-table system columns: `id text PRIMARY KEY`,
  `version_id bigint NOT NULL` (monotonic per resource, starts at 1),
  `last_updated timestamptz NOT NULL`. `Resource.meta` is otherwise stored
  like any other element.
- **M3.3** Every scalar (non-repeating, primitive-typed) element of the
  resource becomes a typed column on the base table.

### Child tables

- **M3.4** Every **repeating** element becomes a child table. A child table
  carries:
  - `rid text NOT NULL` — the root resource id, FK to the base table with
    `ON DELETE CASCADE`,
  - `ords smallint[] NOT NULL` — the 1-based index at each repeating
    ancestor crossing from the resource root down to and including this
    element (`{2,1}` = second parent instance, first child instance),
  - primary key `(rid, ords)`,
  - typed columns for every scalar element reachable without crossing
    another repeating element.
  The array form (rather than one ordinal column per level) is what lets
  recursive elements (`Questionnaire.item.item`, via `contentReference`)
  share one table at any depth: recursion appears as longer `ords` paths.
- **M3.5** Non-repeating complex elements (datatypes and backbone elements)
  **flatten** into the nearest enclosing table as prefixed columns
  (`Patient.maritalStatus.text` → `patient.marital_status_text`); only their
  repeating descendants open tables. Three exceptions force a table for a
  non-repeating element, with a fixed ordinal of 1: (a) a flattened width
  that would approach PostgreSQL's 1600-column limit (generator threshold
  150 columns — this catches the open `value[x]` choices with ~54 types),
  (b) backbone elements targeted cyclically by a `contentReference`
  (`ImplementationGuide.definition.page`), and (c) nothing else. There are
  no shared "coding" tables; each usage site owns its rows.

### Type mapping

- **M3.6** FHIR primitive → PostgreSQL column types:

  | FHIR | PostgreSQL |
  | --- | --- |
  | boolean | `boolean` |
  | integer, unsignedInt, positiveInt | `integer` |
  | integer64 (R5) | `bigint` |
  | decimal | `numeric` — original textual precision MUST survive round-trip |
  | string, code, id, markdown, uri, url, canonical, oid, uuid, xhtml, base64Binary | `text` |
  | date | `text` + derived `date` column `<name>_sort` |
  | dateTime, instant | `text` (verbatim) + derived `timestamptz` column `<name>_sort` for ordering/search |
  | time | `text` (fractional-second lexical fidelity) |

  Partial dates ("2026", "2026-07") make FHIR temporal values
  non-representable in native types without loss, hence verbatim text plus a
  derived sort column, computed by the engine at write time (partial values
  sort at their period start; offset-less dateTimes sort as UTC).
- **M3.7** Elements bound `required` to a FHIR value set get a
  `CHECK (col IN (…))` constraint generated from the code system; other
  binding strengths are unconstrained columns.

### Choice elements

- **M3.8** A choice element `value[x]` becomes one column (or child table,
  for complex types) per allowed type — `value_boolean`, `value_quantity_…` —
  plus a generated `CHECK` that at most one alternative is populated.

### References

- **M3.9** A Reference element stores: `<name>_ref_type text`,
  `<name>_ref_id text` (parsed from relative literal references),
  `<name>_ref_url text` (absolute/other references, verbatim), plus columns
  for `display` and expanded `identifier`. Parsing MUST be reversible: the
  original `reference` string reconstructs exactly.
- **M3.10** Referential integrity across resources is NOT enforced by
  foreign keys (FHIR permits dangling references). `fhirpg` MAY offer an
  advisory integrity report; it MUST NOT reject writes for dangling refs.

### Extensions and primitive extensions

- **M3.11** Extensions are stored relationally as **typed leaf rows** in one
  generated table per resource type:
  `<resource>_ext(rid, path, ords, modifier, ext_ord, url, leaf, v_kind,
  v_text, v_num, v_bool)`, PK (rid, path, ords, modifier, ext_ord, leaf).
  `path`/`ords` locate the attach point (dotted JSON-name path, "" for the
  resource itself; ordinals at each repeating crossing). `ext_ord` is the
  1-based index in the extension array (`modifier` distinguishes
  modifierExtension); `url` is the top-level extension url, denormalized for
  querying. `leaf` addresses one scalar inside the extension's content as a
  dotted path whose all-digit segments are 0-based array indexes
  (`valueCodeableConcept.coding.0.code`); nested extensions are ordinary
  leaves (`extension.0.valueString`). `v_kind` ∈ s/n/b/z tags the JSON
  scalar kind; numbers keep their lexical form in `v_text` and a queryable
  `numeric` in `v_num`. This one uniform encoding covers every extension
  value type — including arbitrarily nested complex values — with no
  JSONB and no per-type tables.
- **M3.12** Primitive extensions (`_birthDate` etc.) reuse M3.11 with the
  primitive's path (and the entry index, for repeating primitives);
  element ids ride the same table as `ext_ord = 0, leaf = 'id'` rows.
  Reconstruction MUST re-emit the `_field` form exactly, including null
  padding in parallel arrays.
- **M3.13** `Resource.contained` resources are stored in a per-resource
  table `<resource>_contained(rid, ord, resource jsonb)`. Elements typed
  `Resource` (Bundle.entry.resource, Parameters.parameter.resource) become
  jsonb columns the same way. These are the sanctioned JSONB usages besides
  history (plan.md D7): such values are anonymous whole resources of
  unknowable type, so normalizing them buys nothing.
- **M3.14** The FHIR type graph contains one true datatype cycle:
  `Reference.identifier: Identifier` and `Identifier.assigner: Reference`.
  Static expansion cuts a cycle at the element that would re-enter an
  in-expansion type (`….identifier.assigner`), and stores anything below the
  cut as leaf rows (M3.11 encoding, minus extension columns) in a
  per-resource `<resource>_deep(rid, path, ords, leaf, v_kind, v_text,
  v_num, v_bool)` table — lossless, relational, and vanishingly rare in
  real data.

### Audit columns

- **M3.15** Every `<resource>_history` table carries, besides H5.1's
  columns, an **audit envelope**: `actor text` (the authenticated principal
  responsible for the change, or `'unauthenticated'`), `actor_source text`
  (how the principal was established, e.g. `header:X-Fhirpg-Principal`),
  `client text` (source address as the server observed it), `request_id
  text` (the value echoed in `X-Request-Id`), and `reason text` (a
  caller-supplied purpose of use, when given). These columns are written by
  the same statement that appends the history row, inside the same
  transaction as the data change — an audit record that can be lost
  independently of the change it describes is not an audit record.
- **M3.16** History is **tamper-evident**. Each history row carries
  `prev_hash bytea` and `row_hash bytea`, where `row_hash` is SHA-256 over
  the row's canonical serialization concatenated with the `prev_hash` of the
  previous version of the same resource id (the first version chains from 32
  zero bytes). Chains are per resource id, so appends stay concurrent.
  `fhirpg verify-audit` MUST recompute every chain and report the first
  break.
- **M3.17** History is **append-only in the database, not merely by
  convention**. `fhirpg init` MUST emit a `BEFORE UPDATE OR DELETE` trigger
  on every history table that raises an exception, and the book MUST
  document the `REVOKE UPDATE, DELETE` grants a deployment applies to the
  application role. Escaping this is then a deliberate DBA act, never an
  application bug.
- **M3.18** Erasure (GDPR Art. 17) is the one sanctioned exception, and it
  is explicit: `fhirpg purge <Type> <id> --reason <text>` removes the
  resource's history rows and replaces them with a single tombstone row
  recording who purged what, when, why, and the `row_hash` chain it
  terminated — so an erased record leaves a verifiable hole rather than a
  silent one. Purge requires `--allow-erasure` and is logged at warn level.

## 4. Shredding and reconstruction

- **R4.1** Shredding (JSON → rows) and reconstruction (rows → JSON) are
  driven by the generated relational map through one generic engine; no
  per-resource handwritten code.
- **R4.2** Round-trip MUST be lossless: for any valid resource,
  `reconstruct(shred(r))` is semantically identical JSON — same values
  (including decimal precision and partial dates), same array order, key
  order not significant. This invariant is enforced by property tests over
  spec examples and generated resources.
- **R4.3** Unknown elements (not in the version's spec) MUST be rejected
  with an error naming the path — never silently dropped.
- **R4.4** A resource write (shred + delete-old-rows + insert) MUST be a
  single transaction.
- **R4.5** A resource **read** MUST likewise see a single snapshot. A read
  touches one base table and many child tables; issued as independent
  statements, a concurrent write between them would reconstruct a resource
  that never existed — base columns from one version, child rows from the
  next. Every multi-statement read (`get`, `export`, search result
  materialization) MUST therefore run inside one `REPEATABLE READ READ ONLY`
  transaction. This is a correctness requirement, not a tuning knob.
- **R4.6** Resource ids MUST satisfy the FHIR `id` production
  (`[A-Za-z0-9\-\.]{1,64}`) wherever they enter the system — URL path, body,
  or Bundle entry. An id that does not is a 400, never a stored row.

## 5. Versioning and history

- **H5.1** Every create/update/delete increments `version_id` and appends
  one row to `<resource>_history(id, version_id, last_updated, op char(1),
  resource jsonb)` where `op` ∈ C/U/D. History is an immutable audit
  archive; JSONB is acceptable there because it is written once and read
  only by vread/history/audit (decision D7, plan.md).
- **H5.2** Delete is soft at the API level (history row with op = D; base
  and child rows removed); a deleted id's history remains readable.
- **H5.3** vread serves any historical version from history; read serves the
  current version reconstructed from the relational tables. A checksum
  comparison between the two paths is part of the test suite, not runtime.

## 6. Search

- **P6.1** All standard SearchParameters of each version MUST be compiled by
  the generator into SQL predicate templates over the normalized columns.
  Search types supported: token, string, date, number, quantity, reference,
  uri; composite and special parameters MAY be deferred (documented per
  parameter in generated docs).
- **P6.2** String search default is case-insensitive prefix match
  (`:exact` and `:contains` modifiers supported). Token search matches
  `system|code` semantics. Date search implements FHIR range/prefix
  semantics (eq, ne, lt, gt, ge, le, sa, eb) against the `_sort` columns
  with precision-aware ranges.
- **P6.3** Result parameters: `_count` (default 50, max 1000), paging via
  opaque cursor, `_sort` on searchable params, `_id`, `_lastUpdated`,
  `_total=accurate|estimate`, `_include`/`_revinclude` (single hop).
- **P6.4** The generator MUST emit indexes for: every base-table search
  column, every child-table FK + ord, reference `(ref_type, ref_id)` pairs,
  and token `(system, code)` pairs.
- **P6.5** Unsupported search parameters MUST return an OperationOutcome
  warning and be ignored per FHIR's lenient handling, or error under
  `Prefer: handling=strict`.
- **P6.6** String search MUST be insensitive to **case, accents, and
  Unicode composition** — FHIR requires it, and a system serving Ærø,
  Ångström, Müller, Muñoz, and Ślusarczyk cannot ship `ILIKE` alone. Each
  string search target column gets a companion `_norm` column holding the
  folded value, **computed by the engine in Rust at write time** (NFD, drop
  combining marks, lowercase, drop marks again). Queries fold the search
  term with the same function and compare against that column, so there is
  exactly one definition of string equality in the system rather than one in
  SQL and one in Rust that must agree for every codepoint. The column is
  declared `COLLATE "C"`, so ordering is by Unicode codepoint.

  A prefix search MUST be emitted as a **range predicate** — `col >= term
  AND col < upper(term)`, with the upper bound computed in Rust — not as
  `LIKE $1 || '%'`. PostgreSQL extracts a prefix from a *constant* pattern
  only, so a `LIKE` against a bound parameter degrades to a sequential scan
  in the generic plan while looking correct in any hand-run `EXPLAIN` with a
  literal. `:exact` compares the stored column, not the folded one, because
  it is defined as the literal string. `:contains` folds both sides and
  remains a scan, as an unanchored match must.

  This fixes P6.2's semantics and T15's unindexed-prefix note in one move,
  and requires **no PostgreSQL extension**: an earlier design built on
  `unaccent` needed an IMMUTABLE wrapper, an expression index the planner
  would not use with a parameterized pattern, and a deployment-time check
  for an extension that managed-Postgres tenants often cannot install.
- **P6.7** A single search request MUST have a bounded cost. Result
  materialization MUST batch (one query per resource type, not one per
  resource); `_include`/`_revinclude` expansion MUST be capped and, when the
  cap truncates, MUST add an OperationOutcome warning to the bundle. Silent
  truncation of clinical results is a patient-safety defect, not a
  performance trade.

## 7. REST API

- **A7.1** The server mounts each installed version at `/{r3|r4|r5}` and
  implements: `GET  {base}/metadata` (CapabilityStatement),
  instance `GET/PUT/DELETE {base}/{type}/{id}`, `GET …/_history` and
  `GET …/_history/{vid}`, type-level `POST {base}/{type}` (create) and
  `GET {base}/{type}` (search, also via `POST …/_search`), and system
  `POST {base}` for batch and transaction Bundles.
- **A7.2** JSON is the required format (`application/fhir+json`); XML is
  available behind the `xml` feature via the fhir crate.
- **A7.3** Concurrency: responses carry `ETag: W/"{version_id}"`; PUT and
  DELETE honor `If-Match` and MUST return 412 on mismatch. Conditional
  create/update/delete via `If-None-Exist` and conditional references in
  transactions MUST be supported.
- **A7.4** Transactions are a single database transaction with FHIR
  processing order (DELETE, POST, PUT, GET), urn:uuid reference resolution,
  and all-or-nothing semantics. Batch entries are independent.
- **A7.5** Every error is an OperationOutcome with correct HTTP status
  (400 malformed, 404 unknown id/type, 405, 409/412 version conflict,
  410 deleted, 422 rejected resource, 500 with an opaque incident id —
  internal detail goes to logs, never to clients).
- **A7.6** Request bodies are capped (default 32 MiB, configurable);
  overlong URLs and pathological search inputs are rejected 414/400.
- **A7.7** Absolute URLs the server emits (`Bundle.entry.fullUrl`, `link`,
  `Location`) MUST come from a **configured** service base URL
  (`--base-url`), not from the request's `Host` header. `Host` and
  `X-Forwarded-Proto`/`-Host` are honored only when `--trust-proxy` is set,
  and then only for hosts on an allowlist. Deriving links from an
  attacker-controlled header lets a caller poison the next-page URLs that a
  client will follow.
- **A7.8** Responses that can carry PHI MUST set `Cache-Control: no-store`
  and `Pragma: no-cache`; every response MUST set
  `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`.
  Cross-origin access is denied unless `--cors-origin` names an origin
  explicitly; there is no permissive default.
- **A7.9** `Bundle.entry.request.ifMatch`, `ifNoneExist`, and
  `ifModifiedSince` MUST be honored inside batch and transaction entries, or
  the entry MUST fail with 501. Accepting a precondition and ignoring it is
  worse than not supporting it: the client believes it has concurrency
  control that it does not have.
- **A7.10** Conditional interactions (`If-None-Exist`, conditional update,
  conditional delete) MUST be atomic with respect to concurrent requests
  with the same criteria: the match and the write happen in one transaction,
  serialized by a transaction-scoped advisory lock on the criteria. A
  search-then-write that races produces duplicate patients.
- **A7.11** Client-visible diagnostics MUST identify *what rule was broken
  and where* (element path, rule id, parameter name). They MAY quote the
  caller's own query terms — the caller just sent them — but MUST NOT
  contain stored resource content, schema detail, or database text. Internal
  error strings go to the log behind an incident id (A7.5) and do not reach
  the wire. The store's error type MUST make this distinction structurally,
  so that "safe to return" is a type, not a habit.
- **A7.12** The generated CapabilityStatement MUST declare what is actually
  implemented — `conditionalCreate`, `conditionalUpdate`, `conditionalDelete`,
  `searchInclude`, `searchRevInclude`, `readHistory`, `versioning`, the
  `security` block naming the deployment's authentication, and the
  interaction list per resource. An over-claiming CapabilityStatement is a
  conformance defect (T11.4).

## 8. CLI

- **C8.1** Commands: `init`, `load`, `export`, `transform`, `serve`, `gen`.
  Global flags: `--fhir-version {r3|r4|r5}` (default r5), PostgreSQL
  connection via standard `PG*` environment variables or `--dsn`.
- **C8.2** `load` accepts NDJSON, Bundle JSON, or single-resource JSON,
  gzipped or plain, detected by content not filename; memory use is bounded
  by the largest single resource. Bad resources are reported with file, line
  and path; `--strict` stops on first error, default skips-and-reports;
  the exit code is nonzero if any resource failed.
- **C8.3** `transform` prints, for one input resource, every row it would
  produce as (table, columns) — the debugging window into the storage model.
- **C8.4** `export` streams NDJSON of reconstructed current resources,
  optionally filtered by type; output round-trips through `load`.

## 9. Validation

- **V9.1** Structural validation (element existence, cardinality, primitive
  lexical rules, choice exclusivity, required bindings) always runs — it is
  inherent to shredding against the map.
- **V9.2** `--validate` (CLI) / `X-Fhirpg-Validate: strict` (server config
  default-on) additionally deserializes through the typed `fhir` crate model
  for the resource's version and rejects on any mismatch.
- **V9.3** Validation failure at the API returns 422 with an
  OperationOutcome listing each issue with a FHIRPath-style location.

## 10. Operations

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

## 11. Conformance testing

- **T11.1** Round-trip property tests (R4.2) over every example resource
  shipped with each FHIR specification, plus proptest-generated resources.
- **T11.2** Live-database integration tests exercise every REST interaction
  in §7 against PostgreSQL 18 in CI (docker compose).
- **T11.3** Search semantics tests derive cases from the FHIR search
  specification per parameter type, including precision-edge dates and
  token system matching.
- **T11.4** The CapabilityStatement MUST be generated from what is actually
  implemented (the relational map + supported params), never hand-edited.
- **T11.5** Load/serve benchmarks are tracked in `doc/benchmarks.md`; a
  regression gate compares against the recorded baseline.
- **T11.6** Concurrency is tested adversarially, not assumed: a reader
  looping against a writer MUST never observe a torn resource (R4.5); N
  racing conditional creates with identical criteria MUST produce exactly
  one resource (A7.10); N racing `If-Match` updates MUST produce exactly one
  success and N-1 412s.
- **T11.7** A redaction test asserts that no log line emitted during a full
  CRUD + search cycle over a resource containing a distinctive marker value
  ever contains that marker (O10.2), and that no OperationOutcome on the
  wire echoes a submitted value (A7.11).
- **T11.8** An audit test asserts that every write records its principal
  (M3.15), that every read appends an access record (PR12.5), that the hash
  chain verifies (M3.16), and that a direct `UPDATE`/`DELETE` on a history
  table is rejected by the database (M3.17).

## 12. Trust, principal, and audit

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

## 13. Compliance mapping

fhirpg is a component, not a certified system: it cannot make a deployment
compliant, but it must not be the reason a deployment cannot be. This table
maps the obligations that shape the requirements above, so a reviewer can
trace a regulation to a numbered requirement to a test.

| Obligation | Requirements | Evidence |
| --- | --- | --- |
| HIPAA §164.312(b) audit controls | M3.15, PR12.4, PR12.5 | T11.8 |
| HIPAA §164.312(c) integrity | M3.16, M3.17, R4.4, R4.5 | T11.6, T11.8 |
| HIPAA §164.312(e) transmission security | O10.5, O10.7, A7.8 | live TLS smoke test |
| HIPAA §164.502 minimum necessary | PR12.1, PR12.8 (perimeter) | boundary table |
| GDPR Art. 17 erasure | M3.18 | purge test |
| GDPR Art. 30 records of processing | PR12.5, PR12.7 | T11.8 |
| GDPR Art. 32 security of processing | O10.7, O10.8, O10.10 | CI gates |
| ONC/HTI FHIR conformance | A7.12, T11.4, §9 validation | Inferno run |
| ONC/HTI Bulk Data | (M8) `$export` | Inferno run |
| IEC 62304 §5–8 lifecycle | spec ↔ tasks ↔ test traceability | this document |
| IEC 62304 / FDA cybersecurity | O10.10 (SBOM, advisories) | release artifacts |

Two gaps are deliberate and stated rather than papered over:
authorization (scopes, compartments, consent, `meta.security` label
enforcement) lives at the perimeter (PR12.8), and terminology validation is
out of scope until a terminology service is integrated (§9).
