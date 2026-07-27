# 9. Validation

- **V9.1** Structural validation (element existence, cardinality, primitive
  lexical rules, choice exclusivity, required bindings) always runs — it is
  inherent to shredding against the map.
- **V9.2** `--validate` (CLI) / `X-Fhirpg-Validate: strict` (server config
  default-on) additionally deserializes through the typed `fhir` crate model
  for the resource's version and rejects on any mismatch.
- **V9.3** Validation failure at the API returns 422 with an
  OperationOutcome listing each issue with a FHIRPath-style location.

---

Part of the [fhirpg specification](index.md).
