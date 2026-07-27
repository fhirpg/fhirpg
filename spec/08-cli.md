# 8. CLI

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

---

Part of the [fhirpg specification](index.md).
