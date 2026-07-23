# Glossary

## FHIR

**FHIR** — Fast Healthcare Interoperability Resources, the HL7 standard for
exchanging electronic health records. Pronounced "fire".

**Resource** — the unit of FHIR data: a JSON object with a `resourceType`
discriminator, such as `Patient`, `Observation`, or `Group`. R5 defines 158.

**R4 / R5** — FHIR releases. R4 is version 4.0.0; R5 is 5.0.0 and is this
project's default. fhirbase supports 1.0.2 through 4.0.0; the 5.0.0 assets are
generated here (decision D5).

**Choice element** — an element that can hold one of several types, written
`value[x]` in the specification and serialized with a type-suffixed key such as
`valueString` or `deceasedBoolean`. The transformation collapses these; see
spec §4.4.

**Reference** — a FHIR datatype pointing at another resource, typically as a
relative URL like `"Practitioner/1"`. The transformation splits it into
`resourceType` and `id`; see spec §4.5.

**Bundle** — a FHIR resource that contains other resources under `entry[]`.
One of the three input formats this tool reads.

**NDJSON** — newline-delimited JSON: one complete JSON object per line. What
Bulk Data API servers emit, and the format loads fastest here.

**Bulk Data API** — the SMART/HL7 asynchronous export protocol: kick off an
export, poll a status URL, then download the NDJSON files it lists.

## fhirbase

**fhirbase** — the Go program this project translates. Unmaintained since 2019.
Reference checkout: `~/github/fhirbase/fhirbase`.

**Transformation map** — the per-FHIR-version JSON document
(`fhirbase-import-<version>.json`) that drives the transformation algorithm,
keyed by type name and carrying `tr/*` directives.

**Directive** — a `tr/`-prefixed key in the transformation map: `tr/act`,
`tr/arg`, `tr/move`, `tr/isCollection`. See spec §4.1.

**Grouped input** — input where resources of the same type are adjacent, as
Bulk Data exports are. `copy` mode is roughly 3× faster on grouped input and
slower on non-grouped input; this is why the mode default differs between local
files and Bulk Data URLs.

**History table** — the `<resourcetype>_history` companion to each resource
table, holding superseded versions keyed by `(id, txid)`.

**txid** — the transaction id column. fhirbase writes `0` for every bulk-loaded
resource; `--txid=new` allocates a real one (defect X10).

## This project

**Green gate** — the four commands that must pass before any task is done. See
[`testing.md`](testing.md).

**D-numbers (D1-D14)** — settled decisions, in [`../plan.md`](../plan.md).

**X-numbers (X1-X11)** — catalogued fhirbase defects that the port fixes rather
than reproduces, in [`../plan.md`](../plan.md).

**T-numbers (T1-T27)** — ordered tasks, in [`../tasks.md`](../tasks.md).

**§-numbers** — sections of [`../spec/index.md`](../spec/index.md), the
normative specification.

**Fidelity oracle** — fhirbase itself, run against FHIR 1.0.2-4.0.0 to produce
reference transform output. It is the reason the legacy versions are retained
even though R5 is the default: without them there is nothing to check the
transformation algorithm against.
