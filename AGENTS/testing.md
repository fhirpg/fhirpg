# Testing

## The green gate

Run all four before finishing any task:

```sh
cargo build --all-targets
cargo test                                    # unit tests AND doctests
cargo clippy --all-targets -- -D warnings     # zero warnings; pedantic is on
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

A change that reduces the passing count is a regression. CI enforces the same
four, plus the MSRV (1.88) and the database suite.

## Panicking in tests

`unwrap_used`, `expect_used`, and `panic` are denied crate-wide (spec invariant
2). Test code is exempt, because panicking is how a test reports — but the
exemption has to be applied in two different ways:

- **Unit tests** inside `src/` are covered by the `#![cfg_attr(test, allow(…))]`
  at the top of `src/main.rs`. Nothing to do.
- **Integration tests** in `tests/` are separate crates compiled as normal
  binaries, so `cfg(test)` is false there and the crate attribute does not
  reach them. Each file needs its own header:

  ```rust
  #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
  ```

## Hermetic by default

`cargo test` must pass on a machine with no PostgreSQL, no network, and no
Podman. Tests that need a database are marked `#[ignore]` and read the
connection string from `FHIRPG_TEST_DB`:

```rust
fn test_db() -> Option<String> {
    std::env::var("FHIRPG_TEST_DB").ok()
}

#[test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
fn init_creates_schema() { /* … */ }
```

Run them with:

```sh
podman compose up -d
FHIRPG_TEST_DB="host=localhost port=5433 user=fhirpg password=fhirpg dbname=fhirpg" \
  cargo test -- --ignored
```

Never make a test depend on the network. The Bulk Data API client (T17) is
tested against `wiremock`, not against a live server.

## Test corpora

- **Ported Go cases are ported verbatim.** The five transform cases from
  `transform_test.go` and the five detection cases from `load_test.go` are the
  translation's primary evidence. Do not paraphrase them; if one fails, the
  port is wrong, not the case.
- **Fidelity comparisons** run our transform against fhirbase's recorded output
  and compare as `serde_json::Value`, not as bytes — object key order is not
  significant (spec invariant 4).
- **Property tests** (`proptest`) cover the invariants that no example can:
  unknown `resourceType` is the identity, no input panics, output is always
  valid JSON. The case count comes from proptest's default locally and from
  `PROPTEST_CASES=10000` in CI, so the edit-test loop stays fast.

  When a property fails, **check the implementation against fhirbase before
  changing either.** The first failure found here was a wrong *property*, not a
  wrong implementation: it assumed a `union` always yields an object, when a
  union over an array yields an array of wrapped elements. The oracle settled
  it in one command.

## Defect regression tests

Each catalogued fhirbase defect (X1-X11 in `plan.md`) gets a test that would
fail against the Go behaviour. These are the most valuable tests in the suite,
because they are the reasons the port exists. The sharpest one: loading a FHIR
`Group` resource, which fhirbase's default insert mode cannot do at all.

## Concurrency tests

T11a's stored-procedure suite includes a concurrency test for the `RETURNING OLD`
race (decision D13). It must be **deterministic** — advisory locks or explicit
statement ordering, never `sleep`. A flaky test here is worse than no test,
because D13's justification rests on it.

## Benchmarks

T25 compares against the Go binary on a fixed corpus. Report what the numbers
show, including when they are unflattering: if UUIDv7 (D12) shows no
index-locality benefit on this workload, that goes in the README and D12 gets
reopened.
