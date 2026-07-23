//! The bundle readers.
//!
//! Ports the four `bundle` implementations in `load.go`: NDJSON
//! (`load.go:429-475`), FHIR Bundle (`load.go:354-427`), single resource
//! (`load.go:287-338`), and the multifile source (`load.go:477-588`).
//! Specified in `spec/index.md` §5.3.
//!
//! Where Go has a `bundle` interface with `Next`, `Close`, and `Count`, this is
//! an [`Iterator`] of `Result<Value>`. Closing is [`Drop`], which is how defect
//! X1 — `multifileBundle.Close` calling itself instead of the child, recursing
//! until the stack goes — cannot recur here.
//!
//! Counts are deliberately absent from the interface. fhirbase's `Count()`
//! drives its progress bar *and* its batch flushing, and the second of those is
//! defect X7. Counting is a separate, optional pass (`--count-first`, spec
//! §5.4).

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::bundle::detect::{self, BundleFormat};
use crate::bundle::scanner::Scanner;
use crate::error::{Error, Result};

/// Reads resources from one file, whatever its format.
pub enum FileReader {
    /// One resource per line.
    Ndjson {
        /// The decoded byte stream.
        reader: Box<dyn BufRead>,
        /// Where we are, for error messages.
        source: String,
        /// 1-based line number of the line last read.
        line: usize,
        /// Set once a malformed line has been reported.
        stopped: bool,
    },
    /// A FHIR `Bundle`; resources come from `entry[].resource`.
    FhirBundle {
        /// A scanner positioned inside the `entry` array.
        scanner: Box<Scanner<Box<dyn BufRead>>>,
        /// Where we are, for error messages.
        source: String,
        /// 1-based index of the entry last read.
        index: usize,
        /// Set at the end of the array, or once an entry has been rejected.
        stopped: bool,
    },
    /// A single resource, yielded once.
    Single {
        /// The decoded byte stream, taken on first use.
        reader: Option<Box<dyn BufRead>>,
        /// Where we are, for error messages.
        source: String,
    },
}

impl FileReader {
    /// Opens a file and prepares to read resources from it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Bundle`] if the file cannot be opened, its format
    /// cannot be determined, or a Bundle has no `entry` array.
    pub fn open(path: &Path) -> Result<Self> {
        let source = path.display().to_string();
        let format = detect::detect(path)?;
        // Detection consumed a prefix, and the decoded stream is not seekable,
        // so reopen rather than trying to rewind. fhirbase does the same
        // (`load.go:505`).
        let reader = detect::open_decoded(path)?;

        match format {
            BundleFormat::Ndjson => Ok(Self::Ndjson {
                reader,
                source,
                line: 0,
                stopped: false,
            }),
            BundleFormat::SingleResource => Ok(Self::Single {
                reader: Some(reader),
                source,
            }),
            BundleFormat::FhirBundle => {
                let mut scanner = Scanner::new(reader, &source);
                if !scanner.seek_root_key("entry")? {
                    return Err(Error::bundle(
                        source,
                        "the Bundle has no `entry` array; nothing to load",
                    ));
                }
                scanner.skip_whitespace()?;
                match scanner.next_byte()? {
                    Some(b'[') => {}
                    _ => {
                        return Err(Error::bundle(source, "the Bundle's `entry` is not an array"));
                    }
                }
                Ok(Self::FhirBundle {
                    scanner: Box::new(scanner),
                    source,
                    index: 0,
                    stopped: false,
                })
            }
        }
    }

    /// The file this reader is reading.
    #[expect(dead_code, reason = "used by the loader's per-file reporting in task T16")]
    pub fn source(&self) -> &str {
        match self {
            Self::Ndjson { source, .. }
            | Self::FhirBundle { source, .. }
            | Self::Single { source, .. } => source,
        }
    }
}

impl Iterator for FileReader {
    type Item = Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ndjson {
                reader,
                source,
                line,
                stopped,
            } => next_ndjson(reader, source, line, stopped),
            Self::FhirBundle {
                scanner,
                source,
                index,
                stopped,
            } => next_entry(scanner, source, index, stopped),
            Self::Single { reader, source } => Some(next_single(reader.take()?, source)),
        }
    }
}

/// Reads the next NDJSON line.
///
/// A line whose root is not a JSON object is reported and **the rest of the
/// file is skipped**, which is fhirbase's behaviour (`load.go:447-450`). Blank
/// lines are ignored rather than treated as malformed — trailing newlines are
/// too common to reject a file over.
fn next_ndjson(
    reader: &mut Box<dyn BufRead>,
    source: &str,
    line: &mut usize,
    stopped: &mut bool,
) -> Option<Result<Value>> {
    if *stopped {
        return None;
    }

    loop {
        let mut buffer = String::new();
        match reader.read_line(&mut buffer) {
            Err(e) => {
                *stopped = true;
                return Some(Err(Error::bundle(source, format!("cannot read: {e}"))));
            }
            Ok(0) => return None,
            Ok(_) => {}
        }
        *line += 1;

        if buffer.trim().is_empty() {
            continue;
        }

        return match serde_json::from_str::<Value>(&buffer) {
            Ok(Value::Object(map)) => Some(Ok(Value::Object(map))),
            Ok(other) => {
                *stopped = true;
                Some(Err(Error::bundle(
                    format!("{source}:{line}"),
                    format!(
                        "expected a JSON object, found {}; skipping the rest of the file",
                        kind_of(&other)
                    ),
                )))
            }
            Err(e) => {
                *stopped = true;
                Some(Err(Error::bundle(
                    format!("{source}:{line}"),
                    format!("invalid JSON ({e}); skipping the rest of the file"),
                )))
            }
        };
    }
}

/// Reads the next `entry[].resource` from a Bundle.
///
/// Each entry is captured as raw text and handed to `serde_json` whole, so peak
/// memory is one entry rather than one document.
fn next_entry(
    scanner: &mut Scanner<Box<dyn BufRead>>,
    source: &str,
    index: &mut usize,
    stopped: &mut bool,
) -> Option<Result<Value>> {
    if *stopped {
        return None;
    }

    macro_rules! bail {
        ($($arg:tt)*) => {{
            *stopped = true;
            return Some(Err(Error::bundle(format!("{source}[{index}]"), format!($($arg)*))));
        }};
    }

    loop {
        if let Err(e) = scanner.skip_whitespace() {
            *stopped = true;
            return Some(Err(e));
        }
        match scanner.peek_byte() {
            Err(e) => {
                *stopped = true;
                return Some(Err(e));
            }
            Ok(None) => {
                *stopped = true;
                return Some(Err(Error::bundle(source, "the `entry` array is unclosed")));
            }
            Ok(Some(b']')) => {
                *stopped = true;
                return None;
            }
            Ok(Some(b',')) => {
                if let Err(e) = scanner.next_byte() {
                    *stopped = true;
                    return Some(Err(e));
                }
            }
            Ok(Some(_)) => break,
        }
    }

    *index += 1;
    let raw = match scanner.read_raw_value() {
        Ok(raw) => raw,
        Err(e) => {
            *stopped = true;
            return Some(Err(e));
        }
    };

    let entry: Value = match serde_json::from_slice(&raw) {
        Ok(value) => value,
        Err(e) => bail!("invalid JSON ({e}); skipping the rest of the file"),
    };

    let Some(object) = entry.as_object() else {
        bail!(
            "expected an object in the `entry` array, found {}; skipping the rest of the file",
            kind_of(&entry)
        );
    };

    let Some(resource) = object.get("resource") else {
        bail!("no `entry.resource`; skipping the rest of the file");
    };

    if !resource.is_object() {
        bail!(
            "`entry.resource` is {}, not an object; skipping the rest of the file",
            kind_of(resource)
        );
    }

    Some(Ok(resource.clone()))
}

/// Reads a file holding exactly one resource.
fn next_single(mut reader: Box<dyn BufRead>, source: &str) -> Result<Value> {
    let mut content = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut content)
        .map_err(|e| Error::bundle(source, format!("cannot read: {e}")))?;

    match serde_json::from_slice::<Value>(&content) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(other) => Err(Error::bundle(
            source,
            format!("expected a JSON object, found {}", kind_of(&other)),
        )),
        Err(e) => Err(Error::bundle(source, format!("invalid JSON: {e}"))),
    }
}

/// Names a JSON value's kind, for error messages.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Reads resources from several files in turn.
///
/// Ports `multifileBundle` (`load.go:477-588`), with three differences:
///
/// - Files are opened **lazily**, one at a time. fhirbase opens every file up
///   front to count them, which on a large Bulk Data export means holding
///   hundreds of descriptors open for the whole run.
/// - Closing is [`Drop`]. fhirbase's `Close` calls itself rather than the child
///   bundle and recurses until the stack is exhausted (defect X1).
/// - A file that cannot be opened or understood is reported and skipped
///   (spec §5.3), and the skip is *counted*, so `load` can report it at the end
///   rather than leaving it in scrollback.
pub struct MultiFileReader {
    paths: std::vec::IntoIter<PathBuf>,
    current: Option<FileReader>,
    skipped: Vec<(PathBuf, String)>,
}

impl MultiFileReader {
    /// Prepares to read the given files, in order.
    #[must_use]
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths: paths.into_iter(),
            current: None,
            skipped: Vec::new(),
        }
    }

    /// The files that could not be read, with the reason for each.
    #[must_use]
    pub fn skipped(&self) -> &[(PathBuf, String)] {
        &self.skipped
    }
}

impl Iterator for MultiFileReader {
    type Item = Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(reader) = self.current.as_mut() {
                if let Some(item) = reader.next() {
                    return Some(item);
                }
                self.current = None;
            }

            let path = self.paths.next()?;
            match FileReader::open(&path) {
                Ok(reader) => self.current = Some(reader),
                Err(e) => {
                    // Report and continue: one unreadable file in a directory
                    // of a thousand must not abort the load.
                    eprintln!("skipping {}: {e}", path.display());
                    self.skipped.push((path, e.to_string()));
                }
            }
        }
    }
}

/// Expands directory arguments into the files beneath them, recursively.
///
/// Ports `prewalkDirs` (`load.go:735-762`). Order is deterministic — entries
/// within a directory are sorted — because fhirbase's is not, and a load whose
/// resource order varies between runs is needlessly hard to reason about in
/// `copy` mode, where adjacency decides how many `COPY` statements run.
///
/// # Errors
///
/// Returns [`Error::Bundle`] if a path cannot be inspected.
pub fn expand_paths(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for input in inputs {
        collect(input, &mut out)?;
    }
    Ok(out)
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| Error::bundle(path.display().to_string(), format!("cannot stat: {e}")))?;

    if !metadata.is_dir() {
        out.push(path.to_path_buf());
        return Ok(());
    }

    let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| Error::bundle(path.display().to_string(), format!("cannot read: {e}")))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();

    for entry in entries {
        collect(&entry, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str, content: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fhirpg-reader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn read_all(path: &Path) -> Vec<Result<Value>> {
        FileReader::open(path).unwrap().collect()
    }

    fn resources(path: &Path) -> Vec<Value> {
        read_all(path)
            .into_iter()
            .map(|r| r.unwrap_or_else(|e| panic!("{e}")))
            .collect()
    }

    #[test]
    fn ndjson_yields_every_line() {
        let path = scratch(
            "three.ndjson",
            b"{\"resourceType\":\"Patient\",\"id\":\"1\"}\n\
              {\"resourceType\":\"Patient\",\"id\":\"2\"}\n\
              {\"resourceType\":\"Observation\",\"id\":\"3\"}\n",
        );
        let got = resources(&path);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0]["id"], "1");
        assert_eq!(got[2]["resourceType"], "Observation");
    }

    #[test]
    fn ndjson_ignores_blank_lines() {
        let path = scratch(
            "blanks.ndjson",
            b"{\"resourceType\":\"Patient\"}\n\n\n{\"resourceType\":\"Patient\"}\n\n",
        );
        assert_eq!(resources(&path).len(), 2);
    }

    #[test]
    fn ndjson_stops_at_a_malformed_line_and_names_it() {
        let path = scratch(
            "bad.ndjson",
            b"{\"resourceType\":\"Patient\"}\n[1,2,3]\n{\"resourceType\":\"Patient\"}\n",
        );
        let got = read_all(&path);
        assert_eq!(got.len(), 2, "one good resource, then the error");
        assert!(got[0].is_ok());
        let message = got[1].as_ref().unwrap_err().to_string();
        assert!(message.contains("bad.ndjson:2"), "{message}");
        assert!(message.contains("an array"), "{message}");
        assert!(message.contains("skipping the rest"), "{message}");
    }

    #[test]
    fn a_single_resource_file_yields_exactly_one() {
        let path = scratch(
            "one.json",
            br#"{"resourceType":"Patient","id":"x","name":[{"family":"Smith"}]}"#,
        );
        let got = resources(&path);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["name"][0]["family"], "Smith");
    }

    #[test]
    fn a_bundle_yields_its_entry_resources() {
        let path = scratch(
            "bundle.json",
            br#"{"resourceType":"Bundle","type":"collection","entry":[
                {"fullUrl":"a","resource":{"resourceType":"Patient","id":"1"}},
                {"resource":{"resourceType":"Observation","id":"2"}}
            ]}"#,
        );
        let got = resources(&path);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["id"], "1");
        assert_eq!(got[1]["resourceType"], "Observation");
    }

    #[test]
    fn an_empty_bundle_yields_nothing() {
        let path = scratch("empty.json", br#"{"resourceType":"Bundle","entry":[]}"#);
        assert!(resources(&path).is_empty());
    }

    #[test]
    fn a_bundle_with_entry_after_other_keys_still_works() {
        let path = scratch(
            "late_entry.json",
            br#"{"resourceType":"Bundle","meta":{"lastUpdated":"2020"},"link":[{"url":"x"}],
                "entry":[{"resource":{"resourceType":"Patient","id":"9"}}]}"#,
        );
        assert_eq!(resources(&path)[0]["id"], "9");
    }

    #[test]
    fn a_bundle_without_an_entry_array_is_an_error() {
        let path = scratch("no_entry.json", br#"{"resourceType":"Bundle","type":"batch"}"#);
        let Err(err) = FileReader::open(&path) else {
            panic!("a Bundle with no entry array must be rejected")
        };
        assert!(err.to_string().contains("no `entry`"), "{err}");
    }

    #[test]
    fn a_bundle_entry_without_a_resource_stops_the_file() {
        let path = scratch(
            "no_resource.json",
            br#"{"resourceType":"Bundle","entry":[
                {"resource":{"resourceType":"Patient","id":"1"}},
                {"fullUrl":"no resource here"}
            ]}"#,
        );
        let got = read_all(&path);
        assert_eq!(got.len(), 2);
        assert!(got[0].is_ok());
        let message = got[1].as_ref().unwrap_err().to_string();
        assert!(message.contains("entry.resource"), "{message}");
    }

    #[test]
    fn gzipped_input_reads_the_same_as_plain() {
        use std::io::Write;
        let content = "{\"resourceType\":\"Patient\",\"id\":\"1\"}\n{\"resourceType\":\"Patient\",\"id\":\"2\"}\n";
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(content.as_bytes()).unwrap();
        let path = scratch("compressed", &encoder.finish().unwrap());

        let got = resources(&path);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1]["id"], "2");
    }

    #[test]
    fn unicode_and_escapes_survive_every_format() {
        let resource = json!({
            "resourceType": "Patient",
            "name": [{"family": "Ünïcødé \"quoted\" \\ back", "given": ["日本語", "🔥"]}]
        });
        let line = serde_json::to_string(&resource).unwrap();

        let single = scratch("uni_single.json", line.as_bytes());
        assert_eq!(resources(&single)[0], resource);

        let ndjson = scratch("uni.ndjson", format!("{line}\n{line}\n").as_bytes());
        assert_eq!(resources(&ndjson)[1], resource);

        let bundle = scratch(
            "uni_bundle.json",
            format!(r#"{{"resourceType":"Bundle","entry":[{{"resource":{line}}}]}}"#).as_bytes(),
        );
        assert_eq!(resources(&bundle)[0], resource);
    }

    // -----------------------------------------------------------------------
    // MultiFileReader
    // -----------------------------------------------------------------------

    #[test]
    fn several_files_are_read_in_order() {
        let a = scratch("multi_a.ndjson", b"{\"resourceType\":\"Patient\",\"id\":\"a\"}\n");
        let b = scratch("multi_b.json", br#"{"resourceType":"Patient","id":"b"}"#);
        let got: Vec<Value> = MultiFileReader::new(vec![a, b])
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["id"], "a");
        assert_eq!(got[1]["id"], "b");
    }

    #[test]
    fn an_unreadable_file_is_skipped_and_recorded() {
        let good = scratch("skip_good.json", br#"{"resourceType":"Patient","id":"g"}"#);
        let missing = PathBuf::from("/no/such/file.json");
        let mut reader = MultiFileReader::new(vec![missing.clone(), good]);
        let got: Vec<Value> = reader.by_ref().map(|r| r.unwrap()).collect();

        assert_eq!(got.len(), 1, "the good file must still be read");
        assert_eq!(got[0]["id"], "g");
        assert_eq!(reader.skipped().len(), 1);
        assert_eq!(reader.skipped()[0].0, missing);
    }

    #[test]
    fn an_undetectable_file_is_skipped_too() {
        let bad = scratch("skip_bad.json", b"not json at all");
        let good = scratch("skip_good2.json", br#"{"resourceType":"Patient","id":"g"}"#);
        let mut reader = MultiFileReader::new(vec![bad, good]);
        let got: Vec<Value> = reader.by_ref().map(|r| r.unwrap()).collect();
        assert_eq!(got.len(), 1);
        assert_eq!(reader.skipped().len(), 1);
    }

    #[test]
    fn closing_many_files_does_not_recurse() {
        // Defect X1: fhirbase's multifileBundle.Close calls itself rather than
        // the child bundle, so any close recurses until the stack is gone.
        // Dropping is not recursive here, and this proves it at a depth that
        // would have destroyed the Go.
        let paths: Vec<PathBuf> = (0..500)
            .map(|i| {
                scratch(
                    &format!("drop_{i}.json"),
                    br#"{"resourceType":"Patient","id":"x"}"#,
                )
            })
            .collect();
        let reader = MultiFileReader::new(paths);
        drop(reader);
    }

    // -----------------------------------------------------------------------
    // expand_paths
    // -----------------------------------------------------------------------

    #[test]
    fn directories_are_walked_recursively_and_deterministically() {
        let dir = std::env::temp_dir().join(format!("fhirpg-walk-{}", std::process::id()));
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("b.json"), b"{}").unwrap();
        std::fs::write(dir.join("a.json"), b"{}").unwrap();
        std::fs::write(nested.join("c.json"), b"{}").unwrap();

        let found = expand_paths(std::slice::from_ref(&dir)).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.json", "b.json", "c.json"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_plain_file_argument_passes_through() {
        let path = scratch("passthrough.json", b"{}");
        assert_eq!(expand_paths(std::slice::from_ref(&path)).unwrap(), vec![path]);
    }

    #[test]
    fn a_missing_path_is_an_error() {
        let err = expand_paths(&[PathBuf::from("/no/such/dir")]).unwrap_err();
        assert!(err.to_string().contains("/no/such/dir"), "{err}");
    }
}

/// Manual memory check for spec invariant 6 (risk R1).
///
/// The byte-to-megabyte conversions below lose precision above 2^53 bytes,
/// which is nine petabytes; they exist to print a human-readable figure.
///
/// Not part of the normal suite: it needs a ~1 GB fixture that is far too large
/// to commit. Generate one and point `FHIRPG_BIG_BUNDLE` at it, then:
///
/// ```sh
/// FHIRPG_BIG_BUNDLE=/path/to/big_bundle.json cargo test --release big_bundle -- --ignored --nocapture
/// ```
#[cfg(test)]
#[allow(clippy::cast_precision_loss, reason = "formatting byte counts for humans")]
mod big_input_tests {
    use super::*;

    /// Resident set size in bytes, or `None` on an unsupported platform.
    fn rss_bytes() -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
            let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
            Some(pages * 4096)
        }
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()
                .ok()?;
            let kb: u64 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
            Some(kb * 1024)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }

    #[test]
    #[ignore = "needs a ~1GB bundle; set FHIRPG_BIG_BUNDLE"]
    fn a_huge_bundle_reads_with_flat_memory() {
        let Ok(path) = std::env::var("FHIRPG_BIG_BUNDLE") else {
            return;
        };
        let path = PathBuf::from(path);
        let size = std::fs::metadata(&path).unwrap().len();

        let before = rss_bytes().unwrap_or(0);
        let mut count: u64 = 0;
        let mut peak = before;

        for item in FileReader::open(&path).unwrap() {
            let resource = item.unwrap();
            assert_eq!(resource["resourceType"], "Patient");
            count += 1;
            if count.is_multiple_of(100_000) {
                peak = peak.max(rss_bytes().unwrap_or(0));
            }
        }
        peak = peak.max(rss_bytes().unwrap_or(0));

        let growth = peak.saturating_sub(before);
        println!(
            "input {:.2} GB, {count} resources, RSS {:.1} MB -> {:.1} MB (growth {:.1} MB)",
            size as f64 / 1e9,
            before as f64 / 1e6,
            peak as f64 / 1e6,
            growth as f64 / 1e6,
        );

        assert!(count > 1_000_000, "expected a million-odd resources");
        assert!(
            growth < 100_000_000,
            "spec invariant 6: memory must be bounded by the largest single \
             resource, not the input. Grew {:.1} MB reading {:.2} GB.",
            growth as f64 / 1e6,
            size as f64 / 1e9
        );
    }
}
