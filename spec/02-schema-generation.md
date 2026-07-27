# 2. Schema generation

- **G2.1** DDL and relational maps MUST be generated from the official FHIR
  specification packages (StructureDefinitions, SearchParameters) by
  `fhirpg gen`, and the generated artifacts MUST be committed under
  `assets/` so that builds and installs never require the spec packages.
- **G2.2** Generation MUST be deterministic: same spec input → byte-identical
  output. `assets/CHECKSUMS.txt` records SHA-256 of every artifact.
- **G2.3** Identifier naming: element paths convert to snake_case
  (`birthDate` → `birth_date`). Table names concatenate the resource name and
  element path (`Patient.name.given` → `patient_name_given`).
- **G2.4** PostgreSQL truncates identifiers at 63 bytes. Where a generated
  name would exceed 63 bytes, the generator MUST abbreviate deterministically
  and, on residual collision, suffix with a 6-hex-digit hash of the full
  path. The full-path → identifier mapping MUST be recorded in the relational
  map and in a generated `doc/` index; two different paths MUST never map to
  the same identifier.
- **G2.5** `fhirpg init` MUST be idempotent and effectively atomic. Because
  creating thousands of tables in one transaction exceeds default PostgreSQL
  lock budgets (`max_locks_per_transaction`), init stages the install under
  a temporary schema (`r5__init`) in chunked transactions and then renames
  it into place in a single statement; a failed init leaves only the staging
  schema, which the next init removes. Init records the applied artifact
  checksum in `fhirpg_meta`, no-ops when the installed checksum matches, and
  refuses to run against a schema created from a different artifact (see §10
  migrations). Schema drops are likewise chunked.

---

Part of the [fhirpg specification](index.md).
