# 7. REST API

- **A7.1** The server mounts each installed version at `/{r3|r4|r5}` and
  implements: `GET  {base}/metadata` (CapabilityStatement),
  instance `GET/PUT/DELETE {base}/{type}/{id}`, `GET …/_history` and
  `GET …/_history/{vid}`, type-level `POST {base}/{type}` (create) and
  `GET {base}/{type}` (search, also via `POST …/_search`), and system
  `POST {base}` for batch and transaction Bundles.
- **A7.2** JSON is the required format (`application/fhir+json`); XML is
  available behind the `xml` feature via the fhir crate.
- **A7.3** Concurrency: responses carry `ETag: W/"{version_id}"`; PUT and
  DELETE honor `If-Match` and MUST return 412 on mismatch. Conditional
  create/update/delete via `If-None-Exist` and conditional references in
  transactions MUST be supported.
- **A7.4** Transactions are a single database transaction with FHIR
  processing order (DELETE, POST, PUT, GET), urn:uuid reference resolution,
  and all-or-nothing semantics. Batch entries are independent.
- **A7.5** Every error is an OperationOutcome with correct HTTP status
  (400 malformed, 404 unknown id/type, 405, 409/412 version conflict,
  410 deleted, 422 rejected resource, 500 with an opaque incident id —
  internal detail goes to logs, never to clients).
- **A7.6** Request bodies are capped (default 32 MiB, configurable);
  overlong URLs and pathological search inputs are rejected 414/400.
- **A7.7** Absolute URLs the server emits (`Bundle.entry.fullUrl`, `link`,
  `Location`) MUST come from a **configured** service base URL
  (`--base-url`), not from the request's `Host` header. `Host` and
  `X-Forwarded-Proto`/`-Host` are honored only when `--trust-proxy` is set,
  and then only for hosts on an allowlist. Deriving links from an
  attacker-controlled header lets a caller poison the next-page URLs that a
  client will follow.
- **A7.8** Responses that can carry PHI MUST set `Cache-Control: no-store`
  and `Pragma: no-cache`; every response MUST set
  `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`.
  Cross-origin access is denied unless `--cors-origin` names an origin
  explicitly; there is no permissive default.
- **A7.9** `Bundle.entry.request.ifMatch`, `ifNoneExist`, and
  `ifModifiedSince` MUST be honored inside batch and transaction entries, or
  the entry MUST fail with 501. Accepting a precondition and ignoring it is
  worse than not supporting it: the client believes it has concurrency
  control that it does not have.
- **A7.10** Conditional interactions (`If-None-Exist`, conditional update,
  conditional delete) MUST be atomic with respect to concurrent requests
  with the same criteria: the match and the write happen in one transaction,
  serialized by a transaction-scoped advisory lock on the criteria. A
  search-then-write that races produces duplicate patients.
- **A7.11** Client-visible diagnostics MUST identify *what rule was broken
  and where* (element path, rule id, parameter name). They MAY quote the
  caller's own query terms — the caller just sent them — but MUST NOT
  contain stored resource content, schema detail, or database text. Internal
  error strings go to the log behind an incident id (A7.5) and do not reach
  the wire. The store's error type MUST make this distinction structurally,
  so that "safe to return" is a type, not a habit.
- **A7.12** The generated CapabilityStatement MUST declare what is actually
  implemented — `conditionalCreate`, `conditionalUpdate`, `conditionalDelete`,
  `searchInclude`, `searchRevInclude`, `readHistory`, `versioning`, the
  `security` block naming the deployment's authentication, and the
  interaction list per resource. An over-claiming CapabilityStatement is a
  conformance defect (T11.4).

---

Part of the [fhirpg specification](index.md).
