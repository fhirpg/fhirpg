//! Optional structural validation against the typed FHIR model.
//!
//! Task T24, behind the non-default `validate` feature.
//!
//! ```sh
//! cargo build --features validate
//! fhirpg --db clinic load --validate export/*.ndjson
//! ```
//!
//! # Why this is optional, and off
//!
//! The loader carries resources as `serde_json::Value` on purpose: it must
//! accept any FHIR version, unknown resource types, and non-conforming data,
//! exactly as fhirbase does (see `AGENTS/architecture.md`, *The hot path*).
//! Deserializing into a typed model would reject data the tool is supposed to
//! store.
//!
//! So validation is a **report**, never a gate on the default path: it says
//! what is wrong and the load continues, unless `--strict` says otherwise. The
//! feature is off by default because it compiles roughly 135,000 lines of
//! generated Rust that a normal load never touches.
//!
//! # Only R5
//!
//! The [`fhir`] crate models each release separately and behind its own
//! feature. This build enables `r5` alone, so `--validate` applies to FHIR
//! 5.0.0 and reports a clear error for anything else rather than pretending to
//! check it. Enabling `r3` or `r4` as well would be a one-line change here and
//! another 270,000 lines of generated code.

use fhir::r5::resources::Resource;
use fhir::r5::validate::Validate;
use serde_json::Value;

use crate::assets::FhirVersion;
use crate::error::{Error, Result};

/// What validating one resource found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Findings {
    /// One line per problem, already formatted for reporting.
    pub issues: Vec<String>,
}

impl Findings {
    /// Whether the resource is free of findings.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Checks that this build can validate the requested FHIR version.
///
/// # Errors
///
/// Returns [`Error::Config`] when the version has no model in this build.
pub fn check_supported(version: FhirVersion) -> Result<()> {
    if version == FhirVersion::V5_0_0 {
        return Ok(());
    }
    Err(Error::Config(format!(
        "--validate needs the typed FHIR model, and this build only has R5; \
         cannot validate FHIR {version}. Load without --validate, or use --fhir 5.0.0."
    )))
}

/// Validates one resource against the typed model.
///
/// Takes the resource **as read**, before transformation: the model describes
/// FHIR's own JSON, not the storage representation the transformation produces.
///
/// # Errors
///
/// Never fails. A resource that will not deserialize is itself a finding —
/// that is the most useful thing validation can report — so it is returned as
/// one rather than as an error.
pub fn validate_resource(resource: &Value, version: FhirVersion) -> Findings {
    if version != FhirVersion::V5_0_0 {
        return Findings::default();
    }

    let typed: Resource = match serde_json::from_value(resource.clone()) {
        Ok(typed) => typed,
        Err(e) => {
            return Findings {
                issues: vec![format!("does not match the FHIR R5 model: {e}")],
            };
        }
    };

    Findings {
        issues: typed
            .validate()
            .into_iter()
            .map(|issue| format!("{}: {}", issue.path, issue.message))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_conforming_resource_has_no_findings() {
        let findings = validate_resource(
            &json!({
                "resourceType": "Patient",
                "id": "p1",
                "active": true,
                "gender": "male",
                "name": [{"family": "Chalmers", "given": ["Peter"]}]
            }),
            FhirVersion::V5_0_0,
        );
        assert!(findings.is_clean(), "unexpected findings: {:?}", findings.issues);
    }

    #[test]
    fn a_code_outside_its_required_value_set_is_reported() {
        // `Patient.gender` has a required binding to administrative-gender.
        let findings = validate_resource(
            &json!({"resourceType": "Patient", "gender": "platypus"}),
            FhirVersion::V5_0_0,
        );
        assert!(!findings.is_clean());
        let joined = findings.issues.join("; ");
        assert!(joined.contains("gender"), "{joined}");
        assert!(joined.contains("required value set"), "{joined}");
    }

    #[test]
    fn a_malformed_primitive_is_reported() {
        // `implicitRules` is a uri, which may not be blank or padded.
        let findings = validate_resource(
            &json!({"resourceType": "Patient", "implicitRules": "  "}),
            FhirVersion::V5_0_0,
        );
        assert!(!findings.is_clean());
        assert!(
            findings.issues.iter().any(|i| i.contains("uri")),
            "{:?}",
            findings.issues
        );
    }

    #[test]
    fn a_missing_required_field_is_reported() {
        // Observation.status is 1..1.
        let findings = validate_resource(
            &json!({"resourceType": "Observation"}),
            FhirVersion::V5_0_0,
        );
        assert!(!findings.is_clean());
        assert!(
            findings.issues[0].contains("status"),
            "{:?}",
            findings.issues
        );
    }

    #[test]
    fn what_the_model_does_not_check_is_documented_by_this_test() {
        // Worth pinning so nobody assumes more coverage than exists. The model
        // types `Patient.id` and `Patient.birthDate` as plain strings, so their
        // FHIR format constraints are not enforced. `--validate` is a
        // structural check, not a conformance suite.
        for lenient in [
            json!({"resourceType": "Patient", "id": "has spaces"}),
            json!({"resourceType": "Patient", "birthDate": "not-a-date"}),
        ] {
            assert!(
                validate_resource(&lenient, FhirVersion::V5_0_0).is_clean(),
                "{lenient} unexpectedly reported findings"
            );
        }
    }

    #[test]
    fn a_resource_the_model_cannot_read_is_a_finding_not_an_error() {
        // The most useful thing validation can say. Note this is exactly the
        // data the loader must still store, which is why validation reports
        // rather than gates.
        let findings = validate_resource(
            &json!({"resourceType": "Patient", "active": "not-a-boolean"}),
            FhirVersion::V5_0_0,
        );
        assert!(!findings.is_clean());
        assert!(
            findings.issues[0].contains("does not match the FHIR R5 model"),
            "{:?}",
            findings.issues
        );
    }

    #[test]
    fn an_unknown_resource_type_is_a_finding() {
        let findings = validate_resource(
            &json!({"resourceType": "NoSuchResource", "x": 1}),
            FhirVersion::V5_0_0,
        );
        assert!(!findings.is_clean());
    }

    #[test]
    fn only_r5_is_supported_in_this_build() {
        assert!(check_supported(FhirVersion::V5_0_0).is_ok());

        let err = check_supported(FhirVersion::V4_0_0).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("only has R5"), "{message}");
        assert!(message.contains("4.0.0"), "{message}");
        // And it says what to do about it.
        assert!(message.contains("--fhir 5.0.0"), "{message}");
    }

    #[test]
    fn an_unsupported_version_validates_nothing_rather_than_lying() {
        // `check_supported` rejects this before a load starts; if it were ever
        // reached anyway, reporting no findings is honest — reporting findings
        // from the wrong model would not be.
        let findings = validate_resource(
            &json!({"resourceType": "Patient", "id": "has spaces"}),
            FhirVersion::V4_0_0,
        );
        assert!(findings.is_clean());
    }
}

