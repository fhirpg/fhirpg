# AGENTS.md

Guidance for AI coding agents working in this repository, following the open
[AGENTS.md](https://agents.md) convention. Human contributors are welcome to
read it too.

## What this project is

`fhirpg` is a command-line utility that imports [FHIR](https://www.hl7.org/fhir/)
data into a PostgreSQL database and stores it relationally: one table per
resource type, resource bodies as `jsonb`, plus history tables and stored
procedures for CRUD.

It is a **Rust translation of [fhirbase](https://github.com/fhirbase/fhirbase)**,
a Go utility by Health Samurai that is no longer maintained. The Go source is
the specification input, not a thing to copy blindly: the translation fixes
sixteen catalogued defects and modernizes the target platform. See
[`plan.md`](plan.md).

## Repository shape

```text
Cargo.toml            # package `fhirpg`; binary `fhirpg`
src/
  main.rs             # thin: parse CLI, dispatch, map error to exit code
  cli.rs              # clap derive: global flags + 5 subcommands
  error.rs            # typed error enum (thiserror)
assets/               # embedded SQL schema, transform maps, web console
spec/                 # the living specifications — the source of truth
AGENTS/               # operational guidance for agents (this folder)
plan.md               # phased delivery plan and the decision log (D1-D14)
tasks.md              # ordered, executable tasks (T1-T27) with acceptance criteria
```

## Commands you must be able to run

| Task | Command |
| --- | --- |
| Build | `cargo build --all-targets` |
| Test (unit + doctests) | `cargo test` |
| Lint (pedantic; must be 0) | `cargo clippy --all-targets -- -D warnings` |
| Docs (deny warnings) | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` |
| Start test PostgreSQL | `podman compose up -d` |
| Stop it | `podman compose down` |
| Database tests | `FHIRPG_TEST_DB=… cargo test -- --ignored` |

## The prime directive: keep it green

Before you consider any task finished, **all four must pass**:

```sh
cargo build --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

`clippy::pedantic` is on, and `unwrap_used`, `expect_used`, and `panic` are
**denied** in `Cargo.toml`. That is deliberate: spec invariant 2 says no code
path reachable from input may panic, and these lints make it mechanical rather
than a promise. If a lint fires, fix the code — do not add an `#[allow]` without
a comment saying why the panic is unreachable.

## How to work here

- **The specs in `spec/` are the source of truth.** Behaviour is defined in
  [`spec/index.md`](spec/index.md) first, then implemented. When code and spec
  disagree, reconcile them — do not silently diverge.
- **Work the task list.** [`tasks.md`](tasks.md) is ordered and each task names
  its dependencies and acceptance criteria. Do not start a task whose
  dependencies are unfinished; the ordering encodes real constraints (most
  sharply at T11a, which must land before T11b and T11c).
- **Cite the Go source.** When porting a unit, name the fhirbase file and line
  range in the commit message. `~/github/fhirbase/fhirbase` is the reference
  checkout.
- **Fix the catalogued defects, do not reproduce them.** X1-X16 in `plan.md`.
  Equally: two surprising behaviours are *intentional* and must be preserved —
  see `AGENTS/conventions.md`.
- **Small, verifiable changes.** Anything with a runtime surface gets a test.

## Map of the guidance

| Document | Purpose |
| --- | --- |
| [`AGENTS/architecture.md`](AGENTS/architecture.md) | Module tree, layering, data flow |
| [`AGENTS/conventions.md`](AGENTS/conventions.md) | Code conventions and the preserve-vs-fix rules |
| [`AGENTS/testing.md`](AGENTS/testing.md) | Test patterns, the green gate, database tests |
| [`AGENTS/glossary.md`](AGENTS/glossary.md) | FHIR, fhirbase, and project terminology |
| [`spec/index.md`](spec/index.md) | The living specifications |
| [`plan.md`](plan.md) | Phases, decisions D1-D14, divergences X1-X16, risks |
| [`tasks.md`](tasks.md) | Ordered tasks T1-T27 |

## House rules

- Keep every file in `AGENTS/` and `spec/` under **40 KB**; split if it grows.
- Do not add dependencies without cause, and annotate each one inline in
  `Cargo.toml` with a comment saying what it is for.
- Never commit to the default branch; branch first. End commit messages with
  the `Co-Authored-By` trailer if you are an agent.
- `assets/` vendored from fhirbase is **byte-identical by contract** and
  checksum-verified. Do not edit it. The only exceptions are stated in the spec:
  `functions.sql.json` (rebranded, D3) and the generated R5 assets (D5).
