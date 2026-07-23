# Benchmarks

Task T25. Measured against fhirbase itself, on identical data, on one machine.
Numbers from a different machine will differ; the ratios are the point.

## Method

- **Hardware:** Apple M-series, macOS 25.4, PostgreSQL 18.4 in a rootless
  Podman container on the same host.
- **Corpus:** fhirbase's own `demo/bundle.ndjson.gzip` — 127,454 resources
  across 15 types, gzipped NDJSON.
- **Both tools built release/optimized.** fhirbase was built from
  `~/github/fhirbase/fhirbase` with one change: `packr`'s compiled-in assets
  swapped for filesystem reads, so it builds with a current Go toolchain. That
  affects startup only.
- Every run starts from a **freshly created database**, initialized by the tool
  under test.
- Timing is wall clock around the `load` invocation, three runs for insert, two
  for copy.
- **stdout is discarded, not piped** — see the methodology note below, which
  cost me a wrong answer.

## fhirbase cannot connect to PostgreSQL 18

Before any of this: fhirbase fails at the first step against a default
PostgreSQL 18.

```
Error connecting to database: unknown authentication type: 10
```

Type 10 is `SCRAM-SHA-256`, the default password mechanism since PostgreSQL 10.
fhirbase's driver — `jackc/pgx` v3.2.0, from 2018 — predates support for it. The
benchmark only runs because the test server was reconfigured for `trust`
authentication.

That is not a defect in the catalogue's sense, and nothing here fixes it: it is
simply what happens to a tool whose dependencies stopped moving in 2019, and it
is the plainest argument for the port existing.

## Load throughput

| Input | Mode | fhirbase | fhirpg | Ratio |
| --- | --- | ---: | ---: | ---: |
| demo bundle (non-grouped) | `insert` | 4.95 s | **3.39 s** | 1.5× faster |
| demo bundle (non-grouped) | `copy` | 43.58 s | **43.23 s** | parity |
| sorted by type (grouped) | `insert` | — | 2.96 s | |
| sorted by type (grouped) | `copy` | — | **1.20 s** | |

Run-to-run spread was small: insert 4.95/4.95/5.07 for fhirbase and
3.39/3.42/3.45 for fhirpg; copy 43.58/45.27 and 43.23/43.51.

Both tools wrote identical row counts.

### Insert mode: 1.5× faster

Modest, and worth being precise about where it comes from. fhirbase issues one
parameterized `INSERT` per resource, pipelined 2,000 at a time; fhirpg issues
one multi-row `INSERT` per 2,000 resources per table. That is roughly 127,000
statements against roughly 100 — but the work is dominated by PostgreSQL either
way, so the saving is 30%, not a multiple.

Profiling agrees: of fhirpg's 3.55 s, 0.94 s is user CPU and 0.09 s is system.
Three quarters of the wall clock is waiting on the database.

### Copy mode: parity, and a 36× swing

The two implementations are the same speed, which is expected — both hand bytes
to `COPY` and wait.

What is worth noticing is the input:

```
copy on non-grouped input   43.2 s
copy on grouped input        1.2 s
```

A `COPY` targets one table, so every change of resource type ends the current
one and starts another. The demo bundle interleaves 15 types, so copy mode runs
a `COPY` per resource; sorted by type it runs fifteen. Spec §8.1 says copy is
"roughly three times faster on grouped input and slower on non-grouped"; measured
here it is **2.5× faster grouped and 13× slower non-grouped**.

This is what makes the mode defaults matter, and they are right: `insert` for
local files, whose ordering is unknown, and `copy` for Bulk Data, which arrives
grouped by resource type.

## Decision D12: UUIDv7 earns its place

D12 replaced `gen_random_uuid()` with `uuidv7()` at all three id-generation
sites, on the argument that time-ordered ids make insertion into the `id`
primary key mostly-append rather than random. T25's instruction was to report
the result **whatever it showed**.

500,000 rows into an otherwise identical table, ids generated per row:

| | `gen_random_uuid()` (v4) | `uuidv7()` | Difference |
| --- | ---: | ---: | ---: |
| Insert time | 1,623 ms | 1,359 ms | **16% faster** |
| Insert time, order reversed | 1,523 ms | 1,248 ms | **18% faster** |
| Primary key index | 36–37 MB | 28 MB | **24% smaller** |
| Table size | 80 MB | 80 MB | unchanged |

The run was repeated with the two inserts in the opposite order, so the result
is not warm-cache ordering. The index-size difference is not subject to timing
noise at all: a quarter less index, for ids of identical length, is exactly the
reduced page splitting D12 predicted.

D12 stands.

## Risk R2: text `COPY` is fine

R2 left text-versus-binary `COPY` to profiling. Copy mode over the demo bundle:

```
real 43.02 s    user 2.60 s    sys 4.52 s
```

fhirpg's own CPU is 7.1 s of 43 s, and most of that is system time — syscalls,
not escaping. Escaping is a fraction of the 2.60 s of user time. Binary `COPY`
would have to look up the `resource_status` enum's OID per database and could
recover at most a few percent of wall clock.

R2 settles: keep text format.

## A methodology note, because it nearly produced a wrong headline

The first insert-mode measurement had fhirbase at **331 seconds** against
fhirpg's 3.3 — a 100× gap that would have been the headline.

It was an artifact. The measurement piped fhirbase's stdout through `grep`, and
its progress bar (`vbauerster/mpb`) behaves pathologically when stdout is a
pipe rather than a terminal. With stdout discarded instead, the same load takes
5.15 seconds — and fhirbase's own reported "5 seconds", which I had described as
wrong, was right all along.

Every number here therefore discards stdout rather than piping it. A 100×
result that appears without an explanation for the mechanism deserves to be
disbelieved first.
