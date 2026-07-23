# fhirpg — FHIR to PostgreSQL

Import [FHIR](https://www.hl7.org/fhir/) data into a PostgreSQL database and
work with it relationally: one table per resource type, resource bodies as
`jsonb`, plus history tables and stored procedures for create, read, update,
and delete.

`fhirpg` is a Rust translation of
[fhirbase](https://github.com/fhirbase/fhirbase), a Go utility by Health
Samurai that has been unmaintained since 2019.

> **Status: pre-release.** All five commands work against PostgreSQL 18, at
> every FHIR version from 1.0.2 to 5.0.0, with 172 tests plus 43 that run
> against a live database. Not yet published to crates.io. The delivery plan is
> in [`plan.md`](plan.md) and the normative behaviour in
> [`spec/index.md`](spec/index.md).

## Quick start

```sh
cargo install fhirpg
export PGHOST=localhost PGUSER=you PGDATABASE=clinic

fhirpg init                       # create the schema
fhirpg load export/*.ndjson       # load resources
fhirpg web                        # browse with SQL
```

Then query it as ordinary relational data:

```sql
SELECT resource->>'gender' AS gender, count(*)
  FROM patient
 GROUP BY 1 ORDER BY 2 DESC;
```

References are split into an id and a type when stored, so joins need no string
parsing:

```sql
SELECT p.resource->'name'->0->>'family' AS family,
       count(o.id) AS observations
  FROM patient p
  LEFT JOIN observation o ON o.resource->'subject'->>'id' = p.id
 GROUP BY 1 ORDER BY 2 DESC;
```

Choice elements are collapsed, with the type moved inside — `deceasedBoolean`
becomes `deceased.boolean`. See [the storage model](book/src/storage-model.md),
or just look:

```sh
fhirpg transform patient.json
```

## Commands

| | |
| --- | --- |
| `fhirpg init` | create the schema for a FHIR version |
| `fhirpg load <paths…\|url>` | load NDJSON, Bundles, or single resources; gzipped or not |
| `fhirpg transform <file>` | show what one resource becomes when stored |
| `fhirpg bulkget <url> <dir>` | run a Bulk Data export and save the NDJSON |
| `fhirpg web` | a browser SQL console, bound to localhost |

Formats and compression are detected by **content, not filename**. Memory is
bounded by the largest single resource: a 1 GB Bundle reads with about 2 MB of
growth.

## Documentation

- **[The book](book/src/SUMMARY.md)** — getting started, the storage model,
  loading, querying, Bulk Data, the web console, FHIR versions.
- [`spec/index.md`](spec/index.md) — the normative specification.
- [`doc/benchmarks.md`](doc/benchmarks.md) — measurements against fhirbase.
- [`plan.md`](plan.md) — decisions D1–D15 and the catalogued fhirbase defects
  X1–X17.

## Performance

Measured against fhirbase itself on its own demo bundle — 127,454 resources —
against PostgreSQL 18.4. Full method and caveats in
[`doc/benchmarks.md`](doc/benchmarks.md).

| Input | Mode | fhirbase | fhirpg |
| --- | --- | ---: | ---: |
| non-grouped | `insert` | 4.95 s | **3.39 s** |
| non-grouped | `copy` | 43.58 s | **43.23 s** |
| grouped | `copy` | — | **1.20 s** |

Copy mode is 2.5× faster than insert on grouped input and 13× slower on
non-grouped, which is why the default depends on the source: `insert` for local
files, `copy` for Bulk Data, which arrives grouped.

Worth knowing before you compare yourself: **fhirbase cannot connect to
PostgreSQL 18 at all** with default authentication — its 2018-vintage driver
predates SCRAM-SHA-256. The benchmark only runs against a server reconfigured
for trust authentication.

## Optional: validation

A build with the `validate` feature can check each resource against the typed
FHIR R5 model from the [`fhir`](https://crates.io/crates/fhir) crate:

```sh
cargo install fhirpg --features validate
fhirpg --db clinic load --validate export/*.ndjson
```

```
Patient: gender.code: code "platypus" is not in the required value set
Observation: does not match the FHIR R5 model: missing field `status`

3 resource(s) did not conform to the FHIR model, and were loaded anyway.
```

It **reports; it does not reject.** Storing data a strict model would refuse is
the point of the tool, so non-conforming resources are counted and written —
`--strict` turns the first finding into an aborted run if you want that. The
feature is off by default because it compiles a large generated model that a
normal load never touches.

## Requirements

- **PostgreSQL 18 or newer.** The stored procedures use `uuidv7()` for
  identifier generation and `RETURNING OLD` for history archival, both of which
  arrived in 18.
- **Rust 1.88 or newer** to build from source.
- [Podman](https://podman.io/) if you want the bundled test database:
  `podman compose up -d`.

## How it differs from fhirbase

This is a translation, not a fork: the storage model, the transformation
algorithm, and the command surface are fhirbase's. What changed, and why, is
recorded as decisions D1–D14 in [`plan.md`](plan.md). The ones you would notice:

- **FHIR R5 by default** (fhirbase defaults to 3.3.0, and stops at 4.0.0).
  Versions 1.0.2 through 4.0.0 remain selectable with `--fhir`.
- **PostgreSQL 18 required**, where fhirbase targets 10.
- **No usage telemetry and no binary self-update.** fhirbase phones home on
  every run unless you pass `--nostats`; `fhirpg` has nothing to disable.
- **Stored procedures are named `fhirpg_*`**, not `fhirbase_*`. This means
  `fhirpg` cannot operate on a database that fhirbase initialized, and vice
  versa.
- **Generated ids are UUIDv7**, not v4, so they sort by creation time and index
  better on bulk loads. They do embed a creation timestamp.

It also fixes seventeen defects catalogued in the Go original (X1–X17 in
[`plan.md`](plan.md)), including an SQL injection vector in the insert loader,
a cleartext password in the connection banner, and an inability to load FHIR
`Group` resources at all.

## Security note

The `web` command serves a browser console that runs **arbitrary SQL with no
authentication**. That is what it is for. It binds `127.0.0.1` by default;
setting `--webhost` to anything else exposes an unauthenticated database
console on the network. Do not do that on an untrusted network, and never
against a database holding real patient data.

## Development

```sh
cargo build --all-targets
cargo test                                    # hermetic: no database needed
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

All four must pass before any change is considered done. For the database
tests:

```sh
podman compose up -d
FHIRPG_TEST_DB="host=localhost port=5433 user=fhirpg password=fhirpg dbname=fhirpg" \
  cargo test -- --ignored
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`AGENTS.md`](AGENTS.md).

## License

`MIT OR Apache-2.0 OR GPL-2.0-only`. Material derived from fhirbase remains
under its MIT terms, © 2018 Health Samurai; see [`LICENSE.md`](LICENSE.md).

FHIR® is a registered trademark of Health Level Seven International.
PostgreSQL® is a registered trademark of the PostgreSQL Community Association
of Canada. This project is affiliated with neither.
