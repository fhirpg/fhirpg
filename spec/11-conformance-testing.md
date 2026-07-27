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

---

Part of the [fhirpg specification](index.md).
