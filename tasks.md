# Tasks — `fhirpg`

Executable task list for [`plan.md`](plan.md). Ordered; respect the `Depends`
column. Each task is sized for one focused session unless marked **EPIC**
(multi-session, with sub-tasks).

Behaviour requirements referenced as `§n` point at [`spec/index.md`](spec/index.md).
Defects referenced as `Xn` are the fhirbase divergences catalogued in
[`plan.md`](plan.md#divergences-from-fhirbase).

## Conventions for the executing session

- **Verify (every task):** the green gate must pass before the task is done.

  ```sh
  cargo build --all-targets
  cargo test
  cargo clippy --all-targets -- -D warnings
  RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
  ```

- **Database-touching tasks** additionally require `podman compose up -d` (T3)
  and are gated behind `#[ignore]` unless `FHIRPG_TEST_DB` is set, so
  the default `cargo test` stays hermetic.
- **Branch first**, never commit to the default branch. Commit a baseline before
  any mass edit.
- **Spec before code.** If a task changes observable behaviour not already in
  `spec/index.md`, update the spec in the same commit. Code and spec must not
  drift.
- **Source of truth for translation:** `~/github/fhirbase/fhirbase`. Cite the Go
  file and line range in the commit message for each ported unit.

---

## Phase 0 — Foundation

### T1. Repository scaffolding
- **Do:** Mirror `fhir-rust-crate`'s shape. Create `AGENTS.md` (what the project
  is, commands, green gate, house rules), `AGENTS/{architecture,conventions,
  testing,glossary}.md`, `CLAUDE.md` (pointer file only), `CONTRIBUTING.md`,
  `CODE_OF_CONDUCT.md`, `CHANGELOG.md`, `cspell.json`, `CITATION.cff`.
- **Accept:** `CLAUDE.md` repeats no guidance, only links it. Every `AGENTS/`
  and `spec/` file is under 40 KB.
- **Depends:** —

### T2. `Cargo.toml`, licensing, attribution
- **Do:** Set `license = "MIT OR Apache-2.0 OR GPL-2.0-only"`, authors,
  description, `repository`, `rust-version = "1.88"`, `edition = "2024"`,
  keywords, categories, and an `include` list that references only files that
  exist. Write `LICENSE.md` in your usual multi-license form **plus** a NOTICE
  section: fhirbase is MIT, © 2018 Health Samurai; this project is a Rust
  translation and vendors its SQL and transform assets (D7). Declare a
  `[workspace]` with member `xtask`.
- **Accept:** `cargo publish --dry-run` reports no missing-`include` warnings;
  the upstream copyright notice is present and accurate.
- **Depends:** —

### T3. Test PostgreSQL and CI
- **Do:** `compose.yaml` running **PostgreSQL 18** (D8; fhirbase's
  `dev/docker-compose.yaml` pins 10.4), driven by **`podman compose`** (D11).
  Keep the file to the plain Compose spec — no Docker-specific extensions — so
  it stays portable, and document a bare `podman run` one-liner as the fallback,
  since `podman compose` needs an external provider (`podman-compose` or
  `docker-compose`) that not every machine has. Prefer a rootless-friendly
  setup: no privileged mode, a named volume rather than a bind mount, and a
  host port above 1024. Include `contrib` so the legacy assets'
  `CREATE EXTENSION pgcrypto` succeeds there, but do **not** let anything depend
  on it — T11 must also pass against an image without `contrib` (D9). Add
  `.github/workflows/ci.yml`: build, test, clippy pedantic `-D warnings`,
  `doc -D warnings`, MSRV check, and a job that starts PostgreSQL and runs the
  `#[ignore]`d database tests with `FHIRPG_TEST_DB` set. Note that GitHub
  Actions' `services:` block is Docker-backed, so start the container with an
  explicit `podman run` step instead — podman is preinstalled on the Ubuntu
  runners — and poll `pg_isready` rather than relying on a Compose healthcheck.
  README badge.
- **Accept:** CI green on an empty crate; the database job connects and runs
  `SELECT 1`. `podman compose up -d` works rootless on a clean machine, and the
  documented `podman run` fallback works with no Compose provider installed.
  No Docker binary or Docker-specific syntax is required anywhere.
- **Depends:** T2

### T4. Error type and CLI skeleton
- **Do:** `src/error.rs` (`thiserror` enum: `Config`, `Asset`, `Db`, `Io`,
  `Bundle`, `Transform`, `Bulk`), `anyhow` only at the `main` boundary.
  `src/cli.rs` with `clap` derive: all global flags from `main.go:42–94`
  (`--nostats` excluded per D1) and the five subcommands as stubs. Preserve
  every short flag: `-n/--host`, `-p/--port`, `-U/--username`, `-s/--sslmode`,
  `-f/--fhir`, `-d/--db`, `-W/--password`, `-m/--mode`. Preserve the ASCII-art
  banner in `--help` (rebranded) and every long `Description` text, adapted.
  `main.rs` stays thin: parse, dispatch, map error → exit code 1.
- **Accept:** `--help` and every `<cmd> --help` render; exit codes match §7;
  passwords never appear in help or debug output (X6).
- **Depends:** T2

---

## Phase 1 — Transform

### T5. Asset embedding and the version registry
- **Do:** Copy the nine `schema/fhirbase-<v>.sql.json`, `schema/functions.sql.json`,
  and the nine `transform/fhirbase-import-<v>.json` into `assets/` **byte-identical**
  (renaming files only). Record their SHA-256 sums in `assets/CHECKSUMS.txt`.
  `src/assets.rs`: embed via `rust-embed` (or `include_bytes!`), expose
  `FhirVersion` (1.0.2, 1.1.0, 1.4.0, 1.6.0, 1.8.0, 3.0.1, 3.2.0, 3.3.0, 4.0.0)
  with `FromStr`/`Display`, and lazily parse+cache the transform map per version
  (`transform.go:132–158` memoizes the same way).
- **Accept:** A test asserts each vendored asset's checksum, that all nine
  transform maps parse, and that **every `tr/move` target resolves** — verified
  to hold for all nine upstream assets. Unknown `--fhir` values produce a clear
  error listing the known versions.
- **Depends:** T4

### T6. The transformation algorithm  ⭐ core
- **Do:** Port `transform.go:16–195` to `src/transform.rs` per §4. Model the
  directives as an exhaustive `enum` — `Union { key, type }`, `Reference`,
  `Move(path)`, `IsCollection` — parsed once when the asset loads, not
  re-interpreted per node. Fix **X4** (fallible path resolution, no panic) and
  **X5** (unknown directive = asset error). Preserve exactly: `union` wrapping,
  the `Reference` special case inside `union`, reference splitting on a single
  `/`, the `display` passthrough, the discarding of other `Reference` fields,
  array recursion with the *same* transform node, and unknown `resourceType`
  passing through untouched.
- **Accept:** All five `transform_test.go` cases pass, ported verbatim as
  `serde_json::Value` comparisons. Deterministic output for the illegal
  both-variants-present case (§4.6). No `unwrap`/`expect`/`panic!` on any
  input-derived path.
- **Depends:** T5

### T7. `transform` subcommand
- **Do:** `src/commands/transform.rs` per `transform.go:198–235`: read the file,
  parse, transform, pretty-print two-space-indented JSON to stdout.
- **Accept:** Output is `serde_json::Value`-equal to fhirbase's for a corpus of
  ≥20 resources spanning ≥5 resource types, at 3.0.1 and 4.0.0. Record the
  corpus and comparison method in `tests/`.
- **Depends:** T6

### T8. Transform property tests
- **Do:** `proptest` over arbitrary JSON: transforming with an unknown
  `resourceType` is the identity; transform never panics; output is always valid
  JSON; the `union`/`reference` shapes are structurally well-formed.
- **Accept:** 10k cases green; failures shrink to a minimal reproducer.
- **Depends:** T6

---

## Phase 2 — Database, `init`, and the PostgreSQL 18 procedures

### T9. Connection configuration
- **Do:** `src/config.rs` + `src/db.rs` per `db.go`. Reproduce libpq precedence
  (`PGHOST`, `PGPORT`, `PGUSER`, `PGDATABASE`, `PGPASSWORD`, `PGSSLMODE`) with
  explicit flags winning, per §6. Map all six `sslmode` values onto
  `tokio-postgres` + `tokio-postgres-rustls`, including `prefer`'s
  fallback-to-plaintext and `verify-ca`/`verify-full`'s real certificate
  verification. Reject an invalid `sslmode` with an error, not `panic!`
  (`db.go:59`). Fix **X6**: log a redacted DSN carrying the actual `sslmode`.
- **Accept:** A table-driven test covers all six modes plus env-vs-flag
  precedence. A test asserts the password never appears in `Debug`, `Display`,
  or log output.
- **Depends:** T4

### T10. Rebrand `functions.sql.json`
- **Do:** Rewrite the 10 statements in `assets/schema/functions.sql.json`,
  renaming `fhirbase_genid`, `_fhirbase_to_resource`, `fhirbase_create` (both
  arities), `fhirbase_update` (both), `fhirbase_read`, and `fhirbase_delete`
  (both) to `fhirpg_*` (D3). Leave the `_resource` composite type
  unbranded. Update every internal call site — `fhirpg_create/1` calls
  `/2`, `_fhirpg_to_resource` is referenced from four procedures.
- **Accept:** A test asserts the file contains no `fhirbase` substring and
  parses as a 10-element JSON array of strings. Diff-reviewed statement by
  statement against the original. Resolve open question R3 (`fhirpg_*`?) before
  this lands.
- **Depends:** T5

### T11. `init` command
- **Do:** `src/commands/init.rs` per `dbinit.go`. Execute schema statements,
  then function statements, then the two `concept`/`concept_history` tables that
  `dbinit.go:16–33` appends in Go code rather than in the asset. `indicatif`
  progress bar. On failure, report the failing statement and index (fhirbase's
  "perhaps target database is not empty?" hint is worth keeping). Per **D9**,
  a failure of `CREATE EXTENSION … pgcrypto` — and only that statement — is
  logged and tolerated, because PostgreSQL 18 provides `gen_random_uuid()` in
  core and no other statement depends on the extension. Verify the server is
  PostgreSQL 18 or newer up front (D8) and refuse with a clear message if not.
- **Accept:** Against an empty database, `init` succeeds for **all nine**
  versions, *including* on a PostgreSQL 18 image without `contrib` installed;
  an older server is refused with an actionable message;
  `transaction_id_seq` exists (created implicitly by
  `transaction.id serial`, which `fhirpg_create/1` depends on);
  `SELECT fhirpg_create('{"resourceType":"Patient"}'::jsonb)` returns a
  resource with `id`, `meta.versionId`, and `meta.lastUpdated`. Re-running
  `init` on an initialized database is diagnosed clearly.
- **Depends:** T9, T10

### T11a. Stored-procedure behaviour suite   ⭐ safety net for T11c
- **Do:** Golden tests for `fhirpg_create` (both arities), `fhirpg_update`,
  `fhirpg_read`, and `fhirpg_delete`, written against the **current translated**
  CTE semantics before any rewrite: create a new resource; recreate an existing
  one and assert the prior version lands in `_history` with the right `status`
  and `txid`; update; read; delete; and assert `meta.versionId` / `meta.lastUpdated`
  are populated by `_fhirpg_to_resource`. Then add the **concurrency test D13
  depends on**: two sessions writing the same `id` under `READ COMMITTED`,
  asserting which version reaches `_history`. If that test does not demonstrate
  the stale-pre-image race, say so plainly and downgrade D13 to a
  simplification-only change — do not claim a correctness fix that was not
  observed.
- **Accept:** Suite green against the T10 procedures. The concurrency test is
  deterministic (advisory locks or explicit statement ordering, not `sleep`),
  and its result — race reproduced or not — is recorded in `spec/`.
- **Depends:** T11

### T11b. UUIDv7 ids (D12)
- **Do:** Change `fhirpg_genid()` in `assets/schema/functions.sql.json` from
  `gen_random_uuid()::text` to `uuidv7()::text`. Record in spec §8.2 that **all**
  generated ids are v7, so T14 emits `uuidv7()::text` server-side and T15
  generates client-side v7 (`uuid` crate with the `v7` feature,
  `Uuid::now_v7()`) rather than v4 — fhirbase generates ids at all three sites
  and they must agree. Document in README that ids embed a creation timestamp
  and are not interchangeable with fhirbase's.
- **Accept:** `fhirpg_create` on a resource without an `id` returns a valid
  UUIDv7 (version nibble `7`, variant bits `10`), and ids generated close in
  time sort in creation order. T25 quantifies the index-locality benefit; if the
  benchmark shows no benefit, record that honestly rather than quietly keeping
  the change for its own sake.
- **Depends:** T11a

### T11c. `RETURNING OLD`/`NEW` rewrite (D13)
- **Do:** Rewrite `fhirpg_create`, `fhirpg_update`, and `fhirpg_delete` to take
  the pre-image from `RETURNING OLD` on the single upsert/delete statement,
  instead of reading it through a sibling `SELECT … WHERE id = $2` CTE. The
  history insert consumes that pre-image; skip it when `OLD` is null (a true
  insert). Keep the returned resource shape byte-identical.
- **Accept:** T11a's suite passes **unchanged** — that is the whole point of
  writing it first. The concurrency test's outcome flips from failing to passing
  if and only if T11a demonstrated the race. Query plans show one fewer index
  lookup per write. No procedure's signature or return shape changes.
- **Depends:** T11a, T11b

---

## Phase 3 — Bundles and `load`   **EPIC**

### T12. Format detection
- **Do:** `src/bundle/detect.rs` per `load.go:36–194`: gzip sniffing with
  transparent fallback to plaintext (`openFile`), `is_complete_json_object`
  (brace counting with string/escape awareness, `load.go:113–141`), and
  `guess_bundle_type` (two lines complete ⇒ NDJSON; else inspect
  `resourceType`: `Bundle` ⇒ FHIR bundle, other non-empty ⇒ single resource).
- **Accept:** All five `load_test.go` cases pass verbatim, plus cases for
  gzip, an empty file, a one-line file, a BOM, and CRLF line endings.
- **Depends:** T4

### T13. Bundle readers
- **Do:** `src/bundle/{ndjson,single,fhir_bundle,multifile}.rs`. A common
  `Iterator<Item = Result<serde_json::Value>>` interface replaces Go's `bundle`
  interface. `multifile` skips unopenable/undetectable files with a warning and
  continues (`load.go:483–533`), and fixes **X1** via `Drop`. Directory
  arguments are walked recursively (`prewalkDirs`, `load.go:735–762`).
  **`fhir_bundle` is the risk (plan R1):** stream the `entry[]` array without
  buffering the whole document — prototype a `DeserializeSeed` visitor first and
  memory-profile against a synthetic 1 GB bundle before committing; `struson` is
  the approved fallback.
- **Accept:** Peak RSS stays flat (< 100 MB) while reading a 1 GB bundle and a
  1 GB NDJSON file. Malformed entries are skipped with a message naming the file
  and line, matching fhirbase's behaviour. `Count()` is advisory only (X7).
- **Depends:** T12

### T14. Insert loader
- **Do:** `src/load/insert.rs` per `load.go:680–733`. Batched, pipelined
  `INSERT … ON CONFLICT (id) DO NOTHING`; missing/empty `id` uses
  `uuidv7()::text` (D12/T11b — fhirbase uses `gen_random_uuid()::text` at
  `load.go:704`). Fix **X2** (validate `resourceType` against the
  version's known resource set, then quote the identifier), **X3** (per **D10**:
  a transform failure skips that resource and increments a tally; `--strict`
  aborts the run instead — never insert a null result), and **X7** (flush on a
  full buffer and once at end-of-stream; never on `count`).
- **Accept:** A regression test loads a **`Group`** resource successfully — this
  fails outright in fhirbase. A test feeds `resourceType` values containing SQL
  metacharacters and asserts they are rejected, not executed. Duplicate `id`s
  keep the first occurrence. A deliberately untransformable resource is skipped,
  counted, and named in the summary — and under `--strict` aborts the run with a
  non-zero exit and no partial commit beyond already-flushed batches.
- **Depends:** T13, T11, T6

### T15. Copy loader
- **Do:** `src/load/copy.rs` per `load.go:664–678` and the
  `copyFromBundleSource` state machine (`load.go:196–285`): a `COPY` runs for a
  maximal run of same-typed resources and a new one starts when the type
  changes. Text-format `COPY` first (plan R2). Client-side ids use
  `uuid::Uuid::now_v7()` (D12/T11b — fhirbase uses v4 at `load.go:269`).
  Preserve `txid = 0` for parity, with `--txid=new` allocating one real
  transaction id per run (**X10**).
- **Accept:** Grouped NDJSON input loads correctly and measurably faster than
  insert mode; non-grouped input still produces *correct* results (just slower),
  matching fhirbase's documented trade-off. Same `Group`/injection tests as T14.
  Ids generated here are UUIDv7 and indistinguishable in form from those T14 and
  `fhirpg_genid()` produce.
- **Depends:** T13, T11b, T6

### T16. `load` command, progress, tally
- **Do:** `src/commands/load.rs` per `load.go:764–896`. Mode selection
  (`insert` default for files, `copy` default for a Bulk Data URL), progress bar,
  per-resource-type counts printed as an aligned table, total duration. Per
  **D14**, `--memusage` reports operating-system **RSS** — current and peak —
  rather than fhirbase's Go GC statistics, which have no Rust equivalent.
  Implement the reader once (Linux `/proc/self/statm`, macOS `task_info`) and
  reuse it for T13's peak-RSS assertion and T25's benchmarks; keep it out of the
  per-resource path, sampling on the same cadence fhirbase used (every 3,000
  resources, `load.go:792–794`). Per plan R6, use an indeterminate bar unless
  `--count-first` is passed.
- **Accept:** Loading the upstream demo `demo/bundle.ndjson.gzip` reports counts
  that match `SELECT count(*)` per table, in both modes. `--memusage` output
  states plainly that the figure is resident set size, not heap allocation, so
  it is not mistaken for fhirbase's `Alloc`. The RSS reader works on Linux and
  macOS and is covered by a test on both.
- **Depends:** T14, T15

---

## Phase 4 — Bulk Data API and `web`

### T17. Bulk Data API client
- **Do:** `src/bulk.rs` per `bulk.go`. Kickoff with `Prefer: respond-async` and
  a configurable `Accept`; poll `Content-Location` until 200, treating any
  non-2xx as fatal with the response body included; parse `output[].url`;
  download `--numdl` files concurrently into temp files with per-file progress
  and `Accept-Encoding: gzip`. Fix **X8** (no discarded request errors, no
  nil-deref, non-2xx is a hard per-file error).
- **Accept:** Tested against a `wiremock` server covering: immediate 200,
  delayed 200 after N polls, 4xx during kickoff, 5xx during polling, a missing
  `Content-Location`, a malformed manifest, and one failing file among several
  (partial success is reported, not silently swallowed).
- **Depends:** T4

### T18. `bulkget` command
- **Do:** `src/commands/bulkget.rs` per `bulk.go:342–372`: download, then move
  each file into the target directory. Create the target if absent, and handle
  cross-filesystem moves (Go's `os.Rename` fails across devices — copy+remove
  fallback).
- **Accept:** Files land with sensible names; a cross-device target works;
  errors name the file and destination.
- **Depends:** T17

### T19. `load <URL>` integration
- **Do:** Wire `load` to `bulk` per `load.go:864–887`: download to temp files,
  load them, and remove them afterwards — including on error, which the Go
  `defer` does handle but which needs an explicit guard type in Rust.
- **Accept:** Temp files are removed on success, on load failure, and on Ctrl-C.
- **Depends:** T17, T16

### T20. `web` command
- **Do:** `src/commands/web.rs` per `web.go` on `axum` +
  `deadpool-postgres`: `GET /q?query=…` streaming `{columns, rows}` JSON,
  `GET /health`, embedded static assets, request logging, graceful shutdown on
  SIGINT. Rebrand the console's title and logo (D6), leaving the JS behaviour
  unchanged. Fix **X11**: default `--webhost` to `127.0.0.1` and print a loud
  warning when a non-loopback bind is requested.
- **Accept:** The console runs a query end-to-end in a browser. A test asserts
  the default bind is loopback. SQL errors return a JSON message with a non-200
  status rather than a panic. The arbitrary-SQL design is documented as
  **dangerous** in README, `--help`, and §9.
- **Depends:** T11

---

## Phase 5 — FHIR R5

### T21. R5 asset generator (`xtask`)   **EPIC**
- **Do:** A workspace member `xtask` depending on `fhir` (path dependency to
  `~/git/joelparkerhenderson/fhir-rust-crate`) that walks `fhir::r5::meta::elements()`
  and emits both assets per §10.
  - **Schema:** the preamble (the `resource_status` enum guarded by the
    `DO $$ … $$` block, then the `transaction` table — **no**
    `CREATE EXTENSION pgcrypto`, per D9), then for each of the 158
    R5 resources a quoted `<lowercase>` table and `<lowercase>_history` table in
    the exact column shape the 4.0.0 asset uses.
  - **Transform:** for each element path — `path` ending in `[x]` ⇒ one `union`
    entry per type code, keyed `<base><TypeCode>` with
    `{key: base, type: typeCode}`; type code `Reference` ⇒ `reference`;
    `max != "1"` ⇒ `tr/isCollection: true`; a complex-datatype reference ⇒
    `tr/move: [TypeName]` with that type present at the top level.
- **Accept:** Both files parse. Every `tr/move` target resolves. Every table
  name is unique, lowercase, and quoted. Resource-type count is 158. The
  generated `union`/`reference` totals are within a plausible band of 4.0.0's
  929/737. **Cross-check:** for resource types present in both R4 and R5, diff
  the generated R5 transform against the vendored 4.0.0 one and require every
  difference to be explainable by a documented R4→R5 change; record the analysis
  in `spec/`.
- **Depends:** T5

### T22. Hand-verification and vendoring
- **Do:** Manually verify ≥10 diverse resource types (`Patient`, `Observation`,
  `Bundle`, `Group`, `MedicationRequest`, `Encounter`, `Questionnaire`,
  `Subscription`, `Evidence`, `ImplementationGuide`) against the published R5
  specification: every choice element, every reference element, table shape.
  Commit the generated assets with checksums (D5: generate once, vendor).
- **Accept:** A written verification record in `spec/` naming each type checked
  and the discrepancies found and resolved. `init --fhir 5.0.0` succeeds and the
  `fhirpg_*` procedures work against an R5 `Patient`.
- **Depends:** T21, T11

### T23. Default to R5
- **Do:** Add `5.0.0` to `FhirVersion`, flip the `--fhir` default from `3.3.0`
  to `5.0.0` (D4), update every help text, the README, and `spec/index.md` §3.
- **Accept:** `fhirpg init` with no `--fhir` produces the R5 schema.
  A CHANGELOG entry flags the changed default as a **deliberate divergence from
  fhirbase**, whose default is 3.3.0.
- **Depends:** T22

### T24. Optional `--validate` on load
- **Do:** Behind a non-default `validate` feature, deserialize each resource
  into `fhir::r5::resources::Resource` and run `fhir`'s `Validate` trait,
  reporting failures in the tally. Must not affect the default hot path — the
  loader stays `serde_json::Value`-based (plan, *The hot path*).
- **Accept:** Benchmarks show no regression with the feature disabled. Invalid
  resources are reported with resource type, id, and reason; the skip-vs-abort
  policy matches `--strict`.
- **Depends:** T23, T16

---

## Phase 6 — Polish and release

### T25. Benchmarks vs. the Go original
- **Do:** Fixed corpus (the demo bundle plus a synthetic 1M-resource NDJSON).
  Measure wall time and peak RSS (reusing T16's reader) for both modes, against
  fhirbase's binary. Additionally quantify **D12**: load a corpus of
  id-less resources with `uuidv7()` versus `gen_random_uuid()` and compare
  insert throughput, index bloat, and page splits — this is the evidence D12
  was adopted on. Settle plan R2 here too: profile text versus binary `COPY`
  and either adopt binary or record why not.
- **Accept:** Numbers published in README with the method and hardware stated.
  A regression in either mode versus Go is investigated, not shipped. The UUIDv7
  result is reported **whatever it shows** — if the locality benefit does not
  materialize on this workload, say so in the README and reopen D12 rather than
  keeping the change unexamined.
- **Depends:** T16, T11b

### T26. Documentation
- **Do:** README (purpose, install, quick start, the SQL surface, the security
  note on `web`, attribution to fhirbase), `book/` via mdBook, `llms.txt` and
  `llms.json`, rustdoc on every public item, runnable `examples/`. Port
  `doc/scenarios.md` from upstream.
- **Accept:** `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` clean; the mdBook
  builds in CI; every README command copy-pastes and works.
- **Depends:** T20, T23

### T27. Release 1.0.0
- **Do:** CHANGELOG covering the whole port and every divergence (X1–X15, D1–D14),
  `CITATION.cff`, version bump, tag, `cargo publish --dry-run`, GitHub release
  with binaries for macOS/Linux (`update.go`'s self-updater is dropped, so
  installation is the release page or `cargo install`).
- **Accept:** Dry-run green; a fresh `cargo install` of the published crate runs
  `init` and `load` against a clean PostgreSQL 18.
- **Depends:** T25, T26

---

## Dependency graph

```text
T1  T2 ─┬─ T3
        └─ T4 ─┬─ T5 ─┬─ T6 ─┬─ T7
               │      │      └─ T8
               │      └─ T10 ─┐
               │              ├─ T11 ─ T11a ─┬─ T11b ─┬─ T11c
               ├─ T9 ─────────┘              │        │
               │                             │        ├─ T14 ─┐
               ├─ T12 ─ T13 ─────────────────┴────────┴─ T15 ─┤
               │                                              ├─ T16 ─┬─ T19
               └─ T17 ─┬─ T18                                 │       ├─ T25
                       └─ T19 ───────────────────────────────-┘       │
                          T11 ─ T20 ─────────────────────────────────-┤
               T5 ─ T21 ─ T22 ─ T23 ─ T24 ───────────────────────────-┤
                                                                      └─ T26 ─ T27
```

Critical path: **T2 → T4 → T5 → T6 → T11 → T11a → T11b → T14 → T16 → T26 → T27.**

Note the shape of the Phase 2 tail: **T11a is a hard gate**. It writes the
golden-behaviour and concurrency tests *against the faithfully translated
procedures*, before T11b and T11c change them. Reversing that order would leave
the two PostgreSQL 18 adoptions with nothing to prove they preserved behaviour —
which is the entire reason D13 was sequenced after the port rather than folded
into T10.
