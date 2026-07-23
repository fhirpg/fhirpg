# Contributing

Thanks for your interest. This project is a Rust translation of
[fhirbase](https://github.com/fhirbase/fhirbase); the translation is
spec-driven, so the workflow is a little more structured than usual.

## Before you start

Read, in order:

1. [`AGENTS.md`](AGENTS.md) — commands, the green gate, house rules.
2. [`spec/index.md`](spec/index.md) — the normative behaviour.
3. [`plan.md`](plan.md) and [`tasks.md`](tasks.md) — what is being built, in
   what order, and why.

## Setup

```sh
git clone https://github.com/joelparkerhenderson/fhirpg/
cd fhirpg
cargo build --all-targets
```

For anything that touches the database you also need
[Podman](https://podman.io/) and PostgreSQL 18:

```sh
podman compose up -d
FHIRPG_TEST_DB="host=localhost port=5433 user=fhirpg password=fhirpg dbname=fhirpg" \
  cargo test -- --ignored
```

`cargo test` on its own is hermetic: it needs no database, no network, and no
container runtime.

## The green gate

Every change must leave these four clean:

```sh
cargo build --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

`clippy::pedantic` is on, and `unwrap_used` / `expect_used` / `panic` are
denied. That is intentional — see spec invariant 2.

## Making a change

1. **Branch first.** Never commit to the default branch.
2. **Check the spec.** If your change alters observable behaviour, update
   `spec/index.md` in the same commit. Code and spec must not drift.
3. **Cite the Go source** when porting: name the fhirbase file and line range in
   the commit message, e.g. `T12: port bundle format detection (load.go:36-194)`.
4. **Add a test.** Anything with a runtime surface gets one. Ported Go test
   cases are ported verbatim, not paraphrased.
5. **Run the gate**, then open a pull request describing what changed and which
   task or spec section it satisfies.

## Reporting a defect in fhirbase

If you find a defect in the Go original that we have not catalogued, that is
genuinely useful. Open an issue describing the Go file and line range, the
incorrect behaviour, and how it should behave instead. If it is real, it becomes
an X-number in `plan.md`, a spec requirement, and a regression test.

## Code of conduct

By participating you agree to abide by the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Licensing of contributions

Contributions are accepted under the same terms as the project: `MIT OR
Apache-2.0 OR GPL-2.0-only`. Note that material derived from fhirbase remains
under its MIT terms and its copyright notice must travel with it — see
[`LICENSE.md`](LICENSE.md).
