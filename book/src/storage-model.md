# The storage model

The model is fhirbase's, unchanged. It is the reason the tool exists.

## One table per resource type

Every FHIR resource type gets two tables, named by the lowercased type:

```sql
CREATE TABLE "patient" (
  id            text primary key,
  txid          bigint not null,
  ts            timestamptz DEFAULT current_timestamp,
  resource_type text default 'Patient',
  status        resource_status not null,
  resource      jsonb not null
);

CREATE TABLE "patient_history" (  -- same columns, PRIMARY KEY (id, txid)
  ...
);
```

Table names are always quoted. At least one FHIR resource type — `Group` —
lowercases to a PostgreSQL reserved word, so unquoted use is a syntax error.
That is exactly the bug that stops fhirbase loading a `Group` at all.

`resource_status` is an enum: `created`, `updated`, `deleted`, `recreated`.

## Resources are stored transformed

The `resource` column does not hold the FHIR JSON you handed in. It holds a
rewritten form, and the rewrite is worth understanding before you write queries.

**Choice elements are collapsed.** FHIR writes a `value[x]` element with the
type in the key; the storage form moves the type inside:

```jsonc
// in                          // stored
{"deceasedBoolean": true}  ->  {"deceased": {"boolean": true}}
{"valueQuantity": {...}}   ->  {"value": {"Quantity": {...}}}
```

**References are split.** A relative reference becomes an id and a type:

```jsonc
{"reference": "Practitioner/1", "display": "Dr Smith"}
  ->
{"resourceType": "Practitioner", "id": "1", "display": "Dr Smith"}
```

This is what makes joins possible without parsing strings in SQL:

```sql
SELECT o.id, p.resource->'name'->0->>'family'
  FROM observation o
  JOIN patient p ON p.id = o.resource->'subject'->>'id';
```

**The reference rewrite is lossy.** Only `reference` and `display` survive;
`identifier`, `type`, and any extensions on the `Reference` are discarded. That
is deliberate, and inherited: it is fhirbase's storage model and its own tests
assert it.

You can see exactly what a resource becomes without touching a database:

```sh
fhirpg transform patient.json
```

## History

Each resource table has a `_history` companion. The stored procedures archive
the previous version there before overwriting:

| Procedure | What it does |
| --- | --- |
| `fhirpg_create(resource[, txid])` | insert, archiving any existing row; `recreated` on conflict |
| `fhirpg_update(resource[, txid])` | update, archiving the previous version |
| `fhirpg_read(type, id)` | read one resource |
| `fhirpg_delete(type, id[, txid])` | delete, archiving with status `deleted` |
| `fhirpg_genid()` | a fresh UUIDv7 |

```sql
SELECT fhirpg_create('{"resourceType":"Patient","name":[{"family":"Smith"}]}'::jsonb);
SELECT fhirpg_read('Patient', 'the-id');
SELECT id, txid, status, ts FROM patient_history ORDER BY ts DESC;
```

Note the names: `fhirpg_*`, not `fhirbase_*`. A database initialized by fhirbase
has the other set, and neither tool recognizes the other's.

## Identifiers

Generated ids are **UUIDv7** — time-ordered, so insertion into the `id` primary
key is mostly-append rather than random. Measured over 500,000 rows that is 16%
faster to insert and a 24% smaller index than v4. The trade is that ids embed
their creation time.

Ids you supply are used as given.
