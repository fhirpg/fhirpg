# The REST API

`fhirpg serve` mounts every installed version at `/{r3|r4|r5}` and speaks
`application/fhir+json`.

| Interaction | Route |
| --- | --- |
| capability statement | `GET /{v}/metadata` |
| create | `POST /{v}/{Type}` (server-assigned id; `If-None-Exist` honored) |
| read / update / delete | `GET/PUT/DELETE /{v}/{Type}/{id}` |
| conditional delete | `DELETE /{v}/{Type}?criteria` (single match) |
| vread / history | `GET /{v}/{Type}/{id}/_history[/{vid}]` |
| search | `GET /{v}/{Type}?…` and `POST /{v}/{Type}/_search` |
| batch / transaction | `POST /{v}` with a Bundle |

Semantics worth knowing:

- Every read and write carries `ETag: W/"{versionId}"`; `PUT`/`DELETE`
  honor `If-Match` and answer **412** on version conflicts.
- Reads distinguish **404** (never existed) from **410** (deleted);
  deleted history remains readable, and recreating an id continues its
  version sequence.
- **Transactions** are all-or-nothing database transactions with FHIR
  processing order (DELETE, POST, PUT) and `urn:uuid` reference
  resolution; batch entries are independent.
- Errors are OperationOutcomes with accurate status codes; internal
  errors never leak details (an opaque message plus server-side logs).
- Bodies are capped at 32 MiB; unimplemented result parameters answer
  501 rather than being silently ignored.

The capability statement is generated from what is actually compiled and
mounted — it lists exactly the search parameters that work.
