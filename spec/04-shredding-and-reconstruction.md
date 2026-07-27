# 4. Shredding and reconstruction

- **R4.1** Shredding (JSON → rows) and reconstruction (rows → JSON) are
  driven by the generated relational map through one generic engine; no
  per-resource handwritten code.
- **R4.2** Round-trip MUST be lossless: for any valid resource,
  `reconstruct(shred(r))` is semantically identical JSON — same values
  (including decimal precision and partial dates), same array order, key
  order not significant. This invariant is enforced by property tests over
  spec examples and generated resources.
- **R4.3** Unknown elements (not in the version's spec) MUST be rejected
  with an error naming the path — never silently dropped.
- **R4.4** A resource write (shred + delete-old-rows + insert) MUST be a
  single transaction.
- **R4.5** A resource **read** MUST likewise see a single snapshot. A read
  touches one base table and many child tables; issued as independent
  statements, a concurrent write between them would reconstruct a resource
  that never existed — base columns from one version, child rows from the
  next. Every multi-statement read (`get`, `export`, search result
  materialization) MUST therefore run inside one `REPEATABLE READ READ ONLY`
  transaction. This is a correctness requirement, not a tuning knob.
- **R4.6** Resource ids MUST satisfy the FHIR `id` production
  (`[A-Za-z0-9\-\.]{1,64}`) wherever they enter the system — URL path, body,
  or Bundle entry. An id that does not is a 400, never a stored row.

---

Part of the [fhirpg specification](index.md).
