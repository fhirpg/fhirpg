//! The crate's typed error.
//!
//! Library code returns [`Result<T>`]; [`anyhow`] appears only at the `main`
//! boundary. Every variant names its source — the file and line for a bundle
//! error, the statement index for an `init` error, the URL for a bulk error —
//! because "something went wrong" is not actionable when a load has been
//! running for twenty minutes.
//!
//! Spec invariant 2 forbids panicking on anything derived from input, so paths
//! that fhirbase resolves with `panic!` or an unchecked type assertion resolve
//! to a variant here instead. Those are marked in the variant documentation.

/// A convenient alias for a result carrying this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in `fhirpg`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A command-line or environment setting is invalid.
    ///
    /// Includes an unrecognized `--sslmode`, which fhirbase resolves with
    /// `panic!` (`db.go:59`).
    #[error("configuration error: {0}")]
    Config(String),

    /// An embedded asset is missing, unparseable, or internally inconsistent.
    ///
    /// Asset validation happens once at load time — every `tr/move` target must
    /// resolve and every `tr/act` must be recognized (spec §3) — so that the
    /// transformation itself cannot fail on a malformed map. fhirbase defers
    /// both checks to transformation time, where they panic (defect X4) or
    /// silently null a field (defect X5).
    #[error("asset error: {0}")]
    Asset(String),

    /// The requested FHIR version is not one this build supports.
    #[error("unknown FHIR version {requested:?}; known versions are: {known}")]
    UnknownFhirVersion {
        /// The value supplied to `--fhir`.
        requested: String,
        /// The supported versions, comma-separated.
        known: String,
    },

    /// A database connection, statement, or protocol failure.
    #[error("database error: {0}")]
    Db(String),

    /// The server is older than the required PostgreSQL 18 (decision D8).
    #[error("PostgreSQL 18 or newer is required; this server reports {found}")]
    UnsupportedServerVersion {
        /// The version string the server reported.
        found: String,
    },

    /// An input file could not be opened, decoded, or understood.
    #[error("{source_name}: {message}")]
    Bundle {
        /// The file, and where in it, the problem occurred.
        source_name: String,
        /// What went wrong.
        message: String,
    },

    /// A resource could not be transformed into the storage representation.
    ///
    /// By default the loader skips and tallies these (decision D10); `--strict`
    /// turns the first one into an aborted run.
    #[error("cannot transform {resource_type} resource: {message}")]
    Transform {
        /// The resource's `resourceType`, or `"<unknown>"` when absent.
        resource_type: String,
        /// What went wrong.
        message: String,
    },

    /// A Bulk Data API interaction failed.
    #[error("bulk data error: {0}")]
    Bulk(String),

    /// An I/O failure not attributable to a specific input file.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Builds an [`Error::Bundle`] without ceremony at the call site.
    pub fn bundle(source_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Bundle {
            source_name: source_name.into(),
            message: message.into(),
        }
    }

    /// Builds an [`Error::Transform`] without ceremony at the call site.
    pub fn transform(resource_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Transform {
            resource_type: resource_type.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fhir_version_lists_the_known_ones() {
        let e = Error::UnknownFhirVersion {
            requested: "9.9.9".to_owned(),
            known: "4.0.0, 5.0.0".to_owned(),
        };
        let rendered = e.to_string();
        assert!(rendered.contains("9.9.9"), "{rendered}");
        assert!(rendered.contains("4.0.0, 5.0.0"), "{rendered}");
    }

    #[test]
    fn bundle_error_names_its_source() {
        let e = Error::bundle("patients.ndjson:42", "expected a JSON object");
        assert_eq!(
            e.to_string(),
            "patients.ndjson:42: expected a JSON object"
        );
    }

    #[test]
    fn unsupported_server_version_states_the_requirement() {
        let e = Error::UnsupportedServerVersion {
            found: "17.4".to_owned(),
        };
        let rendered = e.to_string();
        assert!(rendered.contains("18"), "{rendered}");
        assert!(rendered.contains("17.4"), "{rendered}");
    }
}
