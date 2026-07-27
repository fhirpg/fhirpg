# 1. Scope

- **S1.1** fhirpg MUST support FHIR R5 (5.0.0), R4 (4.0.1), and R3 (3.0.2).
  R5 is the default everywhere a version is optional.
- **S1.2** Each FHIR version's data lives in its own PostgreSQL schema:
  `r5`, `r4`, `r3`. Versions are independent; a database MAY host any subset.
- **S1.3** All resource types defined by the version's specification MUST be
  supported — no unsupported-type errors for spec-defined types.
- **S1.4** Target database is PostgreSQL 18. Features requiring ≥18 MAY be
  used; older servers are unsupported.

---

Part of the [fhirpg specification](index.md).
