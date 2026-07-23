//! The Bulk Data API client.
//!
//! Ports `bulk.go`. Implements the SMART/HL7 asynchronous export flow: kick off
//! an export, poll a status URL until it completes, then download the NDJSON
//! files it lists.
//!
//! # Defect X8
//!
//! fhirbase discards the error from `http.NewRequest` and then calls
//! `req.Header.Add` on the possibly-`nil` result (`bulk.go:73-75`, `212-214`),
//! and reports a non-200 download before using the body anyway
//! (`bulk.go:221-223`). Every fallible step here is propagated, and a non-2xx
//! response is a hard error for that file.
//!
//! # Downloads stay compressed
//!
//! `Accept-Encoding: gzip` is set explicitly and `reqwest`'s `gzip` feature is
//! deliberately off, so the response body is written to disk still compressed —
//! exactly what fhirbase does. The loader detects gzip by content anyway
//! (spec §5.1), so nothing downstream needs to know.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};

/// How the Bulk Data client behaves.
#[derive(Clone, Debug)]
pub struct BulkOptions {
    /// Value for the `Accept` header on the kickoff request.
    ///
    /// Implementations disagree: Cerner wants `application/ndjson`, SMART wants
    /// `application/fhir+json`. fhirbase exposes this for the same reason.
    pub accept_header: String,
    /// How many files to download at once.
    pub parallel: usize,
    /// How long to wait between status polls.
    pub poll_interval: Duration,
    /// How many times to poll before giving up.
    pub max_polls: usize,
}

impl Default for BulkOptions {
    fn default() -> Self {
        Self {
            accept_header: "application/fhir+json".to_owned(),
            parallel: 5,
            // fhirbase's interval (`bulk.go:109`).
            poll_interval: Duration::from_secs(1),
            // fhirbase polls forever. An export that never completes should not
            // hang a load indefinitely; an hour at one second is generous.
            max_polls: 3600,
        }
    }
}

/// Files downloaded from a Bulk Data endpoint.
///
/// The scratch directory is removed when this is dropped, which covers the
/// success path, the error path, and an early return — fhirbase relies on a
/// `defer` that only covers the first two (`load.go:875-879`).
#[derive(Debug)]
pub struct Downloads {
    /// Held only for its `Drop`, which removes the directory and everything in
    /// it. Never read — that is the point.
    #[expect(dead_code, reason = "kept alive so Drop removes the scratch directory")]
    directory: tempfile::TempDir,
    files: Vec<PathBuf>,
}

impl Downloads {
    /// The downloaded files, in the order the server listed them.
    #[must_use]
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Moves the files into `destination` and returns where they landed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Bulk`] if the destination cannot be created or a file
    /// cannot be moved.
    pub fn move_into(self, destination: &Path) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(destination).map_err(|e| {
            Error::Bulk(format!(
                "cannot create {}: {e}",
                destination.display()
            ))
        })?;

        let mut moved = Vec::with_capacity(self.files.len());
        for file in &self.files {
            let Some(name) = file.file_name() else {
                continue;
            };
            let target = destination.join(name);

            // `rename` fails across filesystems, and the scratch directory is
            // very often on a different one from the destination — fhirbase
            // uses `os.Rename` alone and simply reports the failure
            // (`bulk.go:363-367`).
            match std::fs::rename(file, &target) {
                Ok(()) => {}
                Err(_) => {
                    std::fs::copy(file, &target).map_err(|e| {
                        Error::Bulk(format!(
                            "cannot move {} to {}: {e}",
                            file.display(),
                            target.display()
                        ))
                    })?;
                }
            }
            moved.push(target);
        }

        // The scratch directory goes away with `self`.
        Ok(moved)
    }
}

/// Runs the whole export flow and returns the downloaded files.
///
/// # Errors
///
/// Returns [`Error::Bulk`] if any step fails.
pub async fn fetch(url: &str, options: &BulkOptions) -> Result<Downloads> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| Error::Bulk(format!("cannot build the HTTP client: {e}")))?;

    let status_url = kickoff(&client, url, &options.accept_header).await?;
    let file_urls = poll_until_ready(&client, &status_url, options).await?;

    if file_urls.is_empty() {
        return Err(Error::Bulk(
            "the export completed but listed no files".to_owned(),
        ));
    }

    download_all(&client, &file_urls, options).await
}

/// Starts an export and returns the status URL to poll.
///
/// # Errors
///
/// Returns [`Error::Bulk`] on a transport failure, a non-2xx response, or a
/// missing `Content-Location` header.
pub async fn kickoff(client: &reqwest::Client, url: &str, accept: &str) -> Result<String> {
    let response = client
        .get(url)
        .header("Prefer", "respond-async")
        .header("Accept", accept)
        .send()
        .await
        .map_err(|e| Error::Bulk(format!("cannot reach {url}: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::Bulk(format!(
            "the export request to {url} returned {status}; response body:\n{body}"
        )));
    }

    response
        .headers()
        .get("Content-Location")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::Bulk(format!(
                "{url} accepted the export but returned no Content-Location header, \
                 so there is no status URL to poll"
            ))
        })
}

/// Polls the status URL until the export completes, then returns the file URLs.
///
/// # Errors
///
/// Returns [`Error::Bulk`] on a transport failure, a non-2xx response, a
/// manifest that cannot be parsed, or exhausting `max_polls`.
pub async fn poll_until_ready(
    client: &reqwest::Client,
    status_url: &str,
    options: &BulkOptions,
) -> Result<Vec<String>> {
    println!("Waiting for the Bulk Data server to prepare files...");

    for attempt in 1..=options.max_polls {
        let response = client
            .get(status_url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Error::Bulk(format!("cannot poll {status_url}: {e}")))?;

        let status = response.status();

        if status.as_u16() == 200 {
            let body = response
                .bytes()
                .await
                .map_err(|e| Error::Bulk(format!("cannot read the export manifest: {e}")))?;
            return parse_manifest(&body);
        }

        // 202 means "still working". Anything else is fatal — fhirbase treats
        // any non-2xx as fatal but keeps looping on other 2xx codes.
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Bulk(format!(
                "polling {status_url} returned {status}; response body:\n{body}"
            )));
        }

        if attempt.is_multiple_of(5) {
            println!("still waiting... ({attempt} polls)");
        }
        tokio::time::sleep(options.poll_interval).await;
    }

    Err(Error::Bulk(format!(
        "the export at {status_url} did not complete after {} polls",
        options.max_polls
    )))
}

/// Extracts `output[].url` from an export manifest.
fn parse_manifest(body: &[u8]) -> Result<Vec<String>> {
    let manifest: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| Error::Bulk(format!("the export manifest is not JSON: {e}")))?;

    let Some(object) = manifest.as_object() else {
        return Err(Error::Bulk(
            "the export manifest is not a JSON object".to_owned(),
        ));
    };

    let Some(output) = object.get("output") else {
        return Err(Error::Bulk(
            "the export manifest has no `output` field".to_owned(),
        ));
    };

    let Some(entries) = output.as_array() else {
        return Err(Error::Bulk(
            "the export manifest's `output` is not an array".to_owned(),
        ));
    };

    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            entry
                .get("url")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    Error::Bulk(format!(
                        "the export manifest's output[{i}] has no string `url`"
                    ))
                })
        })
        .collect()
}

/// Downloads every file, `parallel` at a time.
async fn download_all(
    client: &reqwest::Client,
    urls: &[String],
    options: &BulkOptions,
) -> Result<Downloads> {
    let directory = tempfile::Builder::new()
        .prefix("fhirpg-bulk-")
        .tempdir()
        .map_err(|e| Error::Bulk(format!("cannot create a scratch directory: {e}")))?;

    println!(
        "Downloading {} file(s), {} at a time",
        urls.len(),
        options.parallel.max(1)
    );

    let progress = indicatif::ProgressBar::new(urls.len() as u64);
    progress.set_style(
        indicatif::ProgressStyle::with_template("downloading {wide_bar} {pos}/{len} files")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar()),
    );

    let mut files = Vec::with_capacity(urls.len());
    let mut failures = Vec::new();

    // Chunked rather than a full task pool: it bounds concurrency exactly, and
    // the ordering of `files` stays the server's ordering, which matters
    // because copy mode's speed depends on resources arriving grouped.
    for chunk in urls.chunks(options.parallel.max(1)) {
        let downloads = chunk.iter().enumerate().map(|(i, url)| {
            let path = directory.path().join(file_name_for(url, files.len() + i));
            async move {
                let outcome = download_one(client, url, &path).await;
                (url.clone(), path, outcome)
            }
        });

        for (url, path, outcome) in futures_util::future::join_all(downloads).await {
            match outcome {
                Ok(()) => files.push(path),
                Err(e) => failures.push(format!("{url}: {e}")),
            }
            progress.inc(1);
        }
    }

    progress.finish_and_clear();

    // Partial success is reported, not swallowed. fhirbase prints each failure
    // and carries on with however many files it got, which silently loads an
    // incomplete export.
    if !failures.is_empty() {
        return Err(Error::Bulk(format!(
            "{} of {} file(s) failed to download; refusing to load a partial export:\n  {}",
            failures.len(),
            urls.len(),
            failures.join("\n  ")
        )));
    }

    println!("Finished downloading {} file(s)", files.len());
    Ok(Downloads { directory, files })
}

/// Downloads one file to `path`, streaming rather than buffering.
async fn download_one(client: &reqwest::Client, url: &str, path: &Path) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let response = client
        .get(url)
        // Deliberately not decompressed: the file stays gzipped on disk and the
        // loader detects it by content.
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .map_err(|e| Error::Bulk(format!("request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        // Defect X8: fhirbase reports this and then reads the body anyway.
        return Err(Error::Bulk(format!("server returned {status}")));
    }

    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| Error::Bulk(format!("cannot create {}: {e}", path.display())))?;

    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::Bulk(format!("cannot read the response body: {e}")))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|e| Error::Bulk(format!("cannot write {}: {e}", path.display())))?;
    }

    file.flush()
        .await
        .map_err(|e| Error::Bulk(format!("cannot flush {}: {e}", path.display())))?;

    Ok(())
}

/// Picks a filename for a downloaded URL.
///
/// The index disambiguates: Bulk Data servers routinely serve every file from
/// the same path with different query strings, and fhirbase's `path.Base`
/// alone would collide (`bulk.go:196-198`).
fn file_name_for(url: &str, index: usize) -> String {
    let base = url
        .split('?')
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("bulk");

    let cleaned: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .take(64)
        .collect();

    if cleaned.is_empty() {
        format!("{index:04}.ndjson")
    } else {
        format!("{index:04}-{cleaned}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parsing_extracts_every_url() {
        let body = br#"{"transactionTime":"2020","output":[
            {"type":"Patient","url":"http://example.com/1.ndjson"},
            {"type":"Observation","url":"http://example.com/2.ndjson"}
        ]}"#;
        assert_eq!(
            parse_manifest(body).unwrap(),
            vec![
                "http://example.com/1.ndjson".to_owned(),
                "http://example.com/2.ndjson".to_owned()
            ]
        );
    }

    #[test]
    fn an_empty_output_array_parses_to_nothing() {
        assert!(parse_manifest(br#"{"output":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn a_malformed_manifest_says_what_is_wrong() {
        let cases: &[(&[u8], &str)] = &[
            (b"not json", "not JSON"),
            (b"[1,2,3]", "not a JSON object"),
            (br#"{"foo":1}"#, "no `output`"),
            (br#"{"output":"nope"}"#, "not an array"),
            (br#"{"output":[{"type":"Patient"}]}"#, "output[0]"),
            (br#"{"output":[{"url":42}]}"#, "output[0]"),
        ];
        for (body, expected) in cases {
            let err = parse_manifest(body).unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "{:?}: expected {expected:?}, got {err}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn file_names_are_unique_and_safe() {
        // Servers commonly serve every file from one path with a query string.
        assert_eq!(file_name_for("http://x/f?id=1", 0), "0000-f");
        assert_eq!(file_name_for("http://x/f?id=2", 1), "0001-f");
        // Path traversal and separators cannot survive.
        let hostile = file_name_for("http://x/../../etc/passwd", 2);
        assert_eq!(hostile, "0002-passwd");
        assert!(!hostile.contains('/'));
        assert!(!hostile.contains(".."));
        // A trailing slash still yields something usable.
        assert_eq!(file_name_for("http://x/", 3), "0003-x");
    }
}

/// Tests against a mock Bulk Data server.
///
/// The client is never tested against a live server: these cover the flows that
/// matter, including the ones a real server would only produce occasionally.
#[cfg(test)]
mod server_tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fast_options() -> BulkOptions {
        BulkOptions {
            poll_interval: Duration::from_millis(1),
            max_polls: 20,
            ..BulkOptions::default()
        }
    }

    #[tokio::test]
    async fn a_complete_export_downloads_every_file() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/Patient/$export"))
            .and(header("Prefer", "respond-async"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Content-Location", format!("{}/status", server.uri()).as_str()),
            )
            .mount(&server)
            .await;

        let manifest = serde_json::json!({
            "output": [
                {"type": "Patient", "url": format!("{}/files/patients.ndjson", server.uri())},
                {"type": "Observation", "url": format!("{}/files/obs.ndjson", server.uri())}
            ]
        });
        Mock::given(method("GET"))
            .and(path("/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(manifest))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/files/patients.ndjson"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "{\"resourceType\":\"Patient\",\"id\":\"1\"}\n",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/files/obs.ndjson"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "{\"resourceType\":\"Observation\",\"id\":\"2\"}\n",
            ))
            .mount(&server)
            .await;

        let downloads = fetch(
            &format!("{}/Patient/$export", server.uri()),
            &fast_options(),
        )
        .await
        .expect("the export should succeed");

        assert_eq!(downloads.files().len(), 2);
        let first = std::fs::read_to_string(&downloads.files()[0]).unwrap();
        assert!(first.contains("Patient"), "{first}");
    }

    #[tokio::test]
    async fn polling_continues_until_the_export_is_ready() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/export"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Content-Location", format!("{}/status", server.uri()).as_str()),
            )
            .mount(&server)
            .await;

        // Three 202s, then the manifest.
        Mock::given(method("GET"))
            .and(path("/status"))
            .respond_with(ResponseTemplate::new(202))
            .up_to_n_times(3)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/status"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"output": []})),
            )
            .mount(&server)
            .await;

        let err = fetch(&format!("{}/export", server.uri()), &fast_options())
            .await
            .expect_err("an empty output should be reported");
        assert!(err.to_string().contains("listed no files"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_kickoff_includes_the_response_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/export"))
            .respond_with(
                ResponseTemplate::new(403).set_body_string("{\"issue\":[{\"details\":\"nope\"}]}"),
            )
            .mount(&server)
            .await;

        let err = fetch(&format!("{}/export", server.uri()), &fast_options())
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("403"), "{message}");
        assert!(message.contains("nope"), "the body must be shown: {message}");
    }

    #[tokio::test]
    async fn a_missing_content_location_is_reported_clearly() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/export"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let err = fetch(&format!("{}/export", server.uri()), &fast_options())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Content-Location"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_server_error_while_polling_is_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/export"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Content-Location", format!("{}/status", server.uri()).as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/status"))
            .respond_with(ResponseTemplate::new(500).set_body_string("exploded"))
            .mount(&server)
            .await;

        let err = fetch(&format!("{}/export", server.uri()), &fast_options())
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("500"), "{message}");
        assert!(message.contains("exploded"), "{message}");
    }

    #[tokio::test]
    async fn an_export_that_never_completes_gives_up() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/export"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Content-Location", format!("{}/status", server.uri()).as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/status"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let options = BulkOptions {
            poll_interval: Duration::from_millis(1),
            max_polls: 3,
            ..BulkOptions::default()
        };
        let err = fetch(&format!("{}/export", server.uri()), &options)
            .await
            .expect_err("fhirbase would poll forever; we give up");
        assert!(err.to_string().contains("did not complete"), "{err}");
    }

    #[tokio::test]
    async fn one_failing_file_fails_the_whole_export() {
        // Defect X8's third part. fhirbase reports the failure and loads
        // whatever else arrived, which silently imports a partial export.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/export"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Content-Location", format!("{}/status", server.uri()).as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [
                    {"url": format!("{}/files/good.ndjson", server.uri())},
                    {"url": format!("{}/files/gone.ndjson", server.uri())}
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/files/good.ndjson"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}\n"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/files/gone.ndjson"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = fetch(&format!("{}/export", server.uri()), &fast_options())
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("1 of 2"), "{message}");
        assert!(message.contains("partial export"), "{message}");
        assert!(message.contains("404"), "{message}");
    }

    #[tokio::test]
    async fn the_accept_header_is_configurable() {
        // Cerner wants application/ndjson; SMART wants application/fhir+json.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/export"))
            .and(header("Accept", "application/ndjson"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Content-Location", format!("{}/status", server.uri()).as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/status"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"output": []})),
            )
            .mount(&server)
            .await;

        let options = BulkOptions {
            accept_header: "application/ndjson".to_owned(),
            ..fast_options()
        };
        // Reaching "listed no files" proves the kickoff matched the header.
        let err = fetch(&format!("{}/export", server.uri()), &options)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("listed no files"), "{err}");
    }

    #[tokio::test]
    async fn the_scratch_directory_is_removed_when_downloads_are_dropped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/export"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Content-Location", format!("{}/status", server.uri()).as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [{"url": format!("{}/f.ndjson", server.uri())}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/f.ndjson"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}\n"))
            .mount(&server)
            .await;

        let downloads = fetch(&format!("{}/export", server.uri()), &fast_options())
            .await
            .unwrap();
        let path = downloads.files()[0].clone();
        assert!(path.exists());

        drop(downloads);
        assert!(!path.exists(), "the scratch directory must be removed");
    }
}
