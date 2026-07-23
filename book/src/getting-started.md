# Getting started

## Requirements

- **PostgreSQL 18 or newer.** Required, not preferred: the stored procedures use
  `uuidv7()` for identifiers and `RETURNING OLD` for history archival, both of
  which arrived in 18.
- **Rust 1.88 or newer** to build from source.

## Install

```sh
cargo install fhirpg
```

Or with the optional [validation](#validation) support:

```sh
cargo install fhirpg --features validate
```

## A database to work against

Any PostgreSQL 18 will do. The repository ships a `compose.yaml` for a
throwaway one, run under [Podman](https://podman.io/):

```sh
podman compose up -d
```

That listens on port 5433, so it will not collide with a PostgreSQL you already
run. If you have no Compose provider installed:

```sh
podman run -d --name fhirpg-db -p 5433:5432 \
  -e POSTGRES_USER=fhirpg -e POSTGRES_PASSWORD=fhirpg -e POSTGRES_DB=fhirpg \
  -v fhirpg-pgdata:/var/lib/postgresql \
  docker.io/library/postgres:18
```

The volume mounts at `/var/lib/postgresql`, not `.../data`. PostgreSQL 18's
image keeps data in a version-specific subdirectory and refuses to start if it
finds a mount at the old path.

## Connecting

Every command takes the same connection flags, and honours the usual libpq
environment variables:

| Flag | Environment | Default |
| --- | --- | --- |
| `-n, --host` | `PGHOST` | `localhost` |
| `-p, --port` | `PGPORT` | `5432` |
| `-U, --username` | `PGUSER` | `postgres` |
| `-d, --db` | `PGDATABASE` | |
| `-W, --password` | `PGPASSWORD` | |
| `-s, --sslmode` | `PGSSLMODE` | `prefer` |

An explicit flag wins over the environment. All six libpq `sslmode` values work,
including `verify-ca`, which validates the certificate chain without checking
the hostname.

## Three commands and you have data

```sh
export PGHOST=localhost PGPORT=5433 PGUSER=fhirpg PGPASSWORD=fhirpg PGDATABASE=clinic

fhirpg init                       # create the schema
fhirpg load export/*.ndjson       # load resources
fhirpg web                        # browse with SQL
```

`init` creates 318 statements' worth of schema for FHIR R5: a table and a
history table for each of 158 resource types, the `transaction` sequence, and
the stored procedures.

## Validation

A build with the `validate` feature checks each resource against the typed FHIR
R5 model and reports what does not conform:

```sh
fhirpg load --validate export/*.ndjson
```

```
Patient: gender.code: code "platypus" is not in the required value set
Observation: does not match the FHIR R5 model: missing field `status`

3 resource(s) did not conform to the FHIR model, and were loaded anyway.
```

It reports; it does not reject. Storing data a strict model would refuse is the
point of the tool. Pass `--strict` if you would rather the first finding aborted
the run.
