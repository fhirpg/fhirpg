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

    if is_bulk_url {
        return Err(Error::NotImplemented {
            command: "load from a Bulk Data URL",
            task: "T19",
        });
    }

    let inputs: Vec<PathBuf> = request.sources.iter().map(PathBuf::from).collect();
    let files = expand_paths(&inputs)?;
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
