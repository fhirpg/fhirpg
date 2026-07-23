# Specifications — index

This directory holds the **living specifications** for `fhirpg`. It is
the source of truth for spec-driven development: behaviour is defined here
first, then implemented and verified. When code and spec disagree, reconcile
them — do not let them drift.

Operational guidance for agents (commands, conventions, how-to) lives in
[`../AGENTS.md`](../AGENTS.md) and `../AGENTS/`. The delivery plan lives in
[`../plan.md`](../plan.md) and [`../tasks.md`](../tasks.md). This directory
defines **what must be true**, not how to work or in what order.

## How to read these specs

- Requirement levels use **MUST / SHOULD / MAY** in the RFC 2119 sense.
- Each section ends with **acceptance criteria** — objective checks that decide
  whether the requirement is met. The green gate (`cargo build`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings`) enforces most mechanically.
- **fhirbase** means the Go program at `~/github/fhirbase/fhirbase`, which this
  project translates. Citations of the form `load.go:680–733` point into it and
  are normative references for behaviour being preserved.
- `Xn` identifiers are the catalogued fhirbase defects in
  [`../plan.md`](../plan.md#divergences-from-fhirbase). Where a section says a
  defect is fixed, that fix is a requirement, not an option.

## The specification set

Sections §1–§10 below are normative today. As each grows past the 40 KB house
limit it graduates into its own numbered file and this index keeps the summary
and the link.

| § | Scope | Status |
| --- | --- | --- |
| 1 | Identity, scope, non-goals | inline |
| 2 | Storage model — tables, history, procedures, server requirements, identifiers | inline |
| 3 | FHIR versions and assets | inline |
| 4 | **The transformation algorithm** | inline (graduates first) |
| 5 | Input formats and detection | inline |
| 6 | Connection configuration | inline |
| 7 | CLI contract | inline |
| 8 | Loading semantics | inline |
| 9 | Web console | inline |
| 10 | R5 asset generation | inline |

## Cross-cutting invariants

Non-negotiable, across every section.

1. **Green gate.** The crate MUST build, pass all unit tests and doctests, and
   produce zero `cargo clippy --all-targets` warnings with `clippy::pedantic`
   enabled.
2. **No panic on input.** No code path reachable from file content, network
   responses, database output, or command-line arguments may `panic!`,
   `unwrap()`, `expect()`, or index out of bounds. Malformed input yields a
   typed error naming the source. This is a hard requirement: fhirbase panics on
   a malformed transform asset (X4) and on an invalid `--sslmode` (`db.go:59`).
3. **No secrets in output.** The database password MUST NOT appear in logs,
   `--help`, `Debug`/`Display` output, error messages, or process arguments
   echoed back (X6).
4. **Transform fidelity.** For the nine vendored FHIR versions, the transform
   output MUST be `serde_json::Value`-equal to fhirbase's for the same input.
   Fidelity is defined on *values*, not bytes: object key order is not
   significant.
5. **Identifier safety.** Every SQL identifier derived from data MUST be
   validated against a known set and quoted before interpolation (X2).
6. **Streaming.** Memory use MUST be bounded by the largest single resource, not
   by input size. A 1 GB input MUST NOT produce a 1 GB allocation.
7. **Spec authority.** For R5, the official HL7 FHIR specification JSON — as
   surfaced by the sibling `fhir` crate's `fhir::r5::meta` — is upstream. These
   specs interpret it for this project.

---

## §1 Identity and scope

`fhirpg` is a command-line utility that imports FHIR data into a
PostgreSQL database and stores it relationally: one table per resource type,
resource bodies as `jsonb`, with history tables and stored procedures for CRUD.

**In scope:** the five commands of §7 — `init`, `transform`, `load`, `bulkget`,
`web`.

**Out of scope:** a FHIR REST server; FHIR search or FHIRPath evaluation;
database backends other than PostgreSQL, and PostgreSQL releases older than 18
(§2.3); re-implementing the FHIR data model (that is the sibling `fhir` crate);
binary self-update and usage telemetry (both present in fhirbase, both
deliberately dropped — decision D1); and migrating a database that fhirbase
already initialized, whose `fhirbase_*` procedures this tool does not create or
recognize (decision D3).

---

## §2 Storage model

The storage model is inherited from fhirbase unchanged. It is the reason the
tool exists and MUST NOT be redesigned during translation.

### 2.1 Tables

For each resource type in the selected FHIR version, the schema MUST define two
tables, named by the **lowercased** resource type:

```sql
CREATE TABLE IF NOT EXISTS "<resourcetype>" (
  id text primary key,
  txid bigint not null,
  ts timestamptz DEFAULT current_timestamp,
  resource_type text default '<ResourceType>',
  status resource_status not null,
  resource jsonb not null
);

CREATE TABLE IF NOT EXISTS "<resourcetype>_history" (
  id text,
  txid bigint not null,
  ts timestamptz DEFAULT current_timestamp,
  resource_type text default '<ResourceType>',
  status resource_status not null,
  resource jsonb not null,
  PRIMARY KEY (id, txid)
);
```

Table names MUST be double-quoted everywhere, in DDL and in DML. At least one
FHIR resource type — **`Group`** — lowercases to a PostgreSQL reserved word, so
unquoted use is a syntax error. fhirbase quotes in DDL but not in the insert
loader, which is why it cannot load a `Group` at all (X2).

`resource_type` and `id` are stored as columns *and* remain inside the `resource`
`jsonb`; `_fhirpg_to_resource` reassembles the canonical resource from
both.

### 2.2 Supporting objects

- `resource_status` — `ENUM ('created', 'updated', 'deleted', 'recreated')`,
  created inside a `DO $$ … $$` guard so `init` is idempotent on the type.
- `transaction (id serial primary key, ts timestamptz, resource jsonb)`. The
  `serial` implicitly creates `transaction_id_seq`, which the single-argument
  `fhirpg_create` and `fhirpg_update` procedures call via
  `nextval`. Removing the `serial` would silently break them.
- `concept` and `concept_history`, appended by the program rather than by the
  version asset (`dbinit.go:16–33`).
### 2.3 Server requirements

**PostgreSQL 18 or newer is required** (decision D8). `init` MUST check the
server version before executing any statement and refuse an older server with an
actionable message.

A consequence: `gen_random_uuid()` is a core function from PostgreSQL 13
onward, so the `pgcrypto` extension is no longer needed. The nine vendored
legacy assets nonetheless open with `CREATE EXTENSION IF NOT EXISTS pgcrypto`
and MUST NOT be edited, because they are vendored byte-identical (§3). That
statement fails on a PostgreSQL 18 installation without `contrib`. Therefore:

- `init` MUST treat a failure of the `pgcrypto` `CREATE EXTENSION` statement —
  **and only that statement** — as a warning rather than an error.
- The generated R5 schema (§10) MUST NOT emit it at all.

No other statement in any asset depends on the extension (decision D9).

### 2.4 Stored procedures

`assets/schema/functions.sql.json` holds 10 statements: the `_resource`
composite type and these procedures, renamed from `fhirbase_*` per decision D3.

| Procedure | Purpose |
| --- | --- |
| `fhirpg_genid()` | `uuidv7()::text` — see §2.5 |
| `_fhirpg_to_resource(_resource)` | Merge columns back into canonical resource JSON, populating `meta.lastUpdated` and `meta.versionId` |
| `fhirpg_create(jsonb, bigint)` / `(jsonb)` | Insert, archiving any existing row to `_history`; on conflict, status `recreated` |
| `fhirpg_update(jsonb, bigint)` / `(jsonb)` | Update with history archival |
| `fhirpg_read(text, text)` | Read by resource type and id |
| `fhirpg_delete(text, text, bigint)` / `(text, text)` | Delete with history archival |

The `_resource` composite type keeps its unbranded name.

The three procedures that archive a prior version — `fhirpg_create`,
`fhirpg_update`, `fhirpg_delete` — MUST obtain that prior version from
`RETURNING OLD` on the same statement that writes the new one, not from a
separate `SELECT … WHERE id = $2` in a sibling CTE as fhirbase does (decision
D13). Beyond removing an index lookup, this guarantees the row written to
`_history` is the genuine pre-image of the row that was replaced: a sibling CTE
reads the statement snapshot while `ON CONFLICT DO UPDATE` re-reads the live
row, so under `READ COMMITTED` with a concurrent writer the two can diverge.

That divergence is **demonstrated**, not assumed. Against the translated
procedures, `procedures_suite::d13_concurrency` reproduced it deterministically
on PostgreSQL 18.4: a committed version was lost from history entirely. The
same test now asserts the opposite and is the standing regression.

Every SQL identifier a procedure builds MUST go through `format`'s `%I`, never
`%s` (defect X15). Because `%I` quotes and therefore preserves case, while an
unquoted identifier folds to lower case, the table name MUST be lowered
explicitly before quoting; the `resourceType` **value** keeps its original case.

`fhirpg_delete` MUST record status `'deleted'` on the history row it writes
(defect X13), and MUST write **exactly one** history row. fhirbase writes two —
the pre-image at its own `txid` and a second at the supplied one — which
collides on `_history`'s `(id, txid)` primary key whenever the two are equal
(defect X14), and that is the common case, since every bulk-loaded row has
`txid = 0`. One row loses nothing: it carries the content that was live at
deletion, and every earlier version was already archived by the create or
update that superseded it.

### 2.5 Identifier generation

Every generated resource id MUST be a **UUIDv7** (decision D12). Ids are
generated at three sites and they MUST agree:

| Site | Mechanism |
| --- | --- |
| `fhirpg_genid()`, used by `fhirpg_create` when a resource has no `id` | `uuidv7()` |
| The insert loader, for a resource with no `id` | `uuidv7()::text` server-side |
| The copy loader, which must know the id before writing the row | `Uuid::now_v7()` client-side |

fhirbase uses `gen_random_uuid()` at the first two sites and a client-side v4 at
the third. UUIDv7 is time-ordered, so ids are near-sequential and insertion into
the `id text` primary key index is mostly-append rather than random — the point
of the change, on the bulk-load path the tool exists for. A mix of v4 and v7
would forfeit that, which is why all three sites move together.

Consequences that MUST be documented: generated ids embed their creation
timestamp, and they are no longer interchangeable with fhirbase's.

**Acceptance:** `init` succeeds for every supported version against an empty
PostgreSQL 18 database, including one without `contrib` installed; a server
older than 18 is refused with an actionable message; `transaction_id_seq`
exists; `SELECT fhirpg_create('{"resourceType":"Patient"}'::jsonb)` returns a
resource carrying `id`, `meta.versionId`, and `meta.lastUpdated`; no asset
contains the string `fhirbase`.

---

## §3 FHIR versions and assets

Supported versions: **1.0.2, 1.1.0, 1.4.0, 1.6.0, 1.8.0, 3.0.1, 3.2.0, 3.3.0,
4.0.0** (vendored byte-identical from fhirbase) and **5.0.0** (generated per
§10). The default for `--fhir` is **5.0.0**, a deliberate divergence from
fhirbase's 3.3.0 (decision D4).

> **Interim state, to be removed at task T23.** The 5.0.0 assets are generated
> by tasks T21 and T22 and do not exist yet, so 5.0.0 is not yet a selectable
> version and the shipped default is fhirbase's **3.3.0**. Defaulting to a
> version with no assets would make every command fail. T23 adds 5.0.0, flips
> the default, and deletes this note.

Each version has two assets:

- `assets/schema/fhirpg-<version>.sql.json` — a JSON array of DDL
  statement strings, executed in order. 4.0.0 has 293 statements covering 145
  resource tables.
- `assets/transform/fhirpg-import-<version>.json` — the transformation
  map of §4. 4.0.0 has 155 top-level entries, 929 `union` directives, and 737
  `reference` directives.

Requirements:

- The nine vendored assets MUST remain byte-identical to fhirbase's, verified by
  checksum. Only filenames change.
- Transform maps MUST be parsed and cached at most once per version per process
  (fhirbase memoizes identically, `transform.go:132–158`).
- Loading a transform map MUST validate it: every `tr/move` target MUST resolve
  to an existing top-level entry, and every `tr/act` value MUST be recognized.
  Both hold for all nine vendored assets. A violation is a startup error.
- An unknown `--fhir` value MUST produce an error listing the known versions.

---

## §4 The transformation algorithm

The core of the port. It rewrites a FHIR resource into fhirbase's storage
representation, and its output is what lands in the `resource` `jsonb` column.
Source: `transform.go:16–195`.

### 4.1 The transformation map

A JSON object keyed by type name (`Patient`, `Reference`, `Identifier`, …).
Each value is a tree mirroring the resource's shape. A node's keys are either
**directives** (prefixed `tr/`) or **field names** whose values are child nodes.

| Directive | Meaning |
| --- | --- |
| `tr/act` | The action at this node: `union` or `reference` |
| `tr/arg` | Arguments for the action: `{key, type}` for `union` |
| `tr/move` | Continue transformation using the node at this path in the map root |
| `tr/isCollection` | Present in the assets and **never read** — see 4.7 |

### 4.2 Entry point

Given a resource and a version:

1. Read `resourceType`. If absent or not a string, this MUST be an error.
2. Look up the map entry for that type. **If absent, return the resource
   unchanged** — unknown resource types pass through untouched, which the
   fhirbase test suite asserts explicitly.
3. Otherwise transform the resource against that node.

### 4.3 The recursion

```text
transform(node, tr_node, map):
    if tr_node has tr/act AND node is not an array:
        apply the action (4.4, 4.5) and return

    match node:
        object → for each (k, v):
                     if tr_node has child k:
                         child     := tr_node[k]
                         out_key   := child.tr/arg.key  if present, else k
                         if child has tr/move:
                             child := resolve(map, child.tr/move)
                         result[out_key] = transform(v, child, map)
                     else:
                         result[k] = transform(v, none, map)
                 → result
        array  → [ transform(e, tr_node, map) for e in node ]   # same tr_node
        other  → node unchanged
```

The `tr/act` guard MUST test that the node is not an array. An array whose
transform node carries `tr/act` falls through to the array branch, and each
*element* receives the action — this is how repeating choice and reference
fields work.

`resolve(map, path)` walks the path from the map root. fhirbase's version
performs an unchecked type assertion and panics on a missing segment (X4); this
implementation MUST return an error instead. Because §3 validates every
`tr/move` at load time, that error is unreachable for valid assets.

### 4.4 `union` — collapsing polymorphic elements

FHIR represents a choice element `value[x]` as a type-suffixed key —
`valueString`, `valueQuantity`, `deceasedBoolean`. fhirbase collapses these into
a single key holding a one-entry object tagged by type.

Given `tr/arg = {key, type}` at a node reached from field `k`:

1. The output key is `key`, not `k`. (`deceasedBoolean` → `deceased`.)
2. Compute the inner value:
   - If `type` is **`Reference`**, apply the reference action (4.5) to the node.
   - Else if the map has a top-level entry named `type`, transform the node
     against it.
   - Else use the node unchanged.
3. The result is `{ type: inner }`.

```json
{"deceasedBoolean": true}          →  {"deceased": {"boolean": true}}
{"multipleBirthInteger": 2}        →  {"multipleBirth": {"integer": 2}}
{"valueReference": {"reference": "Immunization/123"}}
                                   →  {"value": {"Reference": {"resourceType": "Immunization", "id": "123"}}}
```

### 4.5 `reference` — splitting relative references

A FHIR `Reference` is rewritten into an id/type pair:

1. Start with an empty object.
2. If `reference` is present, split its string value on `/`:
   - exactly two components → `{resourceType: <first>, id: <second>}`;
   - otherwise → `{id: <whole string>}`.
3. If `display` is present, copy it.
4. Emit that object.

**All other fields of the `Reference` are discarded** — `identifier`, `type`,
`extension`, `reference`'s original form. This is lossy, it is intentional, it
is asserted by fhirbase's tests, and it MUST be preserved. It is documented here
because it is the single most surprising behaviour in the algorithm.

```json
{"reference": "Practitioner/1", "display": "John"}
                                   →  {"resourceType": "Practitioner", "id": "1", "display": "John"}
{"reference": "urn:uuid:abc"}      →  {"id": "urn:uuid:abc"}
{"display": "ACME corp"}           →  {"display": "ACME corp"}
```

Note the third case: a `Reference` with no `reference` field yields an object
with only `display`.

### 4.6 Determinism

Two `union` directives can target the same output key — `deceasedBoolean` and
`deceasedDateTime` both write `deceased`. FHIR forbids both being present, but
input is untrusted. fhirbase's result depends on Go's randomized map iteration
order and is therefore nondeterministic.

This implementation MUST be deterministic: process object keys in a defined
order and let the **last** key in that order win, so identical input always
yields identical output. The rule MUST be documented and tested.

### 4.7 `tr/isCollection`

Present throughout the assets; never read by fhirbase, because 4.3's array
branch already recurses with the same transform node, which handles repeating
fields correctly for both actions. This implementation MUST also ignore it.
Retained in the assets for byte-identical vendoring (§3) and emitted by the R5
generator (§10) for consistency.

### 4.8 Acceptance criteria

- The five `transform_test.go` cases pass, ported verbatim: `CarePlan`
  references and nested `Identifier.assigner`; `Claim.information[].valueReference`
  (union-of-Reference); `Patient` with `deceasedBoolean`, `multipleBirthInteger`,
  and `managingOrganization`; a `Reference` carrying only `display`; and an
  unknown `resourceType` passing through unchanged.
- Output is `serde_json::Value`-equal to fhirbase's across a corpus of ≥20
  resources spanning ≥5 resource types, at 3.0.1 and 4.0.0.
- Property tests: unknown `resourceType` is the identity; no input panics;
  output is always valid JSON.
- The both-variants-present case is deterministic across 1,000 runs.

---

## §5 Input formats and detection

`load` accepts three formats, each optionally gzip-compressed, mixed freely in
one invocation. Detection is by **content, not filename** (`load.go:36–194`).

### 5.1 Compression

Attempt to read the file as gzip. On failure, rewind to offset 0 and read it as
plaintext. No filename heuristic.

### 5.2 Format

1. Read the first two lines.
2. If **both** are complete JSON objects — brace-balanced, counting only braces
   outside string literals and honouring backslash escapes — the file is
   **NDJSON**.
3. Otherwise parse from the start and inspect `resourceType`:
   - `"Bundle"` → **FHIR Bundle**; resources are read from `entry[].resource`.
   - any other non-empty string → **single resource**.
   - absent → treated as a FHIR Bundle.
4. If the file has only one line, apply step 3 to it.

### 5.3 Reading

- **NDJSON:** one resource per line. A line whose root is not a JSON object MUST
  be reported with filename and line number, and the rest of the file skipped —
  fhirbase's behaviour, preserved.
- **FHIR Bundle:** stream `entry[]`, yielding each `entry.resource`. A
  non-object entry, or an entry without `resource`, is reported and the rest of
  the file skipped. The array MUST be streamed, never buffered whole
  (invariant 6).
- **Single resource:** the whole document is one resource.
- **Multiple inputs:** files are read in argument order; directory arguments are
  walked recursively. A file that cannot be opened or whose format cannot be
  determined MUST be reported and skipped, not fatal.

### 5.4 Counts

Resource counts drive progress display only. They MUST NOT influence batching,
flushing, or termination (X7). Exact counts require a counting pass, which for
compressed input means inflating twice; therefore progress is indeterminate by
default and `--count-first` opts into exact totals.

**Acceptance:** the five `load_test.go` detection cases pass verbatim, plus
gzip, empty file, single-line file, BOM, and CRLF cases; peak RSS stays flat
while reading a 1 GB bundle and a 1 GB NDJSON file.

---

## §6 Connection configuration

Precedence, highest first: explicit command-line flag → environment variable →
built-in default.

| Flag | Env | Default |
| --- | --- | --- |
| `-n, --host` | `PGHOST` | `localhost` |
| `-p, --port` | `PGPORT` | `5432` |
| `-U, --username` | `PGUSER` | `postgres` |
| `-d, --db` | `PGDATABASE` | *(empty)* |
| `-W, --password` | `PGPASSWORD` | *(empty)* |
| `-s, --sslmode` | `PGSSLMODE` | `prefer` |

`--sslmode` MUST accept exactly libpq's six values and behave accordingly:

| Value | Behaviour |
| --- | --- |
| `disable` | Plaintext only |
| `allow` | Plaintext first, TLS fallback |
| `prefer` | TLS first without certificate verification, plaintext fallback |
| `require` | TLS required, certificate not verified |
| `verify-ca` | TLS required, certificate chain verified, **hostname not checked** |
| `verify-full` | TLS required, chain and hostname verified |

`verify-ca` MUST NOT verify the hostname. That is what distinguishes it from
`verify-full`, and fhirbase collapses the two into one branch that verifies both
(defect X12), refusing connections libpq would accept.

`allow` MUST try plaintext first and fall back to TLS; `prefer` MUST try TLS
first and fall back to plaintext. The two differ only in order, and
`tokio-postgres` implements `prefer` natively but has no `allow`, so `allow` is
implemented as two sequential connection attempts.

An unrecognized value MUST be a typed error. fhirbase calls `panic!`
(`db.go:59`); invariant 2 forbids that.

Before executing any statement, a command that connects MUST verify the server
is PostgreSQL 18 or newer (§2.3) and refuse an older one with an actionable
message.

The connection banner MUST report the **actual** `sslmode` and MUST redact the
password. fhirbase hardcodes `sslmode=disable` into the banner regardless of the
real setting and prints the password in cleartext, in two places (X6).

**Acceptance:** a table-driven test covers all six modes and flag-vs-env
precedence; a test asserts the password never appears in `Debug`, `Display`, or
log output.

---

## §7 CLI contract

Binary: `fhirpg`. Global flags per §6, plus `-f, --fhir`. Subcommands:

| Command | Arguments | Purpose |
| --- | --- | --- |
| `init` | — | Create the schema, procedures, and concept tables |
| `transform` | `FILE` | Transform one resource, print to stdout |
| `load` | `URL` \| `PATH…` | Load resources into the database |
| `bulkget` | `URL DIR` | Download Bulk Data NDJSON to a directory |
| `web` | — | Serve the SQL console |

Command-specific flags: `load` takes `-m/--mode` (`insert` \| `copy`), `--numdl`
(default 5), `--accept-header` (default `application/fhir+json`), `--strict`
(§8.2), `--count-first` (§5.4), `--txid=new` (§8.2), and `--memusage`;
`bulkget` takes `--numdl` and `--accept-header`; `web` takes `--webport`
(default 3000) and `--webhost` (default `127.0.0.1` — see §9).

`--memusage` reports the process's **resident set size**, current and peak,
sampled every 3,000 resources. fhirbase's flag of the same name prints Go
garbage-collector statistics (`Alloc`, `TotalAlloc`, `Sys`, `NumGC`), which have
no Rust equivalent. Because RSS is a different quantity — resident pages
including allocator slack, not live heap — the output MUST state what it is
measuring, so it is not read as fhirbase's `Alloc` (decision D14).

Requirements:

- Flag names, short forms, and defaults MUST match fhirbase except where a
  decision says otherwise (`--fhir` default per D4, `--webhost` default per §9,
  `--nostats` removed per D1).
- Invoking with no subcommand prints help and exits **0**.
- A command error prints the error and exits **1**.
- Missing required arguments print that command's help and exit non-zero.
- `--help` retains the ASCII-art banner (rebranded) and the long per-command
  descriptions, which are genuinely useful documentation.

---

## §8 Loading semantics

### 8.1 Modes

- **`insert`** — batched, pipelined `INSERT … ON CONFLICT (id) DO NOTHING`.
  Order-insensitive; tolerates duplicate ids by keeping the first occurrence;
  performs identically on grouped and non-grouped input. The default for local
  files.
- **`copy`** — `COPY … FROM STDIN`. A single `COPY` covers a maximal run of
  consecutive same-typed resources; a new one begins when the type changes.
  Roughly 3× faster on **grouped** input (all resources of a type adjacent, as
  produced by Bulk Data servers) and slower on non-grouped input. The default
  when the argument is a Bulk Data URL.

Both modes MUST produce identical rows for identical input.

### 8.2 Per-resource pipeline

1. Read the next resource (§5).
2. Determine `resourceType`. It MUST be validated against the selected version's
   known resource set; an unknown type is reported and the resource skipped.
   Only then is the lowercased name quoted and used as a table name (X2).
3. Transform it (§4). A transform failure MUST be explicit and MUST NOT write a
   row. By default the resource is **skipped and counted**, and the tally is
   reported at the end of the run (§8.4); under `--strict` the run aborts with a
   non-zero exit instead (decision D10). fhirbase prints the error and then
   inserts the possibly-null result anyway (X3).
4. Determine the id: the resource's `id` if a non-empty string, else a generated
   **UUIDv7** (§2.5).
5. Write with `txid = 0` and `status = 'created'`, matching fhirbase.
   `--txid=new` allocates one real `transaction_id_seq` value for the run
   instead (X10).

### 8.3 Batching

Flush when the batch buffer is full, and once at end of stream. Flushing MUST
NOT depend on a resource count (X7). Batch size defaults to 2,000, fhirbase's
value.

### 8.4 Reporting

On completion, print total resources, elapsed seconds, and a right-aligned
table of counts per resource type. Skipped resources — unopenable files,
undetectable formats, unknown types, transform failures — MUST be surfaced in
that summary rather than only as scrollback.

**Acceptance:** a `Group` resource loads successfully in both modes (fhirbase
cannot); `resourceType` values containing SQL metacharacters are rejected rather
than executed; loading `demo/bundle.ndjson.gzip` yields per-type counts matching
`SELECT count(*)`; duplicate ids keep the first occurrence.

---

## §9 Web console

`web` serves the vendored static console plus two endpoints:

- `GET /q?query=<sql>` — executes the SQL and streams
  `{"columns": […], "rows": [[…]]}`. A missing `query` is 400; a SQL error is a
  non-200 with `{"message": …}`; neither may panic.
- `GET /health` — 200 when a connection can be acquired and a trivial query
  runs.

Requests are logged (method, URL, remote address, user agent) and the server
shuts down gracefully on SIGINT.

**Security.** `/q` executes arbitrary SQL with no authentication. That is the
feature. fhirbase compounds it by defaulting `--webhost` to the empty string,
binding all interfaces. This implementation MUST default to `127.0.0.1`, MUST
require an explicit `--webhost` to expose the port, and MUST print a prominent
warning when a non-loopback address is bound. The risk MUST be documented in
README and in `web --help` (X11).

---

## §10 R5 asset generation

fhirbase's newest assets are 4.0.0; R5 has none, and the generator that produced
the originals was never published. The two 5.0.0 assets are therefore generated
from the sibling `fhir` crate's `fhir::r5::meta` element table — 9,333 entries
derived from the official HL7 specification JSON — then hand-verified and
vendored (decision D5).

### 10.1 Schema asset

Emit, in order: the `resource_status` enum inside its `DO $$ … $$` guard; the
`transaction` table; then for each R5 resource type the pair of tables from
§2.1. Column shapes, defaults, and quoting MUST match the 4.0.0 asset exactly.

The generated asset MUST NOT emit `CREATE EXTENSION … pgcrypto`. PostgreSQL 18
is required (§2.3) and provides `gen_random_uuid()` in core, so the extension is
dead weight that fails on installations without `contrib` (decision D9). This is
the one intentional structural difference from the vendored legacy schemas.

### 10.2 Transform asset

For each element path in the metadata:

- Path ending in `[x]` → one `union` entry per declared type code, keyed
  `<base><TypeCode>` with `tr/arg = {key: <base>, type: <TypeCode>}`. The
  suffix uses FHIR's capitalization rule, so `deceased[x]` with type `boolean`
  yields key `deceasedBoolean` and `type` `boolean`.
- Type code `Reference` → `tr/act: "reference"`.
- `max` other than `"1"` → `tr/isCollection: true` (§4.7: emitted, never read).
- A complex datatype reference → `tr/move: ["<TypeName>"]`, with that type
  present as a top-level entry.

### 10.3 Verification

Because the assets are generated once rather than reproduced from an oracle,
correctness rests on four checks, all of which are requirements:

1. **Structural.** Both files parse; every `tr/move` target resolves; every
   table name is unique, lowercase, and quoted; the resource-type count is 158.
2. **Plausibility.** `union` and `reference` totals fall within a defensible
   band of 4.0.0's 929 and 737.
3. **Differential.** For resource types present in both R4 and R5, diff the
   generated R5 transform against the vendored 4.0.0 one. **Every** difference
   MUST be explainable by a documented R4→R5 specification change; the analysis
   is recorded in this directory.
4. **Manual.** At least ten diverse resource types — `Patient`, `Observation`,
   `Bundle`, `Group`, `MedicationRequest`, `Encounter`, `Questionnaire`,
   `Subscription`, `Evidence`, `ImplementationGuide` — are checked element by
   element against the published R5 specification, with the record kept here.

**Acceptance:** all four checks pass and are recorded; `init --fhir 5.0.0`
succeeds; the `fhirpg_*` procedures round-trip an R5 `Patient`.

---

## Status

**Specification proposed, implementation not started.** The target repository
currently contains only a hello-world `main.rs`. §1–§10 define the behaviour to
be built; [`../tasks.md`](../tasks.md) sequences the work; each task names the
sections it satisfies.

Divergences from fhirbase are deliberate and enumerated in
[`../plan.md`](../plan.md): fifteen defect fixes (X1–X15) and fourteen decisions
(D1–D14). Nothing else in the observable behaviour may differ without a spec
change landing first.
