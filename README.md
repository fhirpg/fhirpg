# fhirpg — FHIR in PostgreSQL, relationally

Store [FHIR](https://hl7.org/fhir/) resources in PostgreSQL 18 as **real
relational tables** — typed columns, child tables, foreign keys, and check
constraints — not JSON or JSONB blobs. Serve them back through the standard
FHIR RESTful API.

- **Given FHIR data** (NDJSON, Bundles, single resources, or REST writes),
  fhirpg shreds each resource into normalized tables generated from the FHIR
  specification itself.
- **Given a FHIR request** (read, vread, search, history, batch/transaction),
  fhirpg reconstructs resources from those tables, losslessly, and answers
  over HTTP exactly as a FHIR server must.

Supported FHIR versions: **R5 (5.0.0, default), R4 (4.0.1), R3 (3.0.2)** —
each with its own generated schema, installed side by side in PostgreSQL
schemas `r5`, `r4`, `r3`.

> **Status: functional end to end, pre-release.** The generator, the
> shred/reconstruct engine, the PostgreSQL store, search, and the FHIR REST
> server all work: all **7,399 official FHIR example resources** (R3 + R4 +
> R5) round-trip **losslessly** through the fully normalized schema — in
> memory, through live PostgreSQL 18, and 10,000 generated property-test
> cases besides. 94.8% of R5 search parameters compile to indexed SQL, and
> `fhirpg serve` mounts every installed version with CRUD, history, ETag
> concurrency, search, and all-or-nothing transaction Bundles. Remaining
> before production: `_sort`/`_include`/cursor paging, conditional
> operations, and the M6 hardening list in [`tasks.md`](tasks.md). This is
> a ground-up rewrite of the earlier fhirbase-style fhirpg (jsonb bodies;
> see git history). Normative behaviour: [`spec/index.md`](spec/index.md);
> measurements: [`doc/benchmarks.md`](doc/benchmarks.md).

## Why relational

JSONB storage makes writing FHIR easy and querying it painful. Normalized
storage inverts that trade, and for a production clinical system the trade is
right:

- **Integrity the database enforces** — enum columns backed by FHIR value
  sets, `CHECK` constraints on choice elements, typed dates and decimals,
  reference columns that can be joined and (optionally) constrained.
- **SQL that reads like the domain** — `SELECT family FROM r5.patient_name`,
  no `->>'…'` path spelunking, and the query planner sees real column
  statistics.
- **Search that is just SQL** — FHIR search parameters compile to indexed
  predicates on ordinary columns.

## Quick start

```sh
cargo install --path crates/fhirpg
export PGHOST=localhost PGUSER=you PGDATABASE=clinic

fhirpg init --fhir-version r5     # create the generated relational schema
fhirpg load export/*.ndjson       # shred and load resources
fhirpg serve                      # FHIR REST server on 127.0.0.1:8080
fhirpg get Patient example        # reconstruct one resource
fhirpg export Patient             # stream resources back out as NDJSON
fhirpg transform patient.json     # show the rows a resource shreds into
```

Then query relationally:

```sql
SELECT n.family, count(o.id) AS observations
  FROM r5.patient p
  JOIN r5.patient_name n ON n.rid = p.id AND n.ords = '{1}'
  LEFT JOIN r5.observation o
    ON o.subject_ref_type = 'Patient' AND o.subject_ref_id = p.id
 GROUP BY n.family
 ORDER BY observations DESC;
```

Every child table addresses its rows with `rid` (the resource id) and
`ords smallint[]` (the 1-based index path through repeating elements), so
arbitrarily nested — even recursive — structure stays joinable.

Or over FHIR REST:

```sh
curl 'localhost:8080/r5/metadata'                  # CapabilityStatement
curl 'localhost:8080/r5/Patient?name=smith&_count=10'
curl 'localhost:8080/r5/Observation?subject=Patient/123&date=ge2026-01-01'
curl -X POST localhost:8080/r5 -d @transaction-bundle.json \
     -H 'content-type: application/fhir+json'      # all-or-nothing
```

## Commands

| Command | Purpose |
| --- | --- |
| `fhirpg init` | create the generated schema for a FHIR version |
| `fhirpg load <paths…>` | load NDJSON, Bundles, or single resources (gzip ok) |
| `fhirpg get / delete` | read back or remove one resource (history retained) |
| `fhirpg export` | reconstruct resources back out as NDJSON |
| `fhirpg transform <file>` | show the rows one resource shreds into |
| `fhirpg search <Type> [name=value…]` | run a FHIR search from the shell |
| `fhirpg serve` | run the FHIR RESTful API server (every installed version) |
| `fhirpg drop --yes` | remove one version's schema and data |
| `fhirpg gen` | (dev) regenerate the relational-map assets from FHIR specs |

## Architecture in one paragraph

A build-time generator (`fhirpg gen`) reads each FHIR version's
StructureDefinitions and SearchParameters and emits two artifacts per
version: the **DDL** (every resource's base table plus child tables for
repeating and nested elements) and a compact **relational map**. At runtime a
single generic engine walks any resource against the map to shred it into
rows, and walks the map in reverse to reconstruct the identical resource —
round-trip fidelity is a tested invariant, including decimal precision.
Search parameters compile against the same map into SQL. The HTTP layer is
axum; storage access is tokio-postgres with a deadpool pool; every write is
one transaction with optimistic concurrency via FHIR ETags. The
[`fhir`](https://crates.io/crates/fhir) crate supplies the typed R3/R4/R5
model for optional strict validation (`--validate`).

## Production posture

fhirpg targets mission-critical clinical deployment: transactional writes,
version history and audit on every resource, optimistic locking, structured
logging with `tracing`, Prometheus metrics, health/readiness endpoints,
connection pooling, versioned migrations, and a documented backup and
zero-downtime upgrade story. See
[`spec/10-operations.md`](spec/10-operations.md). fhirpg handles PHI: deployments must put TLS and authentication
in front of it (or terminate TLS in-process via the `tls` feature) — the
spec defines what fhirpg guarantees and what the deployment must provide.

## Documentation

- **[The book](book/src/SUMMARY.md)** — getting started, the storage
  model, querying, search, the REST API, operations, architecture.
- [`spec/`](spec/index.md) — the normative specification, one file per section.
- [`plan.md`](plan.md) — design decisions, risks, milestones.
- [`tasks.md`](tasks.md) — the implementation work breakdown.
- [`doc/benchmarks.md`](doc/benchmarks.md) — measured performance.
- [`doc/ci.md`](doc/ci.md) — the gates on GitHub and Codeberg, and how
  releases are cut.
- [`CHANGELOG.md`](CHANGELOG.md).

## License

MIT OR Apache-2.0.
