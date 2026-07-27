# 6. Search

- **P6.1** All standard SearchParameters of each version MUST be compiled by
  the generator into SQL predicate templates over the normalized columns.
  Search types supported: token, string, date, number, quantity, reference,
  uri; composite and special parameters MAY be deferred (documented per
  parameter in generated docs).
- **P6.2** String search default is case-insensitive prefix match
  (`:exact` and `:contains` modifiers supported). Token search matches
  `system|code` semantics. Date search implements FHIR range/prefix
  semantics (eq, ne, lt, gt, ge, le, sa, eb) against the `_sort` columns
  with precision-aware ranges.
- **P6.3** Result parameters: `_count` (default 50, max 1000), paging via
  opaque cursor, `_sort` on searchable params, `_id`, `_lastUpdated`,
  `_total=accurate|estimate`, `_include`/`_revinclude` (single hop).
- **P6.4** The generator MUST emit indexes for: every base-table search
  column, every child-table FK + ord, reference `(ref_type, ref_id)` pairs,
  and token `(system, code)` pairs.
- **P6.5** Unsupported search parameters MUST return an OperationOutcome
  warning and be ignored per FHIR's lenient handling, or error under
  `Prefer: handling=strict`.
- **P6.6** String search MUST be insensitive to **case, accents, and
  Unicode composition** — FHIR requires it, and a system serving Ærø,
  Ångström, Müller, Muñoz, and Ślusarczyk cannot ship `ILIKE` alone. Each
  string search target column gets a companion `_norm` column holding the
  folded value, **computed by the engine in Rust at write time** (NFD, drop
  combining marks, lowercase, drop marks again). Queries fold the search
  term with the same function and compare against that column, so there is
  exactly one definition of string equality in the system rather than one in
  SQL and one in Rust that must agree for every codepoint. The column is
  declared `COLLATE "C"`, so ordering is by Unicode codepoint.

  A prefix search MUST be emitted as a **range predicate** — `col >= term
  AND col < upper(term)`, with the upper bound computed in Rust — not as
  `LIKE $1 || '%'`. PostgreSQL extracts a prefix from a *constant* pattern
  only, so a `LIKE` against a bound parameter degrades to a sequential scan
  in the generic plan while looking correct in any hand-run `EXPLAIN` with a
  literal. `:exact` compares the stored column, not the folded one, because
  it is defined as the literal string. `:contains` folds both sides and
  remains a scan, as an unanchored match must.

  This fixes P6.2's semantics and T15's unindexed-prefix note in one move,
  and requires **no PostgreSQL extension**: an earlier design built on
  `unaccent` needed an IMMUTABLE wrapper, an expression index the planner
  would not use with a parameterized pattern, and a deployment-time check
  for an extension that managed-Postgres tenants often cannot install.
- **P6.7** A single search request MUST have a bounded cost. Result
  materialization MUST batch (one query per resource type, not one per
  resource); `_include`/`_revinclude` expansion MUST be capped and, when the
  cap truncates, MUST add an OperationOutcome warning to the bundle. Silent
  truncation of clinical results is a patient-safety defect, not a
  performance trade.

---

Part of the [fhirpg specification](index.md).
