# Changelog

## Unreleased — 2.0.0-dev (ground-up rewrite, 2026-07)

A complete rewrite: FHIR resources now live in **fully normalized
relational tables** (typed columns, child tables per repeating element,
`ords smallint[]` ordinal paths) instead of jsonb bodies. The prior
fhirbase-style implementation remains in git history.

- Spec-driven generator: DDL + relational map assets for R5 5.0.0,
  R4 4.0.1, R3 3.0.2 (158/146/117 resource types; 7,355 tables for R5).
- Lossless round trip proven against all 7,399 official spec examples —
  in memory and through live PostgreSQL 18 — plus 10,000 map-driven
  property-test cases (which caught, and fixed, an ordinal ambiguity
  between QuestionnaireResponse's two recursive item paths).
- Extensions, primitive extensions, and element ids stored relationally
  as typed leaf rows; the Reference↔Identifier type cycle spills to a
  relational leaf table; no live data in JSONB.
- Store: transactional create/update/delete with append-only history,
  vread, 404-vs-410 status, optimistic concurrency (If-Match), and
  multi-op all-or-nothing transactions.
- Search: 94.8% of R5 SearchParameters compiled to indexed SQL
  (token/string/date/number/quantity/reference/uri), `_sort`, `_total`,
  paging; strict unsupported-parameter reporting.
- FHIR REST server (`fhirpg serve`): every installed version mounted,
  CRUD + history + vread, search with next-link paging, batch and
  transaction Bundles with urn:uuid resolution, generated
  CapabilityStatements, OperationOutcome errors, request ids, Prometheus
  metrics, graceful shutdown, pool wait-timeout → 503 + Retry-After,
  server-side statement timeouts.
- CLI: gen, init (staged-schema, idempotent, checksum-guarded), load
  (optional `--validate` through the typed fhir crate model, R5),
  get/delete/export/search/transform/drop/serve.
