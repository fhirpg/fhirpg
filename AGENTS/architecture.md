# Architecture

## Module tree

```text
src/
  main.rs            # thin: parse CLI, dispatch, set exit code
  cli.rs             # clap derive: global flags + 5 subcommands
  config.rs          # PgConfig; libpq env precedence; sslmode        (T9)
  db.rs              # connect, TLS policy, pool construction         (T9)
  error.rs           # thiserror error enum
  assets.rs          # embedded schema/transform/web + version registry (T5)
  transform.rs       # the fhirbase transformation algorithm          (T6)
  bundle/
    mod.rs           # Iterator<Item = Result<Value>> over resources  (T13)
    detect.rs        # gzip sniff; ndjson vs FHIR-Bundle vs single    (T12)
    ndjson.rs  fhir_bundle.rs  single.rs  multifile.rs                (T13)
  load/
    mod.rs           # Loader trait, progress, per-type tallies       (T16)
    copy.rs          # COPY FROM STDIN, re-issued per type run        (T15)
    insert.rs        # batched INSERT ... ON CONFLICT DO NOTHING      (T14)
  bulk.rs            # Bulk Data API: kickoff, poll, parallel download (T17)
  commands/
    init.rs  transform.rs  load.rs  bulkget.rs  web.rs
```

Parenthesized task numbers point into [`../tasks.md`](../tasks.md). Modules not
yet created belong to tasks not yet started; do not stub them ahead of time.

## Layering

Strictly downward. A module may use anything below it and nothing above it.

```text
commands/                    # user-facing orchestration, progress, reporting
   ↓
load/    bulk.rs             # the write path and the network path
   ↓
bundle/  transform.rs        # reading resources, rewriting resources
   ↓
assets.rs  db.rs  config.rs  # embedded data, connections, settings
   ↓
error.rs                     # typed errors, used by everything
```

Two consequences worth stating, because they are easy to erode:

- **`transform.rs` has no I/O and no database dependency.** It is a pure
  function from (JSON value, transformation map) to JSON value. That is what
  makes the algorithm — the highest-risk part of the port — unit-testable
  without PostgreSQL, exactly as it is in Go.
- **`bundle/` does not know about the database, and `load/` does not know about
  file formats.** They meet through an iterator of `serde_json::Value`.

## The hot path

`load` is the only performance-sensitive command:

```text
file(s) → gzip? → format detect → stream resources (serde_json::Value)
        → transform (assets/transform/<version>.json)
        → COPY or batched INSERT → PostgreSQL
```

Design rules for this path:

- Resources stay as `serde_json::Value`. They are deliberately **not**
  deserialized into the sibling `fhir` crate's typed R5 model: the loader must
  accept resources of any FHIR version, unknown resource types, and
  non-conforming data, exactly as fhirbase does. Typed validation is an opt-in
  `--validate` feature (T24), never the default path.
- Memory is bounded by the largest single resource, not by input size
  (spec invariant 6). A 1 GB input must not produce a 1 GB allocation. This is
  why `bundle/fhir_bundle.rs` streams the `entry[]` array rather than
  deserializing the whole document.
- Resource counts are **advisory**, for progress display only. Nothing about
  batching, flushing, or termination may depend on them — that dependency is
  defect X7 in the Go original.

## Where the sibling `fhir` crate fits

`~/git/joelparkerhenderson/fhir-rust-crate` (crate `fhir`) provides the FHIR R5
model and, more importantly here, `fhir::r5::meta` — an element-metadata table
derived from the official HL7 specification JSON. It is used in exactly two
places, both off the hot path:

1. **`xtask`** (T21), the one-shot generator that emits the FHIR 5.0.0 schema
   and transform assets that fhirbase never had.
2. **The optional `validate` feature** (T24).

It is deliberately **not** a default dependency of the binary, so `fhirpg` can
ship independently of that crate's publication status.
