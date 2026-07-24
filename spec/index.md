# fhirpg specification

This is the normative specification for fhirpg. Requirements are numbered and
use RFC 2119 keywords. Sections: [Scope](#1-scope), [Schema
generation](#2-schema-generation), [Storage model](#3-storage-model),
[Shredding and reconstruction](#4-shredding-and-reconstruction),
[Versioning and history](#5-versioning-and-history), [Search](#6-search),
[REST API](#7-rest-api), [CLI](#8-cli), [Validation](#9-validation),
[Operations](#10-operations), [Conformance testing](#11-conformance-testing).

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
