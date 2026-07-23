# FHIR versions

```sh
fhirpg --fhir 4.0.0 init
fhirpg load export/*.ndjson        # 5.0.0, the default
```

Ten versions are supported: **1.0.2, 1.1.0, 1.4.0, 1.6.0, 1.8.0, 3.0.1, 3.2.0,
3.3.0, 4.0.0, 5.0.0**.

The default is **5.0.0 (R5)**, the current HL7 release. fhirbase defaults to
3.3.0 — a pre-R4 draft ballot — and stops at 4.0.0 entirely.

The version decides two things: which tables `init` creates, and which
transformation rules `load` applies. Use the same one for both. A resource whose
type the selected version does not define is skipped and counted, not written to
a table that does not exist.

## Where the assets come from

Each version needs a schema (the DDL) and a transformation map (the rewrite
rules). For 1.0.2 through 4.0.0 these are fhirbase's own files, vendored
byte-for-byte and verified by checksum.

FHIR 5.0.0 is newer than fhirbase, so its assets are **generated** from the
official HL7 StructureDefinitions by a tool in this repository.

That generator is checked against fhirbase itself. Pointed at R3 and R4 it
reproduces fhirbase's own maps:

| Release | Compared against | Nodes | Identical |
| --- | --- | ---: | ---: |
| R3 | fhirbase 3.0.1 | 126 | 126 (100%) |
| R4 | fhirbase 4.0.0 | 155 | 151 (97%) |

The four R4 differences are a specification change, not a defect: FHIR's open
type list gained `Meta` between 4.0.0 and 4.0.1.

R5-specific constructs — above all `CodeableReference`, which R3 and R4 do not
have — are checked separately, by re-deriving the expected map from the
specification along a different path than the generator uses. 266 checks.

The full record is in `doc/r5-generation/validation.md`.

## Which R5 resources are storable

Every concrete resource in the specification gets a table: 158 of them. That
includes `Bundle`, `SearchParameter`, and `TestScript`, which fhirbase's own
schema omits.
