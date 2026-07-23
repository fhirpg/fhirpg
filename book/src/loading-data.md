# Loading data

```sh
fhirpg load export/*.ndjson
fhirpg load a-directory/
fhirpg load bundle.json patient.json more.ndjson.gz
fhirpg load https://example.org/fhir/Patient/$export
```

## What it reads

Three formats, each optionally gzipped, mixed freely in one invocation:

- **NDJSON** — one resource per line. What Bulk Data servers produce.
- **FHIR Bundle** — resources come from `entry[].resource`.
- **A single resource** — one JSON object.

Detection is by **content, not filename**. Extensions can be wrong or absent;
compression is found by looking at the bytes. Directory arguments are walked
recursively, in sorted order.

Memory is bounded by the largest single resource, never by input size. A 1 GB
Bundle reads with about 2 MB of growth, because the `entry[]` array is streamed
rather than parsed whole.

## Two modes, and why the default depends on the source

| | `insert` | `copy` |
| --- | --- | --- |
| Mechanism | batched `INSERT … ON CONFLICT DO NOTHING` | `COPY … FROM STDIN` |
| Duplicate ids | keeps the first, ignores the rest | **fails** |
| Grouped input | fast | fastest |
| Non-grouped input | fast | **very slow** |
| Default for | local files | Bulk Data URLs |

"Grouped" means resources of the same type are adjacent. A `COPY` targets one
table, so every change of resource type ends the current one and starts another.
On a Bulk Data export, grouped by construction, that is a handful of long
`COPY`s. On a mixed bundle it is a `COPY` per resource.

Measured on the same 127,454 resources:

```
copy, grouped        1.2 s
insert, grouped      3.0 s
insert, non-grouped  3.4 s
copy, non-grouped   43.2 s
```

So: leave the default alone unless you know your input is grouped. If a `copy`
load fails on a duplicate id, the error says to use `--mode insert`, which keeps
the first occurrence.

## What happens to a resource that will not load

Nothing is dropped silently. Each is counted and reported at the end:

```
Done, inserted 127451 resources in 3.4 seconds (insert mode, txid 0):
  Observation            57619
  Patient                  600
  ...

Skipped 3 resource(s):
          2  unknown resource type
                     2  "NotAResource"
          1  transformation failed
```

- **Unknown resource type** — not a type this FHIR version defines. It is
  rejected before it can reach a SQL identifier.
- **Transformation failed** — the resource could not be rewritten.
- **Malformed** — not a JSON object, or unreadable.

Unreadable *files* are listed separately, with the reason. Pass `--strict` to
abort on the first problem instead.

## Other flags

| Flag | What it does |
| --- | --- |
| `--mode insert\|copy` | override the default |
| `--strict` | abort on the first unusable resource |
| `--count-first` | count up front for an exact progress total; costs a full extra read |
| `--memusage` | report resident set size as the load runs |
| `--txid new` | allocate a real transaction id instead of writing `0` |
| `--validate` | check against the typed FHIR model (needs the `validate` feature) |

`--txid new` is worth knowing about. By default every loaded row gets `txid = 0`,
which is what fhirbase does; it keeps loaded rows outside the history mechanism
the stored procedures use.

`--memusage` reports **resident set size**, not heap allocation — a different
quantity from the figure fhirbase prints under the same flag, because Rust has
no garbage collector to ask.
