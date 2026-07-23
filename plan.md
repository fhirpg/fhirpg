# Plan — `fhirpg`

A Rust translation of [fhirbase](https://github.com/fhirbase/fhirbase), the Go
command-line utility that imports [FHIR](https://www.hl7.org/fhir/) data into
PostgreSQL and exposes it relationally.

Status: **proposed**, 2026-07-23. Companion files: [`tasks.md`](tasks.md)
(discrete executable tasks) and [`spec/index.md`](spec/index.md) (the living
specification — the source of truth for behaviour).

## Vision

Take fhirbase's proven design — FHIR resources stored as `jsonb`, one table per
resource type, plus history tables and CRUD stored procedures — and re-express
it in Rust: memory-safe, `async`, dependency-lean, and current with **FHIR R5**
rather than frozen at the 2019 draft ballot versions.

Two things make this more than a mechanical port:

1. **`fhir-rust-crate` already exists.** The sibling crate
   `~/git/joelparkerhenderson/fhir-rust-crate` (`fhir` v1.0.0) ships the full R5
   model — 158 resources, 50 datatypes, 419 code enums — *and* a runtime
   element-metadata table (`fhir::r5::meta`, 9,333 entries) derived from the
   official HL7 specification JSON. That metadata is exactly the input needed to
   generate the R5 schema and transform assets that fhirbase never had. We are
   not guessing at R5; we are deriving it from the spec.
2. **The Go original has real defects.** Translation is an opportunity to fix
   them rather than faithfully reproduce them. They are enumerated in
   [Divergences](#divergences-from-fhirbase) and specified in `spec/index.md`.

## Source inventory (what is being translated)

`~/github/fhirbase/fhirbase`, Go 1.12, 13 `.go` files, ~2,656 lines.

| Go file | Lines | Role | Disposition |
| --- | ---: | --- | --- |
| `main.go` | 308 | `urfave/cli` app: global flags + 6 subcommands | → `clap` derive (`src/cli.rs`) |
| `db.go` | 97 | `pgx` connection config, libpq env, sslmode | → `src/db.rs` (`tokio-postgres`) |
| `dbinit.go` | 121 | `init`: run schema + function DDL | → `src/commands/init.rs` |
| `transform.go` | 235 | The fhirbase JSON transformation algorithm | → `src/transform.rs` |
| `load.go` | 896 | Bundle detection/readers, copy & insert loaders | → `src/bundle/`, `src/load/` |
| `bulk.go` | 372 | Bulk Data API client, parallel downloads | → `src/bulk.rs` |
| `web.go` | 189 | HTTP SQL console + static assets | → `src/commands/web.rs` (`axum`) |
| `update.go` | 106 | Self-update from fhirbase GitHub releases | **dropped** |
| `stats.go` | 105 | Anonymous telemetry to `license.aidbox.app` | **dropped** |
| `*_test.go` | 227 | Transform cases, bundle-type cases, db init | → Rust tests, cases preserved |

Embedded assets, ~1.6 MB:

| Asset | Content |
| --- | --- |
| `schema/fhirbase-<v>.sql.json` × 9 | JSON array of DDL statements; 293 for 4.0.0 (145 resource tables + 145 history tables + preamble) |
| `schema/functions.sql.json` | 10 statements: `_resource` composite type + the `fhirbase_*` CRUD procedures |
| `transform/fhirbase-import-<v>.json` × 9 | The per-version transformation map; 4.0.0 has 155 top-level types, 929 `union` and 737 `reference` directives |
| `web/{index.html,app.js,app.css,logo.svg}` | ~23 KB vanilla-JS SQL console |

## Decisions (settled)

| # | Decision | Rationale |
| --- | --- | --- |
| D1 | Port `init`, `transform`, `load`, `bulkget`, `web`. Drop `update` and `stats`. | Self-update points at fhirbase's GitHub releases and telemetry posts to Health Samurai's endpoint; neither transfers to a fork. Dropping telemetry also removes a network call, a machine-ID probe, and a `WaitGroup` race. |
| D2 | `tokio` + `tokio-postgres`, `deadpool-postgres` for the web pool. | Closest to the Go concurrency model. `tokio-postgres` exposes `COPY … FROM STDIN`, which the copy loader requires; `sqlx`'s compile-time checking buys nothing here because the schema is generated from JSON at runtime. |
| D3 | Rename the SQL procedures `fhirbase_*` → `fhirpg_*`. | Only `schema/functions.sql.json` (10 statements, 4.9 KB) carries branded identifiers; the nine per-version schema files carry none. The rename is contained to one small file. **Note:** this deliberately breaks drop-in compatibility with existing fhirbase databases — see [Risks](#risks) R3. |
| D4 | Default `--fhir` is **5.0.0 (R5)**; 1.0.2–4.0.0 remain selectable. | R5 is the current HL7 release. Requires generating two new assets — see D5. |
| D5 | Generate the R5 assets once from `fhir::r5::meta`, hand-check a sample, vendor the results. | The `fhir` crate's element metadata already encodes every path, cardinality, type code, and reference target. A one-shot generator is far more trustworthy than hand-authoring 320 DDL statements and ~1,000 transform directives. |
| D6 | Reuse the web console assets, rebranding title/logo only. | Keeps the port focused on the Rust backend; the console is 4 KB of vanilla JS with no build step. |
| D7 | License `MIT OR Apache-2.0 OR GPL-2.0-only`, retaining upstream attribution. | Matches your other crates. fhirbase is MIT (© 2018 Health Samurai); the vendored SQL/transform assets and the translated logic are MIT-derived, so the upstream copyright notice and a NOTICE section are **required** regardless of the additional license options offered on original work. |
| D8 | Require **PostgreSQL 18**. | A single modern target removes a compatibility axis. `gen_random_uuid()` has been built in since 13, so `pgcrypto` is no longer needed (see D9). PostgreSQL 18 additionally offers `uuidv7()` and `RETURNING`'s `OLD`/`NEW` aliases, both adopted — see [PostgreSQL 18 features](#postgresql-18-features), D12, and D13. |
| D9 | Drop `CREATE EXTENSION pgcrypto` from the **generated** R5 schema; keep it in the **vendored** legacy schemas, and treat its failure as non-fatal. | The nine legacy assets must stay byte-identical (D5 vendoring, spec §3), so the statement cannot be edited out of them. On a minimal PostgreSQL 18 install without `contrib`, that statement fails — yet nothing depends on it, since `gen_random_uuid()` is core. `init` therefore tolerates its failure specifically, and the generated R5 asset simply omits it. |
| D10 | Transform failure on load: **skip the resource and tally it**; `--strict` aborts the run. | Bulk FHIR data is routinely imperfect, and aborting a million-resource load on one bad resource is hostile. The tally makes the loss visible rather than silent, which is the actual failing in fhirbase (X3). |
| D11 | **Podman**, not Docker, for the local test database. | Rootless and daemonless. The compose file stays plain Compose spec so it is portable, but the documented workflow is `podman compose up -d`. Note that GitHub Actions' `services:` block is Docker-backed, so CI starts PostgreSQL with an explicit `podman run` step rather than a service container. fhirbase also shipped a `docker` Makefile target that built and pushed an image; if an equivalent is wanted later it becomes a `podman build` / `podman push` task, not a port of that target. |
| D12 | Generate ids with **UUIDv7** everywhere: `fhirpg_genid()` uses `uuidv7()`, the insert loader uses `uuidv7()::text` instead of `gen_random_uuid()::text`, and the copy loader generates client-side v7 (`uuid` crate, `v7` feature) instead of v4. | Time-ordered ids give near-append B-tree insertion on the `id` primary key. All three sites must change together — fhirbase generates ids in the procedure (`functions.sql.json`), in the insert loader (`load.go:704`), and client-side in the copy loader (`load.go:269`), and a mix of v4 and v7 would defeat the locality benefit for exactly the bulk-load path it is meant to help. Trade-off: ids embed a creation timestamp and are no longer interchangeable with fhirbase's. Task T11b; benchmarked in T25. |
| D13 | Rewrite `fhirpg_create`, `fhirpg_update`, and `fhirpg_delete` to take the pre-image from `RETURNING OLD` rather than from a sibling CTE. | Simpler SQL and one fewer index lookup per write — but the real motivation is correctness. The current CTE reads the row to archive via a separate `SELECT … WHERE id = $2`, which sees the statement snapshot, while the `ON CONFLICT DO UPDATE` in the sibling branch re-reads the live row. Under `READ COMMITTED` with a concurrent writer those two can differ, so the row written to `_history` may not be the row that was actually replaced. `RETURNING OLD` yields the genuine pre-image of the same statement. **Demonstrated** by `procedures_suite::d13_concurrency` against PostgreSQL 18.4 (task T11a): session A commits `v=2`, session B's `fhirpg_create` replaces it, and history ends up holding only `v=1` — A's committed version is lost from history entirely. Tasks T11a, T11c. |
| D14 | `--memusage` reports **operating-system RSS** (current and peak), not a heap-allocation figure. | fhirbase's flag prints Go GC statistics — `Alloc`, `TotalAlloc`, `Sys`, `NumGC` (`load.go:590–604`) — which have no Rust equivalent, since there is no garbage collector and the default allocator does not expose live bytes. RSS is a different quantity and the docs must say so, but it is the one users actually want ("will this load exhaust my machine?"), it costs nothing on the hot path, and spec invariant 6 plus T13's acceptance already require measuring peak RSS while streaming a 1 GB file. Reusing that measurement is nearly free; a counting global allocator was rejected as hot-path overhead for a debug flag. Task T16. |

## Repository conventions

Follow `fhir-rust-crate` exactly — that project's shape is the house style:

- `AGENTS.md` + `AGENTS/` for operational guidance; `CLAUDE.md` points at them.
- `spec/` holds living specifications; **spec is the source of truth**, and code
  and spec must not drift.
- Every file in `AGENTS/` and `spec/` stays under 40 KB; split when it grows.
- The **green gate**, run before any task is considered done:

  ```sh
  cargo build --all-targets
  cargo test                                    # unit tests + doctests
  cargo clippy --all-targets -- -D warnings     # pedantic, zero warnings
  RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
  ```

- Never commit to the default branch; branch first.
- Dependencies are added only with cause and are annotated inline in
  `Cargo.toml`, one comment per dependency.

## Architecture

```text
fhirpg/
  Cargo.toml
  src/
    main.rs            # thin: parse CLI, dispatch, set exit code
    cli.rs             # clap derive: global flags + 5 subcommands
    config.rs          # PgConfig; libpq env precedence; sslmode
    db.rs              # connect, TLS policy, pool construction
    error.rs           # thiserror error enum; anyhow at the boundary
    assets.rs          # embedded schema/transform/web assets + version registry
    transform.rs       # the fhirbase transformation algorithm (spec §4)
    bundle/
      mod.rs           # `Bundle` trait / `Source` enum: Iterator<Item = Result<Value>>
      detect.rs        # gzip sniff + ndjson vs FHIR-Bundle vs single-resource
      ndjson.rs
      fhir_bundle.rs   # streaming `entry[]` reader
      single.rs
      multifile.rs     # concatenates sources, skipping unreadable ones
    load/
      mod.rs           # Loader trait, progress plumbing, per-type tallies
      copy.rs          # COPY … FROM STDIN, re-issued per resource-type run
      insert.rs        # pipelined INSERT … ON CONFLICT DO NOTHING, batched
    bulk.rs            # Bulk Data API: kickoff, poll, parallel download
    commands/
      init.rs  transform.rs  load.rs  bulkget.rs  web.rs
  assets/
    schema/       # 9 vendored + 1 generated (5.0.0) + functions.sql.json
    transform/    # 9 vendored + 1 generated (5.0.0)
    web/          # rebranded console
  xtask/          # one-shot R5 asset generator (workspace member, not published)
  spec/           # living specifications
  AGENTS/         # agent guidance
  tests/          # integration tests (transform cases, bundle detection, pg)
```

Layering, strictly downward: `commands` → `load`/`bulk` → `bundle`/`transform`
→ `assets`/`db`/`config`. `transform.rs` has no I/O and no database
dependency, so it is unit-testable in isolation — as in Go, where the transform
tests need no PostgreSQL.

### The hot path

`load` is the only performance-sensitive command. Its shape must stay:

```text
file(s) → gzip? → format detect → stream resources (serde_json::Value)
        → transform (assets/transform/<version>.json)
        → COPY or batched INSERT → PostgreSQL
```

Resources stay as `serde_json::Value` throughout. They are deliberately **not**
deserialized into `fhir::r5::resources::*`: the loader must accept resources of
any version, unknown resource types, and non-conforming data, exactly as
fhirbase does. The typed model is used at asset-generation time and, optionally,
behind a `--validate` flag (Phase 5).

## Phases

Each phase is independently shippable and leaves the tree green.

### Phase 0 — Foundation

Repository scaffolding at parity with `fhir-rust-crate`: `AGENTS.md`, `AGENTS/`,
`spec/`, `CLAUDE.md`, `LICENSE.md` with upstream attribution, `CHANGELOG.md`,
`CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `cspell.json`, GitHub Actions CI
running the full green gate plus MSRV, and a `compose.yaml` for a PostgreSQL 18
test instance run under Podman (fhirbase's dev compose pins 10.4 and assumes
Docker).

No behaviour. Ends with `cargo build` green on a hello-world binary.

### Phase 1 — Transform (the algorithmic core)

Port `transform.go` and vendor the nine transform assets. This is the highest-value
phase to do first: it is pure, self-contained, has an existing test corpus, and
everything else depends on it being right.

Deliverables: `src/transform.rs`, `src/assets.rs`, the `transform` subcommand,
and the five Go test cases plus property tests. Specified in `spec/index.md` §4.

Ends with `fhirpg --fhir 4.0.0 transform patient.json` producing output
byte-equivalent (as `serde_json::Value`) to fhirbase's.

### Phase 2 — Database and `init`

`config.rs` (libpq env precedence, `sslmode` policy), `db.rs`, `assets` schema
registry, the rebranded `functions.sql.json`, and the `init` command with a
progress bar. Ends with `init` producing a schema against which
`SELECT fhirpg_create('{"resourceType":"Patient"}'::jsonb)` succeeds.

### Phase 3 — Bundles and `load`

The largest phase. Format detection, the four bundle readers, the multifile
source, both loaders, progress reporting, and the per-resource-type tally. This
is where the Go defects concentrate — see [Divergences](#divergences-from-fhirbase).

Ends with a round-trip integration test: load the repo's demo
`bundle.ndjson.gzip`, assert row counts per table.

### Phase 4 — Bulk Data API and `web`

`bulkget` (kickoff → poll `Content-Location` → parallel download with
per-file progress) and the `web` SQL console on `axum`. `load <http://…>` wires
the two together.

### Phase 5 — R5

Generate `assets/schema/fhirpg-5.0.0.sql.json` and
`assets/transform/fhirpg-import-5.0.0.json` from `fhir::r5::meta` via
`xtask`, hand-check a sample of resource types against the R5 specification,
vendor them, flip the default `--fhir` to `5.0.0`, and add the optional
`--validate` load flag backed by `fhir`'s `Validate` trait.

Deliberately last: the port is proven against the nine vendored assets *first*,
so any R5 problem is unambiguously an asset problem, not a port problem.

### Phase 6 — Polish and release

README with an asciicast, `book/` (mdBook), `llms.txt`, benchmark comparison
against the Go original on a fixed corpus, `CITATION.cff`, 1.0.0.

## Divergences from fhirbase

Defects found while reading the Go source. Each becomes a spec requirement and a
test. This list is the strongest argument for the port being worth doing.

| # | Go location | Defect | Resolution |
| --- | --- | --- | --- |
| X1 | `load.go:539–547` | `multifileBundle.Close()` calls `b.Close()` — itself — instead of `bndl.Close()`. Infinite recursion; stack overflow on any close. | Rust `Drop`, correct by construction. |
| X2 | `load.go:704–706` | `insertLoader` interpolates the table name unquoted: `INSERT INTO %s`. The name derives from the input data's `resourceType`. FHIR has a **`Group`** resource → `INSERT INTO group` is a PostgreSQL syntax error, so insert mode (the default) cannot load `Group` at all. It is also an **SQL injection vector** from resource content. The schema DDL quotes correctly (`"group"`), and the copy loader is safe via `pgx.Identifier`. | Validate `resourceType` against the version's known resource set; quote every identifier. Regression test loads a `Group`. |
| X3 | `load.go:693–697` | `transformedResource, err := doTransform(…)` shadows the outer `err`. A transform failure is printed, then the possibly-`nil` result is queued for insertion anyway. | `?` propagation; a transform failure fails the resource explicitly, with a documented skip-or-abort policy. |
| X4 | `transform.go:22` | `getByPath` does `res[key].(map[string]interface{})` and only then tests `res == nil` — the unchecked type assertion **panics** first on a missing or non-object path segment. | Return `Result`; a malformed transform asset is a startup error, not a panic. |
| X5 | `transform.go:37–83` | A `tr/act` other than `union`/`reference` falls through with `res` unset, silently replacing the field with `null`. | Exhaustive `enum TrAct`; unknown directives are a load-time asset error. |
| X6 | `db.go:79`, `web.go:140` | The "Connected to database …" banner is built with `sslmode=disable` hardcoded regardless of the real setting, and **prints the password in cleartext** to stdout. | Log a redacted DSN with the actual `sslmode`. |
| X7 | `load.go:709` | Batch flush fires on `curResource % batchSize == 0`, which is true at `curResource == 0`, sending an empty first batch; the terminal condition depends on `Count()`, which `multifileBundle` overstates whenever a file fails to open. | Flush on a filled buffer and once at end-of-stream; progress totals are advisory and never control correctness. |
| X8 | `bulk.go:73–75, 212–214` | `http.NewRequest`'s error is discarded, then `req.Header.Add` dereferences a possibly-`nil` `req`; a non-200 download response is reported but the body is used anyway. | `?` on every fallible step; non-2xx is a hard per-file error. |
| X9 | `stats.go:30` | `eventsWg.Add(1)` runs *inside* the spawned goroutine, racing `Wait()`; events are silently lost. | Not applicable — telemetry dropped (D1). |
| X10 | `load.go:277` | `txid` is hardcoded to `0` for every loaded resource, so bulk-loaded rows sit outside the `transaction_id_seq` history mechanism. | Preserve the behaviour for parity, but allocate a single real `txid` per load run behind `--txid=new`, and document it. |
| X12 | `db.go:54–58` | `verify-ca` and `verify-full` share one branch that sets `ServerName`, and Go's TLS stack then verifies the hostname as well. So `verify-ca` behaves as `verify-full` — **stricter than libpq**, where `verify-ca` validates the chain but deliberately does *not* check the hostname. A connection libpq accepts is refused. Found while porting T9. | Map the two modes separately: `verify-ca` verifies the chain only, `verify-full` verifies chain and hostname. `native-tls`'s two danger switches express this directly. Covered by a table-driven test over all six modes. |
| X13 | `functions.sql.json`, `fhirbase_delete` | The `deleted` CTE copies `status` from the live row instead of writing `'deleted'`, so **no procedure ever produces the `resource_status` enum's `'deleted'` value**. History cannot distinguish a delete from an update. Confirmed against PostgreSQL 18.4: create → update → delete leaves history `['created','updated','updated']`. | Write `'deleted'` for the delete row. Witness test `witness_x13_…` asserts the broken behaviour today and flips when T11c lands. |
| X14 | `functions.sql.json`, `fhirbase_delete` | The procedure inserts into `_history` **twice** — once with the row's existing `txid`, once with the supplied one — and `_history`'s primary key is `(id, txid)`. Supplying a `txid` equal to the row's current one is a unique violation. Not hypothetical: every bulk-loaded row has `txid = 0` (X10), so `fhirpg_delete(rt, id, 0)` on loaded data always fails. | Derive the history rows so the two cannot collide; `RETURNING OLD` (D13) removes the double read that causes it. Witness test `witness_x14_…`. |
| X15 | `functions.sql.json`, every procedure | Procedures build SQL with `format('%s', resource_type)` rather than `%I`. Two consequences: a resource type that lowercases to a reserved word is **unusable** — `fhirpg_create` and `fhirpg_read` on a FHIR `Group` both fail with a syntax error — and resource-derived data is concatenated into a string that `EXECUTE` runs. Demonstrated: feeding `fhirpg_read` a crafted type introduces a join and comments out the query's own alias. This is X2's twin, one layer down. | Use `%I` for every identifier. Witness test `witness_x15_…` covers `Group` and the query-structure change. |
| X11 | `web.go:20–87` | `/q` executes arbitrary SQL from a query-string parameter with no authentication, bound by default to **all interfaces** (`--webhost` defaults to empty). | Keep the console, but default the bind address to `127.0.0.1`, require an explicit `--webhost 0.0.0.0` to expose it, and print a prominent warning when it is exposed. |

Two behaviours that look like bugs but are **intentional and must be preserved**:

- The `reference` directive keeps only `id`, `resourceType`, and `display`,
  discarding `identifier`, `type`, and extensions on a `Reference`. This is
  fhirbase's storage model, and the transform tests assert it.
- `tr/isCollection` is present in the assets but the Go transform never reads
  it; arrays are handled structurally by recursing with the same transform node.
  Do the same, and note the redundancy in the spec rather than "fixing" it.

## Risks

| # | Risk | Mitigation |
| --- | --- | --- |
| R1 | **Streaming a FHIR Bundle's `entry[]` array.** `serde_json` has no pull parser; the naive port buffers a multi-GB bundle into memory. | Push-based `DeserializeSeed`/`Visitor` over `entry`, yielding each resource to a callback. Prototype and memory-profile in Phase 3 before committing; `struson` is the fallback dependency. |
| R2 | **`COPY` with the `resource_status` enum.** `BinaryCopyInWriter` needs the enum's runtime OID. | Use text-format `COPY` first (simple, correct, handles enums natively); measure, and only move to binary if profiling justifies it. |
| R3 | **D3 breaks compatibility with existing fhirbase databases.** A database initialized by fhirbase has `fhirbase_*` procedures; this tool creates `fhirpg_*` ones, and neither recognizes the other's. | Accepted per D3, and settled: the procedure prefix is `fhirpg_`, matching the binary. Document the incompatibility prominently in README and CHANGELOG. Migrating an existing fhirbase database is out of scope; re-running `init` against it is diagnosed, not attempted. |
| R4 | **R5 asset correctness.** Generated once and vendored (D5), so there is no upstream oracle to diff against. | Structural self-checks (every `tr/move` target resolves — verified to hold for 4.0.0; every table name unique and quoted); hand-check ~10 diverse resource types against the R5 spec; and diff generated-R5 against vendored-4.0.0 for shared types, where every difference must be explainable by a real R4→R5 change. |
| R5 | **`fhir` is a path dependency** not yet on crates.io, blocking `cargo publish`. | Only `xtask` needs it (plus the optional `--validate` feature). Keep it out of the default dependency set so the binary can ship independently. |
| R6 | **Progress bars need exact totals**, which fhirbase gets by counting entries in a first pass and rewinding — costly for gzip, since it re-inflates. | Indeterminate progress by default for compressed and streamed input; `--count-first` opts into the two-pass behaviour. |

## Non-goals

- A FHIR REST server, FHIRPath evaluation, or FHIR search. `fhirpg`
  imports and stores; querying is SQL.
- Re-implementing the FHIR data model — that is `fhir-rust-crate`'s job.
- Database backends other than PostgreSQL.
- Bug-for-bug fidelity with fhirbase. Output fidelity is the goal; the defects
  above are fixed.

## PostgreSQL 18 features

Requiring PostgreSQL 18 (D8) makes three features available that the Go
original, targeting PostgreSQL 10, could not use. The first two are **adopted**
(D12, D13); the third needs no decision.

1. **`uuidv7()` for generated ids — adopted as D12.** UUIDv7 is time-ordered, so
   generated ids are near-sequential. Since `id` is the `text` primary key of
   every resource table, that turns random B-tree insertion into
   mostly-append insertion: materially better index locality and less page
   splitting on large loads, which is exactly the workload this tool exists for.
   Adoption has **three** touch points, not one — see D12 — because ids are
   generated in three places, and they must agree.
2. **`RETURNING` with `OLD` and `NEW` aliases — adopted as D13.** Beyond
   simplifying the archival SQL, this looks like a **correctness** fix; see D13.
   Sequenced after the procedures are proven, with a golden-behaviour suite
   written first (T11a).
3. **Asynchronous I/O and B-tree skip scans.** No code change; they may simply
   make loads faster. Relevant only to the T25 benchmark narrative.

## Open questions

None. Every question raised during planning is settled as D1–D14. Two items
remain *deliberately deferred to measurement* rather than decided in advance,
and both are tracked as risks rather than questions:

- **Text vs. binary `COPY`** (R2) — text format first, because it handles the
  `resource_status` enum without needing its runtime OID; revisit only if T25's
  profiling justifies the complexity.
- ~~**The `RETURNING OLD` concurrency claim** (D13)~~ — **settled**. T11a's
  `d13_concurrency` reproduces the stale-pre-image race deterministically, so
  D13 stands as a correctness fix, not merely a simplification.
