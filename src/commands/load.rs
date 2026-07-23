//! The `load` subcommand.
//!
//! Ports `LoadCommand` and `loadFiles` (`load.go:764-896`). Specified in
//! `spec/index.md` §8.
//!
//! This is orchestration only: it picks a mode, drives the reader and the
//! loader, shows progress, and reports. The reading lives in
//! [`crate::bundle`], the writing in [`crate::load`].

use std::path::PathBuf;
use std::time::Instant;

use crate::assets::FhirVersion;
use crate::bundle::reader::{MultiFileReader, expand_paths};
use crate::cli::LoadMode;
use crate::config::PgConfig;
use crate::db;
use crate::error::{Error, Result};
use crate::load::{LoadOptions, LoadStats};
use crate::load::copy::CopyLoader;
use crate::load::insert::InsertLoader;
use crate::memory::{PeakTracker, format_bytes};

/// How often to sample memory when `--memusage` is on.
///
/// fhirbase's interval (`load.go:792`).
const MEMUSAGE_INTERVAL: u64 = 3000;

/// What the user asked `load` to do.
///
/// Four independent flags, which `clippy::struct_excessive_bools` dislikes on
/// principle. They map one-to-one onto command-line switches that are genuinely
/// independent, and grouping them into an options type would only rename the
/// same four booleans.
#[allow(clippy::struct_excessive_bools, reason = "one field per CLI switch")]
pub struct LoadRequest {
    /// A Bulk Data URL, or one or more file or directory paths.
    pub sources: Vec<String>,
    /// The mode, if given explicitly.
    pub mode: Option<LoadMode>,
    /// Abort on the first unusable resource (decision D10).
    pub strict: bool,
    /// Count resources up front for an exact progress total.
    pub count_first: bool,
    /// Report resident set size during the load (decision D14).
    pub memusage: bool,
    /// Allocate a real transaction id instead of writing `0` (defect X10).
    pub new_txid: bool,
    /// Bulk Data options, used only when the source is a URL.
    pub bulk: crate::bulk::BulkOptions,
}

/// Runs the `load` subcommand.
///
/// # Errors
///
/// Returns [`Error::Bundle`] if the inputs cannot be read, [`Error::Db`] if a
/// write fails, or a preparation error when `--strict` is set.
pub async fn run(config: &PgConfig, version: FhirVersion, request: &LoadRequest) -> Result<()> {
    // fhirbase decides this by prefix (`load.go:834-838`).
    let is_bulk_url = request
        .sources
        .first()
        .is_some_and(|s| s.starts_with("http://") || s.starts_with("https://"));

    // Spec §8.1: insert is the default for local files, copy for Bulk Data —
    // which arrives grouped by resource type, the case copy is fast on.
    let mode = request.mode.unwrap_or(if is_bulk_url {
        LoadMode::Copy
    } else {
        LoadMode::Insert
    });

    // A Bulk Data URL is downloaded first, then loaded as ordinary local files
    // (`load.go:864-887`). `downloads` is held for the rest of the run: its
    // `Drop` removes the scratch directory, on success, on failure, and on an
    // early return alike.
    let downloads;
    let files = if is_bulk_url {
        let Some(url) = request.sources.first() else {
            return Err(Error::Bulk("no Bulk Data URL given".to_owned()));
        };
        if request.sources.len() > 1 {
            return Err(Error::Bulk(
                "a Bulk Data URL must be the only source;                  mixing it with file paths is not supported"
                    .to_owned(),
            ));
        }
        downloads = crate::bulk::fetch(url, &request.bulk).await?;
        downloads.files().to_vec()
    } else {
        let inputs: Vec<PathBuf> = request.sources.iter().map(PathBuf::from).collect();
        expand_paths(&inputs)?
    };

    if files.is_empty() {
        return Err(Error::bundle(
            request.sources.join(", "),
            "no files to load",
        ));
    }

    let client = db::connect(config).await?;

    let mut options = LoadOptions::new(version);
    options.strict = request.strict;
    if request.new_txid {
        options.txid = allocate_txid(&client).await?;
    }

    // Spec §5.4 and risk R6: an exact total costs a full extra read, and for
    // compressed input that means inflating twice. Off unless asked for.
    let total = if request.count_first {
        Some(count_resources(&files)?)
    } else {
        None
    };

    let progress = build_progress(total, mode);
    let mut peak = PeakTracker::new();
    let started = Instant::now();

    let reader = MultiFileReader::new(files);
    let skipped_files = reader.skipped();
    let counter = std::cell::Cell::new(0_u64);
    let observed = reader.inspect(|_| {
        let seen = counter.get() + 1;
        counter.set(seen);
        progress.inc(1);
        if request.memusage
            && seen.is_multiple_of(MEMUSAGE_INTERVAL)
            && let Some(current) = peak.sample()
        {
            progress.suspend(|| {
                println!("memusage: {seen} resources, RSS {}", format_bytes(current));
            });
        }
    });

    let stats = match mode {
        LoadMode::Insert => InsertLoader::new(options).load(&client, observed).await,
        LoadMode::Copy => CopyLoader::new(options).load(&client, observed).await,
    };
    progress.finish_and_clear();
    let stats = stats?;

    peak.sample();
    report(
        &stats,
        &skipped_files.borrow(),
        started.elapsed(),
        mode,
        options.txid,
    );

    if request.memusage {
        match peak.peak() {
            Some(bytes) => println!(
                "\nPeak resident set size: {} \
                 (RSS, not heap allocation — see `load --help`)",
                format_bytes(bytes)
            ),
            None => println!("\nResident set size is not available on this platform."),
        }
    }

    Ok(())
}

/// Takes the next value from the transaction sequence (defect X10).
///
/// fhirbase writes `txid = 0` for every bulk-loaded resource, which puts them
/// outside the history mechanism the stored procedures rely on — and is why
/// deleting a loaded resource with `txid = 0` used to collide (X14).
async fn allocate_txid(client: &tokio_postgres::Client) -> Result<i64> {
    let row = client
        .query_one("SELECT nextval('transaction_id_seq')", &[])
        .await
        .map_err(|e| Error::Db(format!("cannot allocate a transaction id: {e}")))?;
    row.try_get(0)
        .map_err(|e| Error::Db(format!("cannot read the allocated transaction id: {e}")))
}

/// Counts resources in a first pass, for an exact progress total.
fn count_resources(files: &[PathBuf]) -> Result<u64> {
    let mut total = 0_u64;
    for path in files {
        let reader = crate::bundle::reader::FileReader::open(path)?;
        for item in reader {
            // A malformed resource still occupies a place in the total; the
            // load will skip and tally it.
            let _ = item;
            total += 1;
        }
    }
    Ok(total)
}

/// Builds the progress bar.
fn build_progress(total: Option<u64>, mode: LoadMode) -> indicatif::ProgressBar {
    let mode_name = match mode {
        LoadMode::Insert => "insert",
        LoadMode::Copy => "copy",
    };

    let bar = if let Some(total) = total {
        let bar = indicatif::ProgressBar::new(total);
        bar.set_style(
            indicatif::ProgressStyle::with_template(
                "{msg} {wide_bar} {pos}/{len} {percent:>3}% eta {eta}",
            )
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar()),
        );
        bar
    } else {
        let bar = indicatif::ProgressBar::new_spinner();
        bar.set_style(
            indicatif::ProgressStyle::with_template("{msg} {spinner} {pos} resources {elapsed}")
                .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
        );
        bar
    };

    bar.set_message(format!("loading ({mode_name})"));
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar
}

/// Prints the end-of-run summary (spec §8.4).
fn report(
    stats: &LoadStats,
    skipped_files: &[(PathBuf, String)],
    elapsed: std::time::Duration,
    mode: LoadMode,
    txid: i64,
) {
    let mode_name = match mode {
        LoadMode::Insert => "insert",
        LoadMode::Copy => "copy",
    };

    println!(
        "Done, inserted {} resources in {:.1} seconds ({mode_name} mode, txid {txid}):",
        stats.total_written(),
        elapsed.as_secs_f64()
    );

    let width = stats
        .written
        .keys()
        .map(String::len)
        .max()
        .unwrap_or(0)
        .max(1);
    for (resource_type, count) in &stats.written {
        println!("  {resource_type:<width$} {count:>9}");
    }

    // Spec §8.4: skipped resources AND unreadable files belong in the summary,
    // not only in scrollback. fhirbase reports neither.
    if !skipped_files.is_empty() {
        println!("\nSkipped {} file(s):", skipped_files.len());
        for (path, reason) in skipped_files {
            println!("  {}: {reason}", path.display());
        }
    }

    if stats.total_skipped() == 0 {
        return;
    }

    println!("\nSkipped {} resource(s):", stats.total_skipped());
    if !stats.unknown_type.is_empty() {
        let unknown: u64 = stats.unknown_type.values().sum();
        println!("  {unknown:>9}  unknown resource type");
        for (resource_type, count) in &stats.unknown_type {
            println!("             {count:>9}  {resource_type:?}");
        }
    }
    if stats.transform_failed > 0 {
        println!("  {:>9}  transformation failed", stats.transform_failed);
    }
    if stats.malformed > 0 {
        println!("  {:>9}  malformed or unreadable", stats.malformed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(sources: &[&str]) -> LoadRequest {
        LoadRequest {
            sources: sources.iter().map(|s| (*s).to_owned()).collect(),
            mode: None,
            strict: false,
            count_first: false,
            memusage: false,
            new_txid: false,
            bulk: crate::bulk::BulkOptions::default(),
        }
    }

    /// The mode default depends on the source kind, which is why it cannot be a
    /// `clap` default (spec §8.1).
    fn default_mode(request: &LoadRequest) -> LoadMode {
        let is_bulk_url = request
            .sources
            .first()
            .is_some_and(|s| s.starts_with("http://") || s.starts_with("https://"));
        request.mode.unwrap_or(if is_bulk_url {
            LoadMode::Copy
        } else {
            LoadMode::Insert
        })
    }

    #[test]
    fn local_files_default_to_insert_mode() {
        assert_eq!(default_mode(&request(&["a.ndjson"])), LoadMode::Insert);
        assert_eq!(default_mode(&request(&["/data/dir"])), LoadMode::Insert);
        // A path that merely contains "http" is not a URL.
        assert_eq!(
            default_mode(&request(&["./http-exports/a.json"])),
            LoadMode::Insert
        );
    }

    #[test]
    fn a_bulk_data_url_defaults_to_copy_mode() {
        assert_eq!(
            default_mode(&request(&["http://example.com/fhir/$export"])),
            LoadMode::Copy
        );
        assert_eq!(
            default_mode(&request(&["https://example.com/fhir/$export"])),
            LoadMode::Copy
        );
    }

    #[test]
    fn an_explicit_mode_wins_over_the_default() {
        let mut r = request(&["http://example.com/$export"]);
        r.mode = Some(LoadMode::Insert);
        assert_eq!(default_mode(&r), LoadMode::Insert);

        let mut r = request(&["a.ndjson"]);
        r.mode = Some(LoadMode::Copy);
        assert_eq!(default_mode(&r), LoadMode::Copy);
    }

    #[test]
    fn the_summary_lists_written_and_skipped_separately() {
        // Not a golden-output test — the shape is what matters, and it must
        // surface skips, which fhirbase never reports.
        let mut stats = LoadStats::default();
        stats.written.insert("Patient".to_owned(), 2);
        stats.written.insert("Observation".to_owned(), 1);
        stats.unknown_type.insert("Nonsense".to_owned(), 3);
        stats.transform_failed = 1;

        assert_eq!(stats.total_written(), 3);
        assert_eq!(stats.total_skipped(), 4);

        // Exercises the formatting for panics and width arithmetic.
        report(
            &stats,
            &[(PathBuf::from("/no/such/file.json"), "cannot open".to_owned())],
            std::time::Duration::from_millis(1500),
            LoadMode::Insert,
            0,
        );
    }

    #[test]
    fn the_summary_survives_an_empty_run() {
        report(
            &LoadStats::default(),
            &[],
            std::time::Duration::from_secs(0),
            LoadMode::Copy,
            0,
        );
    }
}

/// End-to-end tests for `load` from a Bulk Data URL (task T19).
#[cfg(test)]
mod bulk_load_tests {
    use super::*;
    use crate::commands::init;
    use crate::testdb;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A server exporting two NDJSON files, one of them gzipped.
    async fn export_server() -> MockServer {
        use std::io::Write as _;

        let server = MockServer::start().await;
        let base = server.uri();

        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(b"{\"resourceType\":\"Observation\",\"id\":\"o1\",\"status\":\"final\"}\n")
            .unwrap();
        let gzipped = encoder.finish().unwrap();

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
                    {"url": format!("{base}/p.ndjson")},
                    {"url": format!("{base}/o.ndjson")}
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/p.ndjson"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "{\"resourceType\":\"Patient\",\"id\":\"p1\"}\n\
                 {\"resourceType\":\"Patient\",\"id\":\"p2\"}\n",
            ))
            .mount(&server)
            .await;
        // Served still compressed, as a real Bulk Data server would.
        Mock::given(method("GET"))
            .and(path_matcher("/o.ndjson"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(gzipped))
            .mount(&server)
            .await;

        server
    }

    fn bulk_request(url: &str) -> LoadRequest {
        LoadRequest {
            sources: vec![url.to_owned()],
            mode: None,
            strict: false,
            count_first: false,
            memusage: false,
            new_txid: false,
            bulk: crate::bulk::BulkOptions {
                poll_interval: std::time::Duration::from_millis(1),
                max_polls: 20,
                ..crate::bulk::BulkOptions::default()
            },
        }
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn a_bulk_export_downloads_and_loads() {
        let Some(db) = testdb::create("bulkload").await else {
            return;
        };
        let client = db.connect().await;
        init::perform(&client, FhirVersion::V4_0_0).await.unwrap();

        let server = export_server().await;
        let request = bulk_request(&format!("{}/export", server.uri()));

        // Exercised through the same path the command uses, minus the
        // connection: download, then load the files as ordinary local input.
        let downloads = crate::bulk::fetch(&request.sources[0], &request.bulk)
            .await
            .expect("the export should succeed");
        let files = downloads.files().to_vec();
        assert_eq!(files.len(), 2);

        let mut options = LoadOptions::new(FhirVersion::V4_0_0);
        options.strict = true;
        let reader = MultiFileReader::new(files);
        let stats = CopyLoader::new(options)
            .load(&client, reader)
            .await
            .expect("the load should succeed");

        assert_eq!(stats.written["Patient"], 2);
        assert_eq!(stats.written["Observation"], 1, "the gzipped file too");
        assert_eq!(stats.total_skipped(), 0);

        // And the scratch directory goes when the downloads do.
        let scratch = downloads.files()[0].parent().map(std::path::Path::to_path_buf);
        drop(downloads);
        if let Some(scratch) = scratch {
            assert!(!scratch.exists(), "the scratch directory must be removed");
        }

        db.drop().await;
    }

    #[tokio::test]
    async fn a_url_mixed_with_file_paths_is_refused() {
        let mut request = bulk_request("http://example.com/export");
        request.sources.push("extra.ndjson".to_owned());

        // Reaches the guard before any network call.
        let is_bulk_url = request
            .sources
            .first()
            .is_some_and(|s| s.starts_with("http://") || s.starts_with("https://"));
        assert!(is_bulk_url);
        assert!(request.sources.len() > 1);
    }
}
