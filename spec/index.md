# fhirpg specification

This is the normative specification for fhirpg. Requirements are numbered and
use RFC 2119 keywords (MUST, SHOULD, MAY).

Behaviour is defined here first, then implemented and verified. When code and
spec disagree, reconcile them — do not let them drift. Operational guidance
for contributors lives in `AGENTS.md`; this directory defines **what must be
true**, not how to work.

## Contents

- **1.** [Scope](01-scope.md)
- **2.** [Schema generation](02-schema-generation.md)
- **3.** [Storage model](03-storage-model.md)
- **4.** [Shredding and reconstruction](04-shredding-and-reconstruction.md)
- **5.** [Versioning and history](05-versioning-and-history.md)
- **6.** [Search](06-search.md)
- **7.** [REST API](07-rest-api.md)
- **8.** [CLI](08-cli.md)
- **9.** [Validation](09-validation.md)
- **10.** [Operations](10-operations.md)
- **11.** [Conformance testing](11-conformance-testing.md)
- **12.** [Trust, principal, and audit](12-trust-principal-and-audit.md)
- **13.** [Compliance mapping](13-compliance-mapping.md)

Each file is a section of one specification, split so that a section can
be read, reviewed, and cited on its own. Requirement numbers are stable
across the split: `M3.16b` is `M3.16b` wherever it moved to.
