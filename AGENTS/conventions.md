# Conventions

## Errors

- One typed error enum per crate, in `src/error.rs`, derived with `thiserror`.
  Library code returns `Result<T, Error>`. `anyhow` appears in `main.rs` only.
- **Never `unwrap`, `expect`, or `panic!` on a value derived from input** —
  file content, network responses, database output, or command-line arguments.
  `Cargo.toml` denies those lints, so this is enforced, not requested.
- If an `#[allow]` is genuinely needed, it carries a comment explaining why the
  panic is unreachable. "It can't happen" is not an explanation; "the asset is
  validated at load time by `assets::validate`" is.
- Error messages name the source: the file and line for a bundle error, the
  statement index for an `init` error, the URL for a bulk error.

## Secrets

The database password must never appear in logs, `--help`, `Debug`, `Display`,
error messages, or a connection banner. Wrap it in a newtype whose `Debug` and
`Display` print `<redacted>`. fhirbase prints it in cleartext in two places
(defect X6); there is a test asserting we do not.

## SQL identifiers

Any identifier derived from data — every resource-type table name — must be
validated against the version's known resource set and then quoted. Never
`format!` an identifier into SQL unquoted. This is defect X2, and it is not
theoretical: FHIR has a `Group` resource, `group` is a PostgreSQL reserved
word, and fhirbase's insert loader cannot load one.

## Preserve, do not "fix"

Two behaviours look like bugs and are not. They are fhirbase's storage model,
asserted by its tests, and must be reproduced exactly:

1. **The `reference` transform is lossy.** It keeps only `id`, `resourceType`,
   and `display`, discarding `identifier`, `type`, and extensions from a FHIR
   `Reference`. See spec §4.5.
2. **`tr/isCollection` is never read.** It appears throughout the transform
   assets, but the array branch of the algorithm already recurses with the same
   transform node, which handles repeating fields correctly. Ignore it, and
   emit it in generated assets for consistency. See spec §4.7.

If you find yourself "improving" either, stop and re-read the spec.

## Rustdoc

- Every public item is documented. `RUSTDOCFLAGS="-D warnings"` is in the gate.
- Doc comments on ported units cite the Go source: `` /// Ports `load.go:113-141`. ``
  This is the fastest way for a reader to check the translation.
- Anything with a runtime surface gets a `# Examples` doctest where it is
  runnable without a database.

## Assets

`assets/` vendored from fhirbase is byte-identical by contract and verified by
checksum in a test. Do not reformat, re-indent, or "clean up" those files. The
only sanctioned exceptions are stated in the spec: `functions.sql.json`
(rebranded per D3) and the generated FHIR 5.0.0 assets (per D5).

## Commits

- Branch first; never commit to the default branch.
- Name the task and the Go source in the subject or body, e.g.
  `T12: port bundle format detection (load.go:36-194)`.
- Agents end commit messages with the `Co-Authored-By` trailer.
