# 5. Versioning and history

- **H5.1** Every create/update/delete increments `version_id` and appends
  one row to `<resource>_history(id, version_id, last_updated, op char(1),
  resource jsonb)` where `op` ∈ C/U/D. History is an immutable audit
  archive; JSONB is acceptable there because it is written once and read
  only by vread/history/audit (decision D7, plan.md).
- **H5.2** Delete is soft at the API level (history row with op = D; base
  and child rows removed); a deleted id's history remains readable.
- **H5.3** vread serves any historical version from history; read serves the
  current version reconstructed from the relational tables. A checksum
  comparison between the two paths is part of the test suite, not runtime.

---

Part of the [fhirpg specification](index.md).
