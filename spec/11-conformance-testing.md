# 11. Conformance testing

- **T11.1** Round-trip property tests (R4.2) over every example resource
  shipped with each FHIR specification, plus proptest-generated resources.
- **T11.2** Live-database integration tests exercise every REST interaction
  in §7 against PostgreSQL 18 in CI (docker compose).
- **T11.3** Search semantics tests derive cases from the FHIR search
  specification per parameter type, including precision-edge dates and
  token system matching.
- **T11.4** The CapabilityStatement MUST be generated from what is actually
  implemented (the relational map + supported params), never hand-edited.
- **T11.5** Load/serve benchmarks are tracked in `doc/benchmarks.md`; a
  regression gate compares against the recorded baseline.
- **T11.6** Concurrency is tested adversarially, not assumed: a reader
  looping against a writer MUST never observe a torn resource (R4.5); N
  racing conditional creates with identical criteria MUST produce exactly
  one resource (A7.10); N racing `If-Match` updates MUST produce exactly one
  success and N-1 412s.
- **T11.7** A redaction test asserts that no log line emitted during a full
  CRUD + search cycle over a resource containing a distinctive marker value
  ever contains that marker (O10.2), and that no OperationOutcome on the
  wire echoes a submitted value (A7.11).
- **T11.8** An audit test asserts that every write records its principal
  (M3.15), that every read appends an access record (PR12.5), that the hash
  chain verifies in every configured algorithm (M3.16, M3.16a), and that a
  direct `UPDATE`/`DELETE` on a history table is rejected by the database
  (M3.17). A test MUST also assert that tampering is caught **independently
  by each algorithm**, since a chain that only ever fails in one of them
  proves nothing about the others. A test MUST assert that a **truncated**
  chain still verifies clean while the checkpoint changes (M3.16c) — that
  gap is the checkpoint's whole reason for existing, and a test that only
  checked the checkpoint moved would not show it. A test MUST assert that
  rotating a key leaves history signed under the retired key verifiable, and
  that dropping that key yields *unverifiable*, never a break (M3.16b).
- **T11.9** Adversarial input MUST be covered by fuzz targets that are run,
  not merely committed. The REST server accepts documents from the network,
  so its parsers MUST be fuzzed on every change with a bounded time budget
  and a committed seed corpus, and a crash, panic, abort, or stack overflow
  MUST fail the build. A stack overflow is not unwindable: it is not caught
  by `catch_unwind`, a worker thread cannot contain it, and the process ends.
  For a server holding clinical data, one request ending the process is a
  denial of service that requires no cleverness. The sibling `fhir` crate's
  XML reader aborted on roughly 160 KB of nested input, well under any sane
  request-size limit, and nothing detected it for the life of the module.
- **T11.10** A test asserting a defect is fixed MUST be shown to fail without
  the fix. Reverting the fix, or mutating the code it guards, MUST make the
  test fail; a test not verified this way is presumed decorative until it is.
  This matters most for the tamper-evidence tests in T11.8, where a test that
  cannot fail is indistinguishable from a control that works — and the
  distinction is the entire value of the control.
- **T11.11** A regression MUST be pinned by the narrowest assertion that
  catches it. Prefer an exact value or a named set over a threshold: a floor
  of "at least 20" tolerates losing four of twenty-four, and "more than zero"
  tolerates losing all but one. Where the expected set is large, commit it as
  a snapshot so a regression names what changed, and keep regeneration an
  explicit, reviewed step so a shrinking baseline cannot be adopted by
  accident.
- **T11.12** Coverage MUST NOT degrade silently. A check that skips — because
  a corpus is absent, a database is unreachable, or a path could not be
  resolved — MUST say so, and MUST fail if it ends up checking nothing. A
  skip is indistinguishable from a pass in a CI summary. The corpus test here
  located its inputs through an absolute path into one machine's temporary
  directory: it skipped silently in CI for its whole life, and on the machine
  where that directory survived it reported a data-fidelity failure that was
  really a missing fetch. Inputs MUST be resolved relative to the repository
  or an environment variable, never an absolute path outside it.

---

Part of the [fhirpg specification](index.md).
