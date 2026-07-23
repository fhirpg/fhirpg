//! The `transform` subcommand.
//!
//! Ports `TransformCommand` (`transform.go:198-235`): read one FHIR resource
//! from a JSON file, transform it, and write the result to stdout as
//! two-space-indented JSON.
//!
//! Exists mostly to demonstrate and debug the transformation, which is what
//! rewrites a resource into the representation stored in the `resource` `jsonb`
//! column. The algorithm is specified in `spec/index.md` §4.

use std::io::Write;
use std::path::Path;

use crate::assets::FhirVersion;
use crate::error::{Error, Result};
use crate::transform::transform_resource;

/// Runs the `transform` subcommand.
///
/// # Errors
///
/// Returns [`Error::Bundle`] if the file cannot be read or is not valid JSON,
/// [`Error::Transform`] if the resource cannot be transformed, and
/// [`Error::Io`] if stdout cannot be written.
pub fn run(file: &Path, version: FhirVersion) -> Result<()> {
    let display = file.display();

    let content = std::fs::read_to_string(file)
        .map_err(|e| Error::bundle(display.to_string(), format!("cannot read file: {e}")))?;

    let resource: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| Error::bundle(display.to_string(), format!("cannot parse as JSON: {e}")))?;

    let map = version.transform_map()?;
    let transformed = transform_resource(&resource, map)?;

    // Two-space indent, matching `jsoniter.MarshalIndent(out, "", "  ")`
    // (transform.go:229).
    let rendered = serde_json::to_string_pretty(&transformed)
        .map_err(|e| Error::Asset(format!("cannot serialize the transformed resource: {e}")))?;

    let mut stdout = std::io::stdout().lock();
    stdout.write_all(rendered.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `content` to a uniquely named file under the system temp
    /// directory and returns the path. Avoids a tempfile dependency for three
    /// tests. The name includes the process id so concurrent runs do not
    /// collide.
    fn scratch_file(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("fhirpg-{}-{name}", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn missing_file_names_the_path() {
        let err = run(Path::new("/no/such/patient.json"), FhirVersion::V4_0_0).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("/no/such/patient.json"), "{message}");
        assert!(message.contains("cannot read file"), "{message}");
    }

    #[test]
    fn malformed_json_names_the_path() {
        let path = scratch_file("transform_malformed.json", "{not json");
        let err = run(&path, FhirVersion::V4_0_0).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("cannot parse as JSON"), "{message}");
    }

    #[test]
    fn a_valid_resource_transforms_and_writes() {
        let path = scratch_file(
            "transform_patient.json",
            r#"{"resourceType":"Patient","deceasedBoolean":true}"#,
        );
        assert!(run(&path, FhirVersion::V4_0_0).is_ok());
    }
}
