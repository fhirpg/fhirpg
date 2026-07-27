# 13. Compliance mapping

fhirpg is a component, not a certified system: it cannot make a deployment
compliant, but it must not be the reason a deployment cannot be. This table
maps the obligations that shape the requirements above, so a reviewer can
trace a regulation to a numbered requirement to a test.

| Obligation | Requirements | Evidence |
| --- | --- | --- |
| HIPAA §164.312(b) audit controls | M3.15, PR12.4, PR12.5 | T11.8 |
| HIPAA §164.312(c) integrity | M3.16, M3.16a, M3.16b, M3.16c, M3.17, R4.4, R4.5 | T11.6, T11.8 |
| HIPAA §164.312(e) transmission security | O10.5, O10.7, A7.8 | live TLS smoke test |
| HIPAA §164.502 minimum necessary | PR12.1, PR12.8 (perimeter) | boundary table |
| GDPR Art. 17 erasure | M3.18 | purge test |
| GDPR Art. 30 records of processing | PR12.5, PR12.7 | T11.8 |
| GDPR Art. 32 security of processing | O10.7, O10.8, O10.10 | CI gates |
| ONC/HTI FHIR conformance | A7.12, T11.4, §9 validation | Inferno run |
| ONC/HTI Bulk Data | (M8) `$export` | Inferno run |
| IEC 62304 §5–8 lifecycle | spec ↔ tasks ↔ test traceability | this document |
| IEC 62304 / FDA cybersecurity | O10.10 (SBOM, advisories) | release artifacts |

Two gaps are deliberate and stated rather than papered over:
authorization (scopes, compartments, consent, `meta.security` label
enforcement) lives at the perimeter (PR12.8), and terminology validation is
out of scope until a terminology service is integrated (§9).

---

Part of the [fhirpg specification](index.md).
