# 3. Storage model

### Base tables

- **M3.1** Every resource type gets a base table named for the resource
  (`r5.patient`). Its primary key is `id text`.
- **M3.2** Base-table system columns: `id text PRIMARY KEY`,
  `version_id bigint NOT NULL` (monotonic per resource, starts at 1),
  `last_updated timestamptz NOT NULL`. `Resource.meta` is otherwise stored
  like any other element.
- **M3.3** Every scalar (non-repeating, primitive-typed) element of the
  resource becomes a typed column on the base table.

### Child tables

- **M3.4** Every **repeating** element becomes a child table. A child table
  carries:
  - `rid text NOT NULL` — the root resource id, FK to the base table with
    `ON DELETE CASCADE`,
  - `ords smallint[] NOT NULL` — the 1-based index at each repeating
    ancestor crossing from the resource root down to and including this
    element (`{2,1}` = second parent instance, first child instance),
  - primary key `(rid, ords)`,
  - typed columns for every scalar element reachable without crossing
    another repeating element.
  The array form (rather than one ordinal column per level) is what lets
  recursive elements (`Questionnaire.item.item`, via `contentReference`)
  share one table at any depth: recursion appears as longer `ords` paths.
- **M3.5** Non-repeating complex elements (datatypes and backbone elements)
  **flatten** into the nearest enclosing table as prefixed columns
  (`Patient.maritalStatus.text` → `patient.marital_status_text`); only their
  repeating descendants open tables. Three exceptions force a table for a
  non-repeating element, with a fixed ordinal of 1: (a) a flattened width
  that would approach PostgreSQL's 1600-column limit (generator threshold
  150 columns — this catches the open `value[x]` choices with ~54 types),
  (b) backbone elements targeted cyclically by a `contentReference`
  (`ImplementationGuide.definition.page`), and (c) nothing else. There are
  no shared "coding" tables; each usage site owns its rows.

### Type mapping

- **M3.6** FHIR primitive → PostgreSQL column types:

  | FHIR | PostgreSQL |
  | --- | --- |
  | boolean | `boolean` |
  | integer, unsignedInt, positiveInt | `integer` |
  | integer64 (R5) | `bigint` |
  | decimal | `numeric` — original textual precision MUST survive round-trip |
  | string, code, id, markdown, uri, url, canonical, oid, uuid, xhtml, base64Binary | `text` |
  | date | `text` + derived `date` column `<name>_sort` |
  | dateTime, instant | `text` (verbatim) + derived `timestamptz` column `<name>_sort` for ordering/search |
  | time | `text` (fractional-second lexical fidelity) |

  Partial dates ("2026", "2026-07") make FHIR temporal values
  non-representable in native types without loss, hence verbatim text plus a
  derived sort column, computed by the engine at write time (partial values
  sort at their period start; offset-less dateTimes sort as UTC).
- **M3.7** Elements bound `required` to a FHIR value set get a
  `CHECK (col IN (…))` constraint generated from the code system; other
  binding strengths are unconstrained columns.

### Choice elements

- **M3.8** A choice element `value[x]` becomes one column (or child table,
  for complex types) per allowed type — `value_boolean`, `value_quantity_…` —
  plus a generated `CHECK` that at most one alternative is populated.

### References

- **M3.9** A Reference element stores: `<name>_ref_type text`,
  `<name>_ref_id text` (parsed from relative literal references),
  `<name>_ref_url text` (absolute/other references, verbatim), plus columns
  for `display` and expanded `identifier`. Parsing MUST be reversible: the
  original `reference` string reconstructs exactly.
- **M3.10** Referential integrity across resources is NOT enforced by
  foreign keys (FHIR permits dangling references). `fhirpg` MAY offer an
  advisory integrity report; it MUST NOT reject writes for dangling refs.

### Extensions and primitive extensions

- **M3.11** Extensions are stored relationally as **typed leaf rows** in one
  generated table per resource type:
  `<resource>_ext(rid, path, ords, modifier, ext_ord, url, leaf, v_kind,
  v_text, v_num, v_bool)`, PK (rid, path, ords, modifier, ext_ord, leaf).
  `path`/`ords` locate the attach point (dotted JSON-name path, "" for the
  resource itself; ordinals at each repeating crossing). `ext_ord` is the
  1-based index in the extension array (`modifier` distinguishes
  modifierExtension); `url` is the top-level extension url, denormalized for
  querying. `leaf` addresses one scalar inside the extension's content as a
  dotted path whose all-digit segments are 0-based array indexes
  (`valueCodeableConcept.coding.0.code`); nested extensions are ordinary
  leaves (`extension.0.valueString`). `v_kind` ∈ s/n/b/z tags the JSON
  scalar kind; numbers keep their lexical form in `v_text` and a queryable
  `numeric` in `v_num`. This one uniform encoding covers every extension
  value type — including arbitrarily nested complex values — with no
  JSONB and no per-type tables.
- **M3.12** Primitive extensions (`_birthDate` etc.) reuse M3.11 with the
  primitive's path (and the entry index, for repeating primitives);
  element ids ride the same table as `ext_ord = 0, leaf = 'id'` rows.
  Reconstruction MUST re-emit the `_field` form exactly, including null
  padding in parallel arrays.
- **M3.13** `Resource.contained` resources are stored in a per-resource
  table `<resource>_contained(rid, ord, resource jsonb)`. Elements typed
  `Resource` (Bundle.entry.resource, Parameters.parameter.resource) become
  jsonb columns the same way. These are the sanctioned JSONB usages besides
  history (plan.md D7): such values are anonymous whole resources of
  unknowable type, so normalizing them buys nothing.
- **M3.14** The FHIR type graph contains one true datatype cycle:
  `Reference.identifier: Identifier` and `Identifier.assigner: Reference`.
  Static expansion cuts a cycle at the element that would re-enter an
  in-expansion type (`….identifier.assigner`), and stores anything below the
  cut as leaf rows (M3.11 encoding, minus extension columns) in a
  per-resource `<resource>_deep(rid, path, ords, leaf, v_kind, v_text,
  v_num, v_bool)` table — lossless, relational, and vanishingly rare in
  real data.

### Audit columns

- **M3.15** Every `<resource>_history` table carries, besides H5.1's
  columns, an **audit envelope**: `actor text` (the authenticated principal
  responsible for the change, or `'unauthenticated'`), `actor_source text`
  (how the principal was established, e.g. `header:X-Fhirpg-Principal`),
  `client text` (source address as the server observed it), `request_id
  text` (the value echoed in `X-Request-Id`), and `reason text` (a
  caller-supplied purpose of use, when given). These columns are written by
  the same statement that appends the history row, inside the same
  transaction as the data change — an audit record that can be lost
  independently of the change it describes is not an audit record.
- **M3.16** History is **tamper-evident**. Each history row carries
  `prev_hash bytea` and, for each hash algorithm of M3.16a, a digest column
  over the row's canonical serialization concatenated with the previous
  version's digest for the same algorithm and resource id (the first version
  chains from that algorithm's length in zero bytes). Chains are per resource
  id, so appends stay concurrent. `fhirpg verify-audit` MUST recompute every
  chain in every algorithm and report the first break in each.
- **M3.16a** The chain MUST be computed under **at least two hash algorithms
  of different design families**, and MUST include **SHA-256** (Merkle–Damgård,
  FIPS 180-4) and **SHA3-256** (sponge, FIPS 202).

  The point is family diversity, not digest length. MD5 and SHA-1 both fell to
  the same line of cryptanalysis, and both are Merkle–Damgård; two digests
  drawn from one family would buy far less than their bit counts suggest. A
  clinical record may be retained for decades — longer than anyone can
  confidently promise a single hash function will stand — so the chain should
  not rest on one construction.

  Both named algorithms are FIPS-approved, so a strict regime is satisfied by
  either. Where one must be named going forward, name **SHA-3**: NIST
  published FIPS 202 precisely so that an approved hash would exist that is
  not a SHA-2 variant.

  Verification MUST recompute every configured algorithm and report each
  separately rather than reducing them to a single verdict, so that a reader
  can rely on whichever algorithm their regime recognises.

  BLAKE3 (ARX tree) would add a third family and is deliberately **not**
  required: it is absent from pgcrypto and from OpenSSL, so it cannot be
  computed in the same statement as the insert, and computing it elsewhere
  would cost the atomicity that makes this chain trustworthy. It is also not
  FIPS-approved and MUST NOT be treated as the control of record where that
  matters. Should pgcrypto gain it, M3.16a should be revisited.

  Requiring SHA-3 makes **pgcrypto a required extension**. `fhirpg init` MUST
  create it, and MUST fail with a message naming the extension if it cannot.

- **M3.16b** Digests MUST be computed by the application, never by the
  database, and a deployment SHOULD additionally keep a **keyed tag**.

  The digests are unkeyed over a published pre-image, so anyone who can write
  to the database can also produce a correct digest for what they wrote.
  Computing them in SQL puts the means of forgery in the same place as the
  data, and forecloses the only real fix: a **MAC whose key the database never
  holds**. A key stored where the attacker already has write access protects
  nothing.

  What the unkeyed chain buys, stated honestly so nobody over-claims it: it
  detects **careless or unaware modification** — a migration, a stray
  `UPDATE`, a row restored from the wrong backup — and it supports an
  **external witness**, because a chain head recorded off-box makes truncation
  and wholesale rewriting detectable even against an attacker who can
  recompute digests. It does not, alone, stop an informed attacker with SQL
  write access.

  The keyed tag is `HMAC-SHA-256` (FIPS 198-1 over 180-4, so the FIPS story
  stays clean) over the same pre-image, stored as `<key-id>:<hex>`:

  - The key MUST NOT be written to the database, logged, or sent in a query,
    and MUST be at least 32 bytes: a placeholder like `changeme` reaching
    production would yield tags an attacker could reproduce by guessing.
  - A **file** SHOULD be the source, and MUST be supported. An environment
    variable is visible in `/proc/<pid>/environ`, survives into crash dumps,
    is reported by orchestrators, and is inherited by every child process; a
    file is none of those, and is what Kubernetes secrets and systemd
    credentials already produce. A key file readable by group or other MUST
    be **refused**, not warned about — a warning is read once at startup
    while the file stays readable for the life of the deployment.
  - Key material SHOULD be zeroed when dropped. Freed memory is not scrubbed,
    so a key otherwise lingers in the heap and is recoverable from a core
    dump.
  - A retired key that cannot be read MUST be an error, never a silent
    omission. Dropping one turns its rows *unverifiable*, and an operator who
    did not intend that should learn it at startup rather than from an audit.
  - Key configuration MUST apply to every command that reads history, not
    only the server. Verification without the key reports every keyed row as
    unverifiable, which is correct and useless.
  - The key id MUST travel with the tag. Without it, rotating a key would
    invalidate every historical row at once — indistinguishable from mass
    tampering, and the same trap as silently changing a hash format. Retired
    keys MUST stay loadable, so rotation is additive rather than a flag day.
  - Verification MUST be constant-time. A timing oracle would let an attacker
    with write access recover a valid tag byte by byte without ever holding
    the key.
  - **Only a tag mismatch is a finding.** A missing tag, a tag naming a key
    this process does not hold, and a malformed tag MUST each be reported as
    what they are and MUST NOT be reported as tampering. Reporting a
    key-distribution problem as a forgery would burn an incident response.

- **M3.16d** A key that can no longer be trusted MUST be retirable without
  losing the history it signed. `fhirpg chain-resign` counter-signs every
  history row under the current key.

  This is only for a suspected compromise. Ordinary rotation is additive
  (M3.16b): keep the old key loadable and nothing needs re-signing.

  - Re-signing MUST verify every chain first and MUST abort entirely on any
    finding. Re-signing rows that do not currently verify would give forged
    history the new key's authority, which turns the recovery procedure into
    the attack. It MUST be one transaction, so a partial re-signing cannot be
    left behind.
  - Counter-signatures MUST be **appended**, never written over the original
    tag. History is append-only, and re-signing in place would be the
    application doing what the append-only guard exists to prevent. The
    original tag is also evidence: replacing it destroys the record of what
    the retired key attested and leaves no way to tell a legitimate
    re-signing from a forged one.
  - A counter-signature MAY stand in for an original tag only where that tag
    **cannot be checked** — absent, or naming a key no longer held. A row
    whose own tag *mismatches* MUST remain a finding whatever later vouched
    for it.
  - Each counter-signature MUST record who ran it, when, and why.

- **M3.16e** fhirpg MUST be able to generate a signing key
  (`fhirpg chain-key-new`), creating the file readable only by its owner from
  the moment it exists.

  The shell equivalent, `openssl rand -hex 32 > key`, applies the process
  umask — commonly `022`, giving a file M3.16b requires be refused — and
  leaves the secret world-readable in the window before `chmod`. Generation
  MUST refuse to overwrite: silently replacing a signing key would orphan
  every row it had signed. The key MUST NOT be printed, since a secret echoed
  to a terminal lives on in scrollback and shell history.

  On an existing install, `init --upgrade` adds the new digest columns but
  MUST NOT backfill them. The rows are recoverable and the digests could be
  computed, but a chain assembled after the fact attests only that the rows
  look consistent *now* — which is exactly what an attacker who rewrote them
  would also produce. `verify-audit` MUST therefore report the new chain as
  beginning where its first digest appears, the same treatment rows predating
  the audit columns already receive. Manufacturing evidence is worse than
  admitting its absence.
- **M3.16c** fhirpg MUST be able to emit a **chain checkpoint**: a single
  value covering every chain head in the schema — resource type, id, latest
  version, and its digests — such that the value changes if any chain gains a
  version, loses one, or has its head altered. `fhirpg chain-witness` prints
  it, and it MUST be keyed when a key is configured, so that whoever holds
  only the data cannot recompute a matching value.

  This is what the per-row tag cannot do. A MAC proves a row was not
  rewritten; it says nothing about a row that is **gone**, and a chain missing
  its most recent version verifies perfectly, because nothing left behind
  refers to what was removed. Only a value recorded outside the database
  closes that gap.

  Checkpoints are also emitted as **INFO log lines on an `audit_checkpoint`
  target**, so a deployment already shipping logs has a witness for free. The
  dedicated target is what makes this practical: an operator can route and
  retain `audit_checkpoint` on its own schedule without keeping every other
  line, and the checkpoint carries no PHI — only counts and digests — so it
  may be retained far longer than ordinary application logs, and in places
  patient data must not go.

  A checkpoint MUST be emitted at startup and after an erasure (M3.18), and
  SHOULD be emitted on an interval a deployment configures. Erasure is
  singled out because it is the one sanctioned deletion: a checkpoint taken
  immediately after it separates a recorded, intentional removal from the
  unrecorded kind.

  The value is only a witness if it lands somewhere the database cannot
  reach. Logs shipped off-host qualify; logs written to a table in the same
  database, or to a disk the same compromised account can rewrite, do not.
  fhirpg cannot enforce this and MUST NOT imply it has: the guarantee is a
  property of the deployment's log path, and the documentation MUST say so.

- **M3.17** History is **append-only in the database, not merely by
  convention**. `fhirpg init` MUST emit a `BEFORE UPDATE OR DELETE` trigger
  on every history table that raises an exception, and the book MUST
  document the `REVOKE UPDATE, DELETE` grants a deployment applies to the
  application role. Escaping this is then a deliberate DBA act, never an
  application bug.
- **M3.18** Erasure (GDPR Art. 17) is the one sanctioned exception, and it
  is explicit: `fhirpg purge <Type> <id> --reason <text>` removes the
  resource's history rows and replaces them with a single tombstone row
  recording who purged what, when, why, and the `row_hash` chain it
  terminated — so an erased record leaves a verifiable hole rather than a
  silent one. Purge requires `--allow-erasure` and is logged at warn level.

---

Part of the [fhirpg specification](index.md).
