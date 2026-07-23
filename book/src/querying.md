# Querying

Everything here is ordinary SQL against ordinary tables. These are starting
points, not an API.

## Counting

```sql
SELECT count(*) FROM patient;

-- Every table with rows, largest first.
SELECT relname AS resource_table, n_live_tup AS approx_rows
  FROM pg_stat_user_tables
 WHERE n_live_tup > 0
 ORDER BY n_live_tup DESC;
```

## Reading fields out of the resource

```sql
SELECT resource->>'gender' AS gender, count(*)
  FROM patient GROUP BY 1 ORDER BY 2 DESC;

SELECT left(resource->>'birthDate', 4) AS birth_year, count(*)
  FROM patient GROUP BY 1 ORDER BY 1;

SELECT resource->'name'->0->>'family' AS family,
       resource->'name'->0->'given'->>0 AS given
  FROM patient LIMIT 10;
```

## Joining on references

This is where the [reference split](storage-model.md#resources-are-stored-transformed)
pays off — the id is a plain field, so no string parsing:

```sql
SELECT p.resource->'name'->0->>'family' AS family,
       count(o.id) AS observations
  FROM patient p
  LEFT JOIN observation o ON o.resource->'subject'->>'id' = p.id
 GROUP BY 1 ORDER BY 2 DESC LIMIT 20;
```

## Choice elements

Remember they are collapsed, with the type as the key:

```sql
-- deceasedBoolean became deceased.boolean
SELECT count(*) FROM patient WHERE resource->'deceased'->>'boolean' = 'true';

-- valueQuantity became value.Quantity
SELECT resource->'code'->>'text' AS code,
       (resource->'value'->'Quantity'->>'value')::numeric AS value
  FROM observation
 WHERE resource->'value' ? 'Quantity'
 LIMIT 20;
```

## Indexing

The tables ship with only a primary key. `jsonb` supports expression indexes,
so index what you actually query:

```sql
CREATE INDEX patient_gender ON patient ((resource->>'gender'));
CREATE INDEX observation_subject ON observation ((resource->'subject'->>'id'));

-- Or a GIN index for containment queries across the whole resource.
CREATE INDEX patient_resource_gin ON patient USING gin (resource jsonb_path_ops);
```

## Using the procedures

```sql
SELECT fhirpg_create('{"resourceType":"Patient","name":[{"family":"Smith"}]}'::jsonb);
SELECT fhirpg_read('Patient', 'the-id');
SELECT fhirpg_update('{"resourceType":"Patient","id":"the-id","active":false}'::jsonb);
SELECT fhirpg_delete('Patient', 'the-id');

SELECT id, txid, status, ts FROM patient_history WHERE id = 'the-id' ORDER BY txid;
```
