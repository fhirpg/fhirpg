//! The `bulkget` subcommand.
//!
//! Ports `BulkGetCommand` (`bulk.go:341-372`): run a Bulk Data export and save
//! the NDJSON files it produces into a directory, without touching the
//! database.
//!
//! Useful on its own — the export can be inspected, archived, or loaded later —
//! and it is the same client `load <URL>` uses (T19).

use std::path::Path;

use crate::bulk::{self, BulkOptions};
use crate::error::Result;

/// Runs the `bulkget` subcommand.
///
/// # Errors
///
/// Returns [`crate::error::Error::Bulk`] if the export fails or the files
/// cannot be saved.
pub async fn run(url: &str, destination: &Path, options: &BulkOptions) -> Result<()> {
    let downloads = bulk::fetch(url, options).await?;
    let count = downloads.files().len();

    // `move_into` consumes the downloads, which removes the scratch directory;
    // anything that fails before this point removes it too, via `Drop`.
    let saved = downloads.move_into(destination)?;

    println!("Saved {count} file(s) to {}:", destination.display());
    for path in &saved {
        let size = std::fs::metadata(path).map_or(0, |m| m.len());
        println!(
            "  {}  {}",
            path.display(),
            crate::memory::format_bytes(size)
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fast_options() -> BulkOptions {
        BulkOptions {
            poll_interval: std::time::Duration::from_millis(1),
            max_polls: 20,
            ..BulkOptions::default()
        }
    }

    /// Mounts a server that exports two small NDJSON files.
    async fn export_server() -> MockServer {
        let server = MockServer::start().await;
        let base = server.uri();

        Mock::given(method("GET"))
            .and(path_matcher("/export"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Content-Location", format!("{base}/status").as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [
                    {"type": "Patient", "url": format!("{base}/files/patient.ndjson")},
                    {"type": "Observation", "url": format!("{base}/files/obs.ndjson")}
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/files/patient.ndjson"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("{\"resourceType\":\"Patient\",\"id\":\"p1\"}\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/files/obs.ndjson"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "{\"resourceType\":\"Observation\",\"id\":\"o1\",\"status\":\"final\"}\n",
            ))
            .mount(&server)
            .await;

        server
    }

    #[tokio::test]
    async fn files_land_in_the_destination_directory() {
        let server = export_server().await;
        let destination = tempfile::tempdir().unwrap();

        run(
            &format!("{}/export", server.uri()),
            destination.path(),
            &fast_options(),
        )
        .await
        .expect("bulkget should succeed");

        let mut names: Vec<String> = std::fs::read_dir(destination.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();

        assert_eq!(names.len(), 2, "got {names:?}");
        assert!(names[0].contains("patient.ndjson"), "{names:?}");
        assert!(names[1].contains("obs.ndjson"), "{names:?}");

        // And the contents survived the move intact.
        let content = std::fs::read_to_string(destination.path().join(&names[0])).unwrap();
        assert!(content.contains("\"p1\""), "{content}");
    }

    #[tokio::test]
    async fn a_missing_destination_directory_is_created() {
        // fhirbase's `os.Rename` into a non-existent directory just fails.
        let server = export_server().await;
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("does/not/exist/yet");

        run(
            &format!("{}/export", server.uri()),
            &destination,
            &fast_options(),
        )
        .await
        .expect("the destination should be created");

        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn a_failed_export_leaves_no_files_behind() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/export"))
            .respond_with(ResponseTemplate::new(500).set_body_string("no"))
            .mount(&server)
            .await;

        let destination = tempfile::tempdir().unwrap();
        let err = run(
            &format!("{}/export", server.uri()),
            destination.path(),
            &fast_options(),
        )
        .await
        .expect_err("a 500 should fail the command");

        assert!(err.to_string().contains("500"), "{err}");
        assert_eq!(
            std::fs::read_dir(destination.path()).unwrap().count(),
            0,
            "nothing should have been written"
        );
    }
}
