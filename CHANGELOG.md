# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Project scaffolding: `AGENTS.md` and `AGENTS/` guidance, `CLAUDE.md`,
  `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `CITATION.cff`, `cspell.json` (T1).
- Package manifest with the multi-license offer and the upstream fhirbase
  attribution required by its MIT terms (T2).
- `compose.yaml` for a PostgreSQL 18 test instance under Podman, and GitHub
  Actions CI running the green gate, the MSRV check, and the database suite (T3).
- Typed error enum and the `clap` command-line skeleton: global connection flags
  and the five subcommands `init`, `transform`, `load`, `bulkget`, `web` (T4).
- The FHIR transformation algorithm and the `transform` subcommand, verified
  against fhirbase itself over 38 resources at 4 FHIR versions — 152 exact
  matches — plus 8 property tests (T5-T8).
- Connection configuration for all six libpq `sslmode` values, and a
  PostgreSQL 18 version gate (T9).
- The `init` subcommand: 305 statements for FHIR 4.0.0 (T10, T11).
- The stored-procedure regression suite, including a deterministic concurrency
  test for the history-archival race (T11a).
- UUIDv7 identifier generation (T11b) and the `RETURNING OLD` archival rewrite,
  which also fixes X13, X14, and X15 (T11c).

### Notes

This release is planning and scaffolding only; no command does anything yet.
The delivery plan is in [`plan.md`](plan.md), the ordered task list in
[`tasks.md`](tasks.md), and the normative behaviour in
[`spec/index.md`](spec/index.md).

Deliberate divergences from fhirbase, decided during planning:

- Requires **PostgreSQL 18** (D8) and defaults to **FHIR R5 / 5.0.0** (D4),
  where fhirbase targets PostgreSQL 10 and defaults to FHIR 3.3.0.
- Drops fhirbase's usage telemetry and binary self-update (D1).
- Renames the SQL stored procedures `fhirbase_*` to `fhirpg_*`, which gives up
  drop-in compatibility with databases initialized by fhirbase (D3).
- Generates ids as **UUIDv7** rather than v4, at all three generation sites (D12).
- Takes the history pre-image from `RETURNING OLD` rather than a sibling CTE (D13).
- `--memusage` reports operating-system RSS, not Go GC statistics (D14).
- Fixes seventeen catalogued defects in the Go original (X1-X17 in `plan.md`),
  including an SQL injection vector and an inability to load FHIR `Group`
  resources at all.
