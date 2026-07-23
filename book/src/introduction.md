# Introduction

`fhirpg` imports [FHIR](https://www.hl7.org/fhir/) data into a PostgreSQL
database and lets you work with it as ordinary relational data: one table per
resource type, resource bodies as `jsonb`, with history tables and stored
procedures for create, read, update, and delete.

It is a Rust translation of [fhirbase](https://github.com/fhirbase/fhirbase), a
Go utility by Health Samurai that has been unmaintained since 2019.

## What it is for

You have FHIR data — a Bulk Data export, a pile of NDJSON, a directory of
Bundles — and you want to ask questions of it with SQL rather than by walking
JSON in application code.

```sql
SELECT resource->>'gender' AS gender, count(*)
  FROM patient
 GROUP BY 1 ORDER BY 2 DESC;
```

That is the whole idea. The resources stay as FHIR — nothing is flattened into
columns you would then have to maintain — but they live in tables you can join,
index, and aggregate.

## What it is not

- **Not a FHIR server.** There is no REST API, no search parameters, no
  FHIRPath. Querying is SQL.
- **Not a validator.** It stores what you give it. There is an opt-in
  `--validate` that *reports* non-conformance, but it never refuses data.
- **Not a FHIR library.** If you want the typed model in Rust, that is the
  [`fhir`](https://crates.io/crates/fhir) crate, which `fhirpg` uses for its
  optional validation.

## Why a translation

fhirbase's design is good and worth keeping. Its implementation stopped in 2019,
and it shows: it cannot connect to a current PostgreSQL at all, because its
driver predates SCRAM authentication. Beyond that, reading the source turned up
seventeen defects — among them an SQL injection vector, an inability to load
FHIR `Group` resources, and a web console shipping two third-party analytics
trackers on a page rendering patient data.

Those are catalogued as X1–X17 in `plan.md`, each with a regression test. The
chapter on [differences from fhirbase](differences-from-fhirbase.md) covers the
ones you would notice.
