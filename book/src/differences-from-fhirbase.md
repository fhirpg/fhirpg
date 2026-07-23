# Differences from fhirbase

This is a translation, not a fork. The storage model, the transformation
algorithm, and the command surface are fhirbase's. What changed is recorded as
decisions D1–D15 and defect fixes X1–X17 in `plan.md`; this chapter covers what
you would actually notice.

## Things that will affect you

**The stored procedures are named `fhirpg_*`, not `fhirbase_*`.** A database
initialized by fhirbase has the other set, and neither tool recognizes the
other's. There is no migration; treat them as separate tools.

**PostgreSQL 18 is required**, where fhirbase targets 10. Not a preference:
`uuidv7()` and `RETURNING OLD` both arrived in 18.

In the other direction — **fhirbase cannot connect to PostgreSQL 18 at all.**
Its driver predates SCRAM-SHA-256, the default password mechanism since
PostgreSQL 10, and fails with `unknown authentication type: 10`.

**FHIR R5 is the default**, where fhirbase defaults to 3.3.0.

**Generated ids are UUIDv7**, not v4. They sort by creation time and index
better; they also embed a timestamp.

**No telemetry.** fhirbase posts usage events to its vendor's endpoint on every
run unless you pass `--nostats`, and its web console carries two third-party
analytics trackers. There is nothing here to disable.

**No self-update.** `fhirbase update` downloaded a new binary from its vendor's
GitHub releases. Install with `cargo install` or a release archive.

## Bugs that are fixed

The ones most likely to have bitten you:

- **A FHIR `Group` could not be loaded at all.** fhirbase's insert loader —
  its *default* mode — builds `INSERT INTO group`, and `group` is a PostgreSQL
  reserved word. Same code path is an injection vector, since the table name
  comes from the resource's own `resourceType`.
- **Deleting a bulk-loaded resource failed.** `fhirpg_delete` wrote two history
  rows, colliding on the primary key whenever the supplied `txid` matched the
  row's — and every bulk-loaded row has `txid = 0`.
- **Deletes were indistinguishable from updates in history.** The
  `resource_status` enum has a `deleted` value that nothing ever wrote.
- **History could record the wrong version.** Under concurrency, the row
  archived was read from a different snapshot than the row actually replaced, so
  a committed version could vanish from history. Demonstrated with a
  deterministic test, then fixed.
- **Format detection buffered the whole file.** A compact FHIR Bundle has no
  newlines, and detection read "the first line". A 1 GB bundle allocated 1 GB
  before loading began.
- **A partial Bulk Data export loaded silently.** If some files failed to
  download, fhirbase loaded the rest — and an incomplete export is
  indistinguishable from a complete one once it is in the database.
- **The connection banner printed your password in cleartext.**

## Things that are deliberately the same

Two behaviours look like bugs and are not. They are the storage model, and
changing them would make this a different tool:

- **The reference rewrite is lossy.** `identifier`, `type`, and extensions on a
  `Reference` do not survive.
- **The transformation is not idempotent.** Transforming an already-transformed
  resource is not a no-op. Never re-transform data read back out of the
  database.

## One place the output shape changed

`fhirpg_delete` writes **one** history row where fhirbase writes two. Nothing is
lost — the row carries the content that was live at deletion, and every earlier
version was already archived by the create or update that superseded it — but if
you count history rows, the number differs.
