# Bulk Data API

The [SMART/HL7 Bulk Data](https://hl7.org/fhir/uv/bulkdata/) export protocol is
asynchronous: you ask a server to prepare an export, poll until it is ready,
then download the NDJSON files it lists.

```sh
# Download only.
fhirpg bulkget 'https://example.org/fhir/Patient/$export' ./export/

# Download and load in one step.
fhirpg load 'https://example.org/fhir/Patient/$export'
```

Quote the URL: `$export` is a shell variable otherwise.

## What happens

1. A `GET` with `Prefer: respond-async`, and an `Accept` header you can set.
2. The server replies `202` with a `Content-Location` — the status URL.
3. That URL is polled until it returns `200` with a manifest.
4. Every `output[].url` in the manifest is downloaded, several at a time.

Downloads are written **still compressed**: `Accept-Encoding: gzip` is set and
the response body is stored as-is. The loader detects gzip by content, so
nothing downstream needs to know.

## Options

| Flag | Default | |
| --- | --- | --- |
| `--numdl` | 5 | files downloaded at once |
| `--accept-header` | `application/fhir+json` | some servers want `application/ndjson` |

The `Accept` header exists because implementations disagree. Cerner expects
`application/ndjson`; SMART's reference server expects `application/fhir+json`.
If a server rejects the export request, that is the first thing to change.

## Failure is not partial

If any file fails to download, the whole export fails and nothing is loaded.
That is deliberate: fhirbase reports each failure and loads whatever else
arrived, which silently imports an incomplete export — and an incomplete
export looks exactly like a complete one once it is in the database.

An export that never becomes ready eventually gives up rather than polling
forever.

## Scratch files

Downloads land in a temporary directory that is removed when the command ends —
on success, on failure, and on an early exit alike. `bulkget` moves them into
the directory you name first, creating it if needed, and copying rather than
renaming when the two are on different filesystems.
