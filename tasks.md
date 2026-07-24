# fhirpg tasks

Work breakdown for the plan's milestones. Each task lists its acceptance
criterion. Order within a milestone is roughly dependency order.

## M1 — Engine proven (R5 vertical slice)

- [x] **T1 Workspace scaffold.** Cargo workspace per plan D14
  (`fhirpg-map`, `fhirpg-gen`, `fhirpg-store`, `fhirpg` — the server crate
  arrives with M4), CI (fmt, clippy, test, live-PG job).
  *Done:* `.github/workflows/ci.yml`; tests self-skip without inputs.
- [x] **T2 Spec-package ingestion.** profiles-resources.json +
  profiles-types.json parsed directly (simpler than reusing the fhir
  crate's parser; that crate still backs `--validate` later) into element
  trees with cardinality, types, choice and contentReference info — for all
  three versions, not just R5. SearchParameters ingestion moves to M3.
- [x] **T3 Relational map format.** `fhirpg-map::model`: node arena (cycles
  via indexes), tables, typed columns, choice variants, reference splits,
  extension/spill channels, 63-byte registry with deterministic
  abbreviation + hash fallback. Assets: `assets/fhirpg-relmap-{r3,r4,r5}
  .json.gz` + CHECKSUMS.txt.
- [x] **T4 DDL generator + scale spike.** Full R5 = 7,355 tables installs
  in 9.5 s via staged-schema + rename (single-transaction DDL exhausts
  PostgreSQL's lock budget — G2.5 amended). Numbers in doc/benchmarks.md;
  risk R1 retired. *Remaining for M3:* search indexes; value-set CHECKs
  (M3.7) not yet emitted.
- [x] **T5 Shredder.** Generic walker: scalars, ords-array child tables,
  choices (incl. force-split wide choices), reference parsing, extension
  leaf rows, primitive extensions with null-padded arrays, element ids,
  contained, type-cycle spill, unknown-element rejection.
- [x] **T6 Reconstructor.** Inverse walker with consumption auditing (every
  row must be used exactly once — gaps surface as integrity errors, never
  silent loss). *Accept exceeded:* the entire three-version corpus
  (7,399 examples) round-trips in memory.
- [x] **T7 Store layer: init/load/read.** tokio-postgres + deadpool;
  transactional put with history append; pipelined multi-table reads;
  chunked multi-row inserts; text-image wire protocol with explicit casts.
  *Accept:* full-corpus live round trip 7,396/7,396 across r3/r4/r5;
  bulk benchmark: 6,146 res/s load, 1.18 ms reads (doc/benchmarks.md).
- [x] **T8 CLI v1.** `gen`, `init`, `load` (NDJSON/Bundle/single, gzip,
  content-detection, per-resource error reporting, nonzero exit), `get`,
  `delete`, `export`, `transform`. *Remaining:* streaming (bounded-memory)
  reads for multi-GB NDJSON — currently whole-file.
- [x] **T9 Round-trip property tests.** Map-driven random-resource
  generator (deterministic SplitMix64 seeds — no proptest dependency):
  deep recursion, sparse primitive arrays with extensions, nested
  extensions, choice variants, decimals, partial dates. 10k cases pass
  (`FHIRPG_PROPTEST_CASES`; default 500 locally).
  *Found a real bug the 7,399-example corpus missed:* two cyclic
  contentReferences into one table (QuestionnaireResponse `item.item` +
  `item.answer.item`) made ordinal paths ambiguous — fixed with ordinal
  sign lanes (`Elem::neg_lane`); the reconstructor's consumption audit is
  what caught it.

## M2 — History + CRUD semantics

- [x] **T10 version_id + history tables.** H5.1–H5.3 in the store:
  history append on C/U/D, soft delete, `vread`, `history`, and `status`
  (the 404-vs-410 distinction); version numbering continues past deletes
  (derived from history max, not the base row).
  *Accept met:* m2_semantics integration test — create→update→delete shows
  D/U/C history, vread of each version matches, deleted reads as Deleted.
- [x] **T11 Optimistic concurrency.** `put_if(resource, expected_version)`
  under FOR UPDATE row locks; `StoreError::Conflict` for the API's 412;
  expected 0 = create-only (If-None-Exist shape). *Accept met:* two racing
  conditional writers — exactly one wins.
- [x] **T12 fhirpg_meta + idempotent init.** Staged-schema install +
  atomic rename, checksum recorded; re-init no-ops on matching checksum
  and refuses a mismatch. Chunked `drop_schema` + `fhirpg drop --yes`.
  *Accept met* in m2_semantics.

## M3 — Search

- [x] **T13 Search-parameter compiler.** FHIRPath subset (unions, casts
  `ofType`/`as`, lenient `where(resolve())`) resolved by walking the map
  tree; targets embedded in the map assets per resource; every uncompiled
  parameter carries its reason (`SearchDef::note`).
  *Accept met:* 94.8% of R5's 1,972 parameters compiled (1,870); the
  remainder are composite/special and exists()-style expressions.
- [~] **T14 Query builder + result params.** Done: `Store::search` — AND
  across params, OR across values and targets, all user input bound (no SQL
  interpolation), modifiers :exact/:contains, token system|code, date
  prefixes with precision ranges + Period overlap, quantity value|system|
  code, reference forms, `_id`, `_lastUpdated`, `_count`/offset; strict
  unsupported-parameter errors; `fhirpg search` CLI; `_sort` (base-table
  params + _id/_lastUpdated, honest errors otherwise) and
  `_total=accurate`. *Accept mostly met:* search_semantics + rest suites
  green against live PG. Single-hop `_include` (via compiled reference
  targets) and `_revinclude` (via the search machinery) with
  search.mode=include entries and dangling-reference tolerance.
  *Remaining:* chained `reference.`, cursor paging, lenient handling.
- [x] **T15 Index emission + explain audit.** One index per distinct
  search-target column set emitted with the DDL (R5: 1,813 indexes; full
  init 5.8 s). EXPLAIN audit in tests/bench.rs: token/reference/date
  searches all plan index scans at 100k resources; the test fails on seq
  scans. *Note:* ILIKE-prefix string search bypasses btree — revisit with
  text_pattern_ops if profiles demand.

## M4 — REST server

- [x] **T16 axum skeleton.** `fhirpg-server` crate + `fhirpg serve`:
  versioned base paths, application/fhir+json, 32 MiB body limit,
  OperationOutcome error mapping (400/404/410/412/501/500 with opaque
  internals), /health + /ready. *Accept met* in the rest integration
  suite. *Remaining:* request ids, graceful-shutdown wiring (M6).
- [x] **T17 Full CRUD + history endpoints.** create (server-assigned ids,
  Location + ETag), read (404 vs 410), update with If-Match → 412, delete
  (idempotent 204), instance history bundle, vread.
  *Accept met:* §7 rest suite green, including If-None-Exist conditional
  create (0 → create, 1 → 200 with the match, many → 412).
  *Remaining:* conditional delete-by-search.
- [x] **T18 Search over HTTP.** GET + POST `_search` (query + form
  merged), searchset bundles with fullUrl, self/next links; next-link
  paging verified by walking it in the test. `_count` capped at 1000;
  unimplemented result params answer 501 rather than lying.
- [x] **T19 Batch/transaction.** Batch: independent entries (GET read,
  POST, PUT, DELETE) with per-entry statuses. Transaction: DELETE→POST→PUT
  ordering, urn:uuid reference rewriting (JSON-walk, whole-string match),
  single database transaction via `Store::transact`.
  *Accept met:* urn resolution verified end-to-end; poison-entry
  transaction provably rolls back. *Remaining:* GET entries inside
  transactions, conditional references.
- [x] **T20 CapabilityStatement generation.** Generated per version from
  the map + compiled search params (only supported params listed) — never
  hand-edited. *Remaining:* touchstone-style external validation (M6).

## M5 — R4 and R3

- [ ] **T21 R4 artifacts.** Run generator on 4.0.1; fix spec-parsing deltas.
  *Accept:* full R4 examples corpus round-trips live; REST suite green on
  `/r4`.
- [ ] **T22 R3 artifacts.** Same for 3.0.2.
  *Accept:* full R3 examples corpus round-trips live; REST suite green on
  `/r3`.
- [x] **T23 Multi-version serve.** `fhirpg serve` mounts every version
  whose assets exist and whose schema is installed; per-version capability
  statements. *Verified:* one process serving r3 + r4 + r5, curl-checked.

## M6 — Production hardening

- [x] **T-validate (V9.2).** `fhirpg load --validate` deserializes each
  resource through the typed `fhir` crate model behind the `validate`
  build feature. R5 only for now: the published fhir 1.2.0 crate's r3/r4
  features fail to compile (missing `fhir_version` macro import — bug to
  fix upstream in fhir-rust-crate).
- [x] **T-graceful.** `fhirpg serve` shuts down cleanly on SIGINT/SIGTERM.
- [~] **T24 Observability.** Done: /health, /ready, /metrics (Prometheus
  text: request/response-class/latency counters), X-Request-Id
  (propagated or generated) with per-request tracing that logs
  method/path/status only — never resource content. *Remaining:* JSON log
  format wiring in the CLI, an automated redaction test, latency
  histogram buckets.
- [~] **T25 Pool + timeout hardening.** Done: server-side
  statement_timeout (FHIRPG_STATEMENT_TIMEOUT_MS, default 30 s), pool
  wait timeout 2 s, exhaustion → 503 + Retry-After.
  *Remaining:* an automated saturation test.
- [x] **T26 Migrations + upgrade.** `init` stores the map asset in
  fhirpg_meta; `init --upgrade` diffs installed vs current maps and
  applies additive DDL (new tables/columns/indexes) in lock-safe chunks;
  destructive steps refuse without --allow-destructive; column type
  changes always demand manual migration. *Accept met:* upgrade test —
  reduced install upgrades to full, data survives, re-upgrade no-ops,
  downgrade guarded then forced.
- [x] **T27 TLS feature + bind guard.** rustls in-process behind the
  `tls` feature (`serve --tls-cert/--tls-key`, axum-server) with graceful
  shutdown; loopback-default binding (an explicit --bind is the
  non-loopback acknowledgement). *Verified:* live HTTPS smoke test
  (HTTP/2, CapabilityStatement served, clean SIGTERM shutdown).
- [~] **T28 Benchmarks + regression gate.** Done: gated bench harness
  (load throughput, read latency, EXPLAIN audit) + doc/benchmarks.md with
  measured numbers (6,146 res/s; 1.18 ms reads at 100k).
  *Remaining:* CI regression gate against a recorded baseline; comparison
  against the historical jsonb implementation.
- [x] **T29 Book + generated schema docs.** mdBook (9 chapters:
  introduction, getting started, storage model, SQL querying, search,
  REST API, versions, operations, architecture); builds locally and in
  CI. Column/table→FHIR-path mapping ships inside the map assets
  themselves. *Remaining nicety:* a rendered path→table index page.
- [~] **T30 Security review + release.** Done: LICENSE-MIT/APACHE,
  CHANGELOG, publish metadata (versioned internal deps, keywords), map
  assets embedded in the binary so `cargo install fhirpg` is
  self-contained, `cargo publish --dry-run` clean for the leaf crate.
  *Remaining (human decisions):* pick the release version, publish the
  five crates in dependency order, tag; optionally add cargo-audit/deny
  to CI.
