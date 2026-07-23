//! Writing resources into the database.
//!
//! Ports the loaders in `load.go`. Specified in `spec/index.md` §8.
//!
//! Two modes exist because they suit different inputs: `insert` batches
//! `INSERT … ON CONFLICT DO NOTHING`, `copy` streams `COPY … FROM STDIN`. They
//! MUST produce identical rows for identical input (spec §8.1); the difference
//! is throughput, not semantics.
//!
//! This module holds what they share: the per-resource pipeline of spec §8.2,
//! and the tally that makes skipped resources visible instead of leaving them
//! in scrollback.

pub mod copy;
pub mod insert;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::assets::FhirVersion;
use crate::error::{Error, Result};
use crate::transform::transform_resource;

/// How a load run is configured.
#[derive(Clone, Copy, Debug)]
pub struct LoadOptions {
    /// The FHIR version whose transform map and resource set apply.
    pub version: FhirVersion,
    /// Abort on the first resource that cannot be prepared, instead of skipping
    /// and tallying it (decision D10).
    pub strict: bool,
    /// The transaction id written to every row.
    ///
    /// `0` matches fhirbase, which hardcodes it (`load.go:277`, defect X10);
    /// `--txid=new` allocates a real one for the run.
    pub txid: i64,
    /// How many resources to buffer before writing.
    pub batch_size: usize,
    /// Check each resource against the typed FHIR model (task T24).
    ///
    /// Only meaningful in a build with the `validate` feature; ignored
    /// otherwise, because there is no model to check against.
    pub validate: bool,
}

impl LoadOptions {
    /// fhirbase's batch size (`load.go:684`).
    pub const DEFAULT_BATCH_SIZE: usize = 2000;

    /// Options with fhirbase's defaults for the given version.
    #[must_use]
    pub fn new(version: FhirVersion) -> Self {
        Self {
            version,
            strict: false,
            txid: 0,
            batch_size: Self::DEFAULT_BATCH_SIZE,
            validate: false,
        }
    }
}

/// What a load run did.
///
/// fhirbase reports only per-type counts (`load.go:812-820`). The skip counters
/// exist because a resource that is silently dropped is worse than one that
/// fails loudly, and fhirbase drops two kinds without a trace: an unknown
/// resource type, and a transform failure whose error it prints before
/// inserting the null result anyway (defect X3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadStats {
    /// Resources written, by resource type.
    pub written: BTreeMap<String, u64>,
    /// Resources skipped because their type is not in this FHIR version.
    pub unknown_type: BTreeMap<String, u64>,
    /// Resources skipped because the transformation failed.
    pub transform_failed: u64,
    /// Resources skipped because they were not a JSON object with a type.
    pub malformed: u64,
    /// Resources that did not conform to the typed FHIR model (task T24).
    ///
    /// Counted, never skipped: validation reports, it does not gate. Zero
    /// unless `--validate` is on.
    pub not_conforming: u64,
}

impl LoadStats {
    /// Total resources written.
    #[must_use]
    pub fn total_written(&self) -> u64 {
        self.written.values().sum()
    }

    /// Total resources skipped, for any reason.
    #[must_use]
    pub fn total_skipped(&self) -> u64 {
        self.unknown_type.values().sum::<u64>() + self.transform_failed + self.malformed
    }

    fn record_written(&mut self, resource_type: &str) {
        *self.written.entry(resource_type.to_owned()).or_default() += 1;
    }

    fn record_unknown_type(&mut self, resource_type: &str) {
        *self.unknown_type.entry(resource_type.to_owned()).or_default() += 1;
    }
}

/// A resource that has passed every check and is ready to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedResource {
    /// The resource's FHIR type, e.g. `Patient`.
    pub resource_type: String,
    /// The table it belongs in: lower case, and known to exist.
    ///
    /// Callers MUST still quote it. It is safe *because* it came from the
    /// version's schema, not because of its spelling.
    pub table: &'static str,
    /// The resource's id, or `None` to have the database generate one.
    pub id: Option<String>,
    /// The transformed resource body.
    pub resource: Value,
}

/// Runs the per-resource pipeline of spec §8.2.
///
/// Returns `Ok(None)` when the resource was skipped and tallied; the caller
/// carries on. Under `strict` every skip becomes an error instead (D10).
///
/// The order matters and is the order of the spec: identify the type, check it
/// against the version's resource set **before** it can reach any SQL (X2),
/// then transform (X3), then choose the id.
///
/// # Errors
///
/// Under `strict`, returns [`Error::Transform`] for anything that would
/// otherwise be skipped.
pub fn prepare(
    resource: &Value,
    options: LoadOptions,
    stats: &mut LoadStats,
) -> Result<Option<PreparedResource>> {
    let Some(object) = resource.as_object() else {
        stats.malformed += 1;
        return if options.strict {
            Err(Error::transform("<unknown>", "not a JSON object"))
        } else {
            Ok(None)
        };
    };

    let Some(resource_type) = object.get("resourceType").and_then(Value::as_str) else {
        stats.malformed += 1;
        return if options.strict {
            Err(Error::transform(
                "<unknown>",
                "no string `resourceType` field",
            ))
        } else {
            Ok(None)
        };
    };

    // Defect X2. This is the only thing standing between resource content and
    // an identifier in a SQL string, so it happens before anything else can go
    // wrong. fhirbase lowercases and interpolates without checking, which makes
    // `Group` unloadable and resource data executable.
    let Some(table) = options.version.table_for(resource_type) else {
        stats.record_unknown_type(resource_type);
        return if options.strict {
            Err(Error::transform(
                resource_type,
                format!(
                    "not a resource type in FHIR {}; refusing to use it as a table name",
                    options.version
                ),
            ))
        } else {
            Ok(None)
        };
    };

    // Task T24. Validation happens on the resource AS READ, before the
    // transformation, because the model describes FHIR's own JSON rather than
    // the storage representation. It reports and continues: the loader is meant
    // to store data a strict model would reject.
    #[cfg(feature = "validate")]
    if options.validate {
        let findings = crate::validate::validate_resource(resource, options.version);
        if !findings.is_clean() {
            stats.not_conforming += 1;
            if options.strict {
                return Err(Error::transform(
                    resource_type,
                    format!("does not conform: {}", findings.issues.join("; ")),
                ));
            }
            for issue in &findings.issues {
                eprintln!("{resource_type}: {issue}");
            }
        }
    }

    // Defect X3. fhirbase shadows the error here, prints it, and queues the
    // possibly-null result for insertion regardless.
    let map = options.version.transform_map()?;
    let transformed = match transform_resource(resource, map) {
        Ok(value) => value,
        Err(e) => {
            stats.transform_failed += 1;
            return if options.strict {
                Err(e)
            } else {
                eprintln!("skipping a {resource_type} resource: {e}");
                Ok(None)
            };
        }
    };

    // An id that is present but empty is treated as absent, as fhirbase does
    // (`load.go:701-703`).
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned);

    Ok(Some(PreparedResource {
        resource_type: resource_type.to_owned(),
        table,
        id,
        resource: transformed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options() -> LoadOptions {
        LoadOptions::new(FhirVersion::V4_0_0)
    }

    #[test]
    fn a_normal_resource_prepares() {
        let mut stats = LoadStats::default();
        let prepared = prepare(
            &json!({"resourceType": "Patient", "id": "p1", "deceasedBoolean": true}),
            options(),
            &mut stats,
        )
        .unwrap()
        .unwrap();

        assert_eq!(prepared.resource_type, "Patient");
        assert_eq!(prepared.table, "patient");
        assert_eq!(prepared.id.as_deref(), Some("p1"));
        // The body is transformed, not raw.
        assert_eq!(prepared.resource["deceased"], json!({"boolean": true}));
        assert_eq!(stats.total_skipped(), 0);
    }

    #[test]
    fn the_reserved_word_resource_type_prepares() {
        // Defect X2. fhirbase cannot load this at all: it builds
        // `INSERT INTO group`, which is a syntax error.
        let mut stats = LoadStats::default();
        let prepared = prepare(
            &json!({"resourceType": "Group", "id": "g1"}),
            options(),
            &mut stats,
        )
        .unwrap()
        .unwrap();
        assert_eq!(prepared.table, "group");
    }

    #[test]
    fn a_missing_or_empty_id_is_left_to_the_database() {
        let mut stats = LoadStats::default();
        for resource in [
            json!({"resourceType": "Patient"}),
            json!({"resourceType": "Patient", "id": ""}),
            json!({"resourceType": "Patient", "id": 42}),
        ] {
            let prepared = prepare(&resource, options(), &mut stats).unwrap().unwrap();
            assert_eq!(prepared.id, None, "{resource}");
        }
    }

    #[test]
    fn a_hostile_resource_type_is_rejected_not_interpolated() {
        // Defect X2's other half. These never reach a SQL string.
        let mut stats = LoadStats::default();
        for hostile in [
            "patient; DROP TABLE patient; --",
            "patient\" ; --",
            "pg_class",
            "NoSuchResource",
            "patient",
        ] {
            let outcome = prepare(
                &json!({"resourceType": hostile, "id": "x"}),
                options(),
                &mut stats,
            )
            .unwrap();
            assert!(outcome.is_none(), "{hostile:?} must be skipped");
        }
        assert_eq!(stats.total_skipped(), 5);
        assert_eq!(stats.unknown_type.len(), 5);
    }

    #[test]
    fn strict_turns_a_skip_into_an_error() {
        // Decision D10.
        let mut options = options();
        options.strict = true;
        let mut stats = LoadStats::default();

        let err = prepare(
            &json!({"resourceType": "NoSuchResource"}),
            options,
            &mut stats,
        )
        .unwrap_err();
        assert!(err.to_string().contains("NoSuchResource"), "{err}");
        assert!(err.to_string().contains("refusing"), "{err}");
    }

    #[test]
    fn a_malformed_resource_is_tallied_separately() {
        let mut stats = LoadStats::default();
        for bad in [json!([1, 2]), json!("text"), json!({"no": "type"})] {
            assert!(prepare(&bad, options(), &mut stats).unwrap().is_none());
        }
        assert_eq!(stats.malformed, 3);
        assert_eq!(stats.transform_failed, 0);
        assert!(stats.unknown_type.is_empty());
    }

    #[test]
    fn stats_separate_written_from_skipped() {
        let mut stats = LoadStats::default();
        stats.record_written("Patient");
        stats.record_written("Patient");
        stats.record_written("Observation");
        stats.record_unknown_type("Nonsense");
        stats.transform_failed += 1;
        stats.malformed += 2;

        assert_eq!(stats.total_written(), 3);
        assert_eq!(stats.written["Patient"], 2);
        assert_eq!(stats.total_skipped(), 4);
    }

    #[test]
    fn an_unknown_type_in_one_version_may_be_known_in_another() {
        // ServiceRequest is R4; it does not exist in DSTU2.
        let mut stats = LoadStats::default();
        let resource = json!({"resourceType": "ServiceRequest", "id": "s1"});

        assert!(prepare(&resource, options(), &mut stats).unwrap().is_some());
        assert!(
            prepare(&resource, LoadOptions::new(FhirVersion::V1_0_2), &mut stats)
                .unwrap()
                .is_none()
        );
    }
}
