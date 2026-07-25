# The trust boundary

fhirpg is a component, not a system. It cannot make a deployment compliant,
and it must not be the reason a deployment cannot be. This chapter states, in
one place, what fhirpg guarantees and what the deployment around it has to
provide — because a boundary nobody can point at is not a boundary (spec
PR12.8).

## What fhirpg guarantees

| Property | How | Spec |
| --- | --- | --- |
| Writes are transactional and versioned | one transaction per write, monotonic `version_id`, append-only history | R4.4, H5.1 |
| Reads see one consistent snapshot | every multi-statement read runs `REPEATABLE READ READ ONLY` | R4.5 |
| Conditional interactions are atomic | criteria-hash advisory lock, match and write in one transaction | A7.10 |
| Optimistic concurrency | `ETag` / `If-Match`, 412 on mismatch, honored inside bundles too | D11, A7.9 |
| Every change records who made it | audit envelope written by the same statement as the change | M3.15 |
| Every read is recorded | disclosure row in `fhirpg_access_log` | PR12.5 |
| History cannot be quietly rewritten | per-resource SHA-256 chain plus a database trigger refusing UPDATE/DELETE | M3.16, M3.17 |
| PHI is encrypted in transit to the database | rustls, `sslmode` honored, refusal to serve non-loopback over a plaintext link | O10.7 |
| Responses are not cached | `Cache-Control: no-store` and friends on every response | A7.8 |
| Emitted URLs are not attacker-controlled | configured `--base-url`; forwarded headers only under `--trust-proxy` | A7.7 |
| Nothing is silently dropped | unknown elements rejected; `_include` truncation warns in the bundle | D12, P6.7 |
| Diagnostics do not leak stored data | client-safe errors are a separate type from internal ones | A7.11 |

## What the deployment must provide

fhirpg does **not** do these, and a deployment that skips them is not safe to
put patient data in:

| Obligation | Why it is not here |
| --- | --- |
| **Authentication** | Identity belongs to the perimeter (plan D13). fhirpg accepts a principal asserted by a trusted proxy and records it; it verifies nothing. |
| **Authorization** | There is no scope check, no compartment restriction, no `meta.security` label enforcement, and no consent evaluation. Any caller who reaches the API can read and write anything in it. |
| **TLS to clients** | Terminate at the perimeter, or use the in-process `tls` feature. |
| **Rate limiting per identity** | fhirpg bounds concurrency and request cost, not per-user quotas. |
| **Network isolation** | The API, `/metrics`, and the database link should not share a network with untrusted clients. |
| **Backup and retention** | Plain PostgreSQL (`pg_dump`, PITR). fhirpg guarantees a consistent snapshot is a valid store; it does not schedule anything. |
| **Key management and at-rest encryption** | Filesystem, volume, or cloud-provider encryption. fhirpg stores no secrets and manages no keys. |

## What neither provides yet

Stated rather than implied, so nobody discovers it during an audit:

- **Terminology validation.** Required-binding `CHECK` constraints only. No
  value-set expansion, no SNOMED/LOINC/ICD membership checks.
- **Profile conformance.** Base-specification structure only — not US Core,
  IPS, or any implementation guide.
- **FHIRPath invariants.** The `fhir` crate enforces three of 314.
- **Referential integrity across resources.** FHIR permits dangling
  references and so does fhirpg (M3.10).

## Configuring the boundary

A deployment that means it:

```sh
export PGSSLMODE=require PGSSLROOTCERT=/etc/ssl/pg-ca.pem
fhirpg serve \
  --bind 0.0.0.0:8080 \
  --base-url https://fhir.example.org \
  --trust-proxy --allowed-host fhir.example.org \
  --principal-header X-Fhirpg-Principal \
  --reason-header X-Fhirpg-Purpose \
  --require-principal
```

Each flag is load-bearing:

- Without `--base-url`, paging links are built from the address fhirpg bound,
  which behind a proxy is wrong. With it, no request header can change them.
- `--trust-proxy` is what makes `--principal-header` mean anything. Without
  it the header is *ignored*, not honored — otherwise any client could name
  itself anyone. Only set it when a proxy you control is the only route in.
- `--require-principal` turns an unattributable request into a 401. Without
  it, writes are recorded as `unauthenticated`, which is honest but not
  useful.

## Choosing an audit mode

`--audit-mode` decides how a disclosure record reaches the log, and the
choice is a real trade rather than a tuning knob:

| Mode | What it costs | What it risks |
| --- | --- | --- |
| `sync` (default) | A round trip on every read. | Nothing: the record commits before the response is sent. |
| `async` | An in-memory enqueue. | Records still queued when the process is killed are lost. Graceful shutdown drains them; `SIGKILL` does not. |
| `off` | Nothing. | Everything this section is about. Requires `--allow-unaudited`. |

`sync` is the default because the failure it prevents cannot be repaired
afterwards. A disclosure with no record is indistinguishable, later, from a
disclosure that never happened, and no amount of investigation recovers the
difference.

In **every** mode, a disclosure that cannot be recorded is refused rather
than served: a saturated queue answers 503. Four counters per version make
the difference visible on `/metrics`:

- `refused` — reads turned away to keep the log honest. Non-zero means the
  system is working as designed under strain.
- `lost` — records the writer could not commit *after* the data was served.
  **Non-zero is an incident**: disclosures happened that the log does not
  show.

Alert on `lost` above zero. Alert on sustained `refused` as a capacity
signal.

## Verifying the audit trail

The hash chain is checked on demand, not on every read:

```sh
fhirpg verify-audit --fhir-version r5
```

It recomputes every resource's chain and exits nonzero on the first break,
naming the resource and version. Rows written before the audit columns
existed carry no hash; they are reported as the point a chain begins, not as
tampering.

Two things to know about what this proves. It detects modification of history
by anything that bypassed the application — including a DBA with table access,
which is the threat it exists for. It does **not** protect against an attacker
who can also recompute and rewrite the chain; for that, ship `row_hash` values
off-box (log shipping, WORM storage, or a notary) so the local copy is not the
only witness.

## Erasure versus append-only history

GDPR Article 17 says a record must be removable. Everything above says history
must not be. These genuinely conflict, and fhirpg resolves it in one direction,
explicitly:

```sh
fhirpg purge Patient 1234 --reason "art-17 request #4471" --allow-erasure
```

The resource and every historical version are deleted, and a **tombstone**
takes their place recording who erased it, when, why, and the `row_hash` the
chain ended on. So an erased record leaves a *verifiable hole* — an auditor
can still see that a chain existed and was deliberately terminated by a named
person — rather than a gap indistinguishable from a resource that never
existed.

`verify-audit` treats a tombstone as a recorded erasure, not a break. That
distinction matters more than it looks: a tamper-evidence report that cries
wolf on every lawful erasure is one an operator learns to ignore, at which
point it detects nothing.

Two limits to state before anyone relies on this:

- **The database is not the estate.** Backups, replicas, WAL archives, and any
  downstream system that consumed the resource still hold it until they age
  out. Promising erasure means having a plan for all of them; `purge` is one
  step in that plan, not the plan.
- **The guard is against accident, not against the application.** The
  append-only trigger permits `DELETE` inside a transaction that sets
  `fhirpg.erasure`, which is how `purge` works — so application-level SQL
  execution could do the same. The trigger stops ordinary code, migrations,
  and stray statements from touching history at all; the tombstone and the
  access log are what make a deliberate erasure accountable.

## Grants

The append-only trigger is enforcement the application cannot bypass. Belt and
braces, restrict the application role too:

```sql
REVOKE UPDATE, DELETE ON ALL TABLES IN SCHEMA r5 FROM fhirpg_app;
GRANT SELECT, INSERT ON r5.patient_history TO fhirpg_app;  -- and each _history
```

With both in place, rewriting history requires a superuser deliberately
disabling a trigger — an act that is itself visible in the server log.
