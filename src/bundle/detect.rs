//! Detecting a file's compression and format.
//!
//! Ports `openFile`, `isCompleteJSONObject`, `guessBundleType`, and
//! `guessJSONBundleType` (`load.go:36-194`). Specified in `spec/index.md` §5.
//!
//! Detection is by **content, not filename**: extensions may be omitted, and a
//! single `load` invocation may mix gzipped and plain files of all three
//! formats. That is fhirbase's behaviour and it is genuinely useful — Bulk Data
//! exports arrive with unhelpful names.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use crate::bundle::scanner::Scanner;
use crate::error::{Error, Result};

/// The three input formats (spec §5.2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BundleFormat {
    /// Newline-delimited JSON: one resource per line.
    Ndjson,
    /// A FHIR `Bundle` resource; resources come from `entry[].resource`.
    FhirBundle,
    /// A single FHIR resource.
    SingleResource,
}

/// The first two bytes of a gzip stream.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Opens a file, transparently decoding gzip.
///
/// fhirbase tries `gzip.NewReader` and seeks back to 0 when it fails
/// (`load.go:36-57`). Sniffing the two magic bytes reaches the same conclusion
/// without needing the stream to be seekable, which matters because the
/// decoded reader is not.
///
/// # Errors
///
/// Returns [`Error::Bundle`] if the file cannot be opened or read.
pub fn open_decoded(path: &Path) -> Result<Box<dyn BufRead>> {
    let name = path.display().to_string();
    let file = std::fs::File::open(path)
        .map_err(|e| Error::bundle(name.clone(), format!("cannot open: {e}")))?;
    let mut reader = BufReader::new(file);

    let magic = reader
        .fill_buf()
        .map_err(|e| Error::bundle(name, format!("cannot read: {e}")))?;

    if magic.starts_with(&GZIP_MAGIC) {
        Ok(Box::new(BufReader::new(flate2::read::GzDecoder::new(reader))))
    } else {
        Ok(Box::new(reader))
    }
}

/// Whether a string's braces balance, ignoring braces inside string literals.
///
/// Ports `isCompleteJSONObject` (`load.go:113-141`) exactly, including two
/// behaviours worth naming because they look like oversights and are load
/// bearing:
///
/// - A string with **no braces at all** balances, so `""` is "complete". That
///   is what makes a file whose single line ends in a newline classify as
///   NDJSON: the absent second line is the empty string.
/// - Only `"` and `\` are examined inside a string literal, so braces within
///   strings never affect the count — which is the entire point.
#[must_use]
pub fn is_complete_json_object(s: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for c in s.chars() {
        if escaped {
            escaped = false;
        } else if in_string {
            match c {
                '"' => in_string = false,
                '\\' => escaped = true,
                _ => {}
            }
        } else {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                '"' => in_string = true,
                _ => {}
            }
        }
    }

    depth == 0
}

/// Classifies a file (spec §5.2).
///
/// # Errors
///
/// Returns [`Error::Bundle`] if the file cannot be read or its root is not a
/// JSON object.
pub fn detect(path: &Path) -> Result<BundleFormat> {
    let reader = open_decoded(path)?;
    guess_format(reader, &path.display().to_string())
}

/// How much of a file detection may buffer.
///
/// Detection needs the first two lines, and reading them with `read_line` would
/// buffer the whole file when it contains no newline — which is exactly what a
/// compact FHIR Bundle is. That is defect X16, inherited from fhirbase's
/// `bufio.Reader.ReadString('\n')`, and measured: a 1 GB single-line Bundle
/// grew RSS by 1,003 MB before this cap existed.
///
/// 1 MiB is far beyond any plausible NDJSON line while still being a constant.
const DETECTION_PREFIX_LIMIT: usize = 1024 * 1024;

/// Classifies an already-opened, already-decoded stream.
///
/// Reads at most [`DETECTION_PREFIX_LIMIT`] bytes, whatever the file's size
/// (spec invariant 6).
///
/// # Errors
///
/// Returns [`Error::Bundle`] if the stream cannot be read or its root is not a
/// JSON object.
pub fn guess_format(mut reader: Box<dyn BufRead>, source: &str) -> Result<BundleFormat> {
    let mut prefix = Vec::new();
    {
        let mut limited = reader.by_ref().take(DETECTION_PREFIX_LIMIT as u64);
        limited
            .read_to_end(&mut prefix)
            .map_err(|e| Error::bundle(source, format!("cannot read: {e}")))?;
    }

    if prefix.is_empty() {
        return Err(Error::bundle(source, "the file is empty"));
    }

    // Split the prefix into its first two lines, if it has them. A file whose
    // first line exceeds the cap has no second line worth comparing, so it
    // falls through to the JSON inspection below — which is the right answer
    // for the case that motivates the cap, a single-line Bundle.
    if let Some(first_end) = prefix.iter().position(|&b| b == b'\n') {
        let first = &prefix[..=first_end];
        let rest = &prefix[first_end + 1..];
        let second_end = rest.iter().position(|&b| b == b'\n');
        let second = match second_end {
            Some(end) => &rest[..=end],
            None => rest,
        };

        // Two complete objects on two lines: NDJSON. Note `second` may be empty
        // — see `is_complete_json_object` — so a lone newline-terminated
        // resource is NDJSON, which is both fhirbase's behaviour and the right
        // answer, since a one-line NDJSON file is still NDJSON.
        let (Ok(first), Ok(second)) = (std::str::from_utf8(first), std::str::from_utf8(second))
        else {
            return Err(Error::bundle(source, "the file is not valid UTF-8"));
        };

        if is_complete_json_object(first) && is_complete_json_object(second) {
            return Ok(BundleFormat::Ndjson);
        }
    }

    classify_json(std::io::Cursor::new(prefix).chain(reader), source)
}

/// Decides between a FHIR Bundle and a single resource by finding
/// `resourceType`.
///
/// Ports `guessJSONBundleType` (`load.go:143-167`), including its outcomes:
/// `"Bundle"` is a bundle, any other non-empty value is a single resource, an
/// empty or non-string value is unknown, and **an object with no `resourceType`
/// at all is treated as a bundle**.
fn classify_json<R: Read>(reader: R, source: &str) -> Result<BundleFormat> {
    match scan_root_resource_type(reader, source)? {
        None => Ok(BundleFormat::FhirBundle),
        Some(rt) if rt == "Bundle" => Ok(BundleFormat::FhirBundle),
        Some(rt) if !rt.is_empty() => Ok(BundleFormat::SingleResource),
        Some(_) => Err(Error::bundle(
            source,
            "the root object's `resourceType` is empty or not a string",
        )),
    }
}

/// Streams the root object's keys and returns `resourceType`'s value.
///
/// Reads only as far as it must: the scan stops at `resourceType`, or at the
/// end of the root object if there is none. It never materializes the document,
/// which is what lets a multi-gigabyte Bundle be classified from its first few
/// bytes (spec invariant 6).
///
/// Returns `None` when the root object has no `resourceType` key.
///
/// # Errors
///
/// Returns [`Error::Bundle`] if the stream cannot be read, does not begin with
/// a JSON object, or is malformed before the answer is known.
fn scan_root_resource_type<R: Read>(reader: R, source: &str) -> Result<Option<String>> {
    let mut scanner = Scanner::new(reader, source);

    scanner.skip_whitespace()?;
    match scanner.next_byte()? {
        Some(b'{') => {}
        Some(other) => {
            return Err(Error::bundle(
                scanner.source,
                format!(
                    "expected a JSON object at the root, found {:?}",
                    char::from(other)
                ),
            ));
        }
        None => return Err(Error::bundle(scanner.source, "the file is empty")),
    }

    loop {
        scanner.skip_whitespace()?;
        match scanner.peek_byte()? {
            Some(b'}') => return Ok(None),
            Some(b',') => {
                scanner.next_byte()?;
                continue;
            }
            Some(b'"') => {}
            Some(other) => {
                return Err(Error::bundle(
                    scanner.source,
                    format!("expected a key, found {:?}", char::from(other)),
                ));
            }
            None => return Err(Error::bundle(scanner.source, "the root object is unclosed")),
        }

        let key = scanner.read_string()?;
        scanner.skip_whitespace()?;
        match scanner.next_byte()? {
            Some(b':') => {}
            _ => {
                return Err(Error::bundle(
                    scanner.source,
                    format!("expected ':' after the key {key:?}"),
                ));
            }
        }
        scanner.skip_whitespace()?;

        if key == "resourceType" {
            // A non-string value is `""` to fhirbase, and `""` means unknown.
            return Ok(Some(match scanner.peek_byte()? {
                Some(b'"') => scanner.read_string()?,
                _ => String::new(),
            }));
        }

        scanner.skip_value()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guess(input: &str) -> Result<BundleFormat> {
        guess_format(Box::new(std::io::Cursor::new(input.to_owned())), "<test>")
    }

    // -----------------------------------------------------------------------
    // The five cases from fhirbase's load_test.go, ported verbatim.
    // -----------------------------------------------------------------------

    #[test]
    fn go_case_two_objects_on_two_lines_is_ndjson() {
        assert_eq!(
            guess("{\"foo\": \"bar\"}\n{\"foo\": \"bar\"}").unwrap(),
            BundleFormat::Ndjson
        );
    }

    #[test]
    fn go_case_braces_inside_strings_do_not_confuse_it() {
        assert_eq!(
            guess("{\"foo\": \"{{\\\"}bar\"}\n{\"foo\": \"bar\"}").unwrap(),
            BundleFormat::Ndjson
        );
    }

    #[test]
    fn go_case_multiline_bundle() {
        assert_eq!(
            guess("{\"foo\": \"{{\\\"}bar\",\n\n\"resourceType\": \"Bundle\"}").unwrap(),
            BundleFormat::FhirBundle
        );
    }

    #[test]
    fn go_case_multiline_single_resource() {
        assert_eq!(
            guess("{\"foo\": \"bar\", \n\n\n\n\n \"resourceType\": \"Observation\"}").unwrap(),
            BundleFormat::SingleResource
        );
    }

    #[test]
    fn go_case_one_line_single_resource() {
        assert_eq!(
            guess("{\"foo\": \"{{\\\"}bar\", \"resourceType\": \"Patient\"}").unwrap(),
            BundleFormat::SingleResource
        );
    }

    // -----------------------------------------------------------------------
    // is_complete_json_object
    // -----------------------------------------------------------------------

    #[test]
    fn brace_balancing_ignores_string_contents() {
        assert!(is_complete_json_object("{}"));
        assert!(is_complete_json_object(r#"{"a":"}}}}"}"#));
        assert!(is_complete_json_object(r#"{"a":"\""}"#));
        assert!(!is_complete_json_object("{"));
        assert!(!is_complete_json_object(r#"{"a":{"#));
        // An empty string balances — load-bearing, see the doc comment.
        assert!(is_complete_json_object(""));
        assert!(is_complete_json_object("no braces here"));
    }

    #[test]
    fn a_trailing_backslash_does_not_run_off_the_end() {
        // Truncated mid-escape. The brace never closes, so this is incomplete —
        // confirmed against the Go, which returns false here too. The point of
        // the test is that neither implementation reads past the end.
        assert!(!is_complete_json_object(r#"{"a":"\"#));
        assert!(!is_complete_json_object(r"{\"));
    }

    // -----------------------------------------------------------------------
    // Edges beyond the Go cases.
    // -----------------------------------------------------------------------

    #[test]
    fn a_newline_terminated_single_line_is_ndjson() {
        // The absent second line is "", which balances, so this is NDJSON.
        // Harmless: a one-line NDJSON file is still NDJSON.
        assert_eq!(
            guess("{\"resourceType\":\"Patient\"}\n").unwrap(),
            BundleFormat::Ndjson
        );
    }

    #[test]
    fn an_object_without_a_resource_type_is_a_bundle() {
        assert_eq!(guess(r#"{"foo": "bar", "baz": 1}"#).unwrap(), BundleFormat::FhirBundle);
    }

    #[test]
    fn resource_type_after_deeply_nested_keys_is_still_found() {
        let input = r#"{"a": {"b": [1, 2, {"c": "}{"}]}, "d": [[["x"]]], "resourceType": "Patient"}"#;
        assert_eq!(guess(input).unwrap(), BundleFormat::SingleResource);
    }

    #[test]
    fn scalars_of_every_kind_are_skipped() {
        let input =
            r#"{"n": -1.5e10, "t": true, "f": false, "z": null, "resourceType": "Observation"}"#;
        assert_eq!(guess(input).unwrap(), BundleFormat::SingleResource);
    }

    #[test]
    fn escapes_in_keys_and_values_survive() {
        let input = r#"{"a\"b": "c\\d", "resourceType": "Patient"}"#;
        assert_eq!(guess(input).unwrap(), BundleFormat::SingleResource);
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        // \r stays on the first line, which does not affect brace balance.
        assert_eq!(
            guess("{\"foo\": \"bar\"}\r\n{\"foo\": \"bar\"}\r\n").unwrap(),
            BundleFormat::Ndjson
        );
    }

    #[test]
    fn an_empty_file_is_an_error() {
        let err = guess("").unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn a_non_object_root_is_an_error() {
        for input in ["[1,2,3]", "\"just a string\"", "42"] {
            let err = guess(input).unwrap_err();
            assert!(
                err.to_string().contains("expected a JSON object"),
                "{input}: {err}"
            );
        }
    }

    #[test]
    fn an_empty_resource_type_is_an_error() {
        let err = guess(r#"{"resourceType": ""}"#).unwrap_err();
        assert!(err.to_string().contains("resourceType"), "{err}");
    }

    #[test]
    fn a_non_string_resource_type_is_an_error() {
        let err = guess(r#"{"resourceType": 42}"#).unwrap_err();
        assert!(err.to_string().contains("resourceType"), "{err}");
    }

    #[test]
    fn a_bundle_is_classified_without_reading_its_entries() {
        // Spec invariant 6. `resourceType` comes first, so the scan must stop
        // there — the trailing megabyte is never touched.
        let big = "x".repeat(1_000_000);
        let input = format!(r#"{{"resourceType": "Bundle", "entry": ["{big}"]}}"#);
        assert_eq!(guess(&input).unwrap(), BundleFormat::FhirBundle);
    }

    // -----------------------------------------------------------------------
    // gzip
    // -----------------------------------------------------------------------

    fn scratch(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("fhirpg-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn gzipped(content: &str) -> Vec<u8> {
        use std::io::Write;
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(content.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn gzip_is_detected_by_content_not_by_name() {
        let content = "{\"resourceType\":\"Patient\"}\n{\"resourceType\":\"Patient\"}";
        // No .gz extension anywhere.
        let path = scratch("detect_gzipped.json", &gzipped(content));
        assert_eq!(detect(&path).unwrap(), BundleFormat::Ndjson);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_plain_file_is_read_as_is() {
        let path = scratch(
            "detect_plain.bin",
            b"{\"foo\": \"bar\", \"resourceType\": \"Patient\"}",
        );
        assert_eq!(detect(&path).unwrap(), BundleFormat::SingleResource);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_file_names_the_path() {
        let err = detect(std::path::Path::new("/no/such/bundle.ndjson")).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("/no/such/bundle.ndjson"), "{message}");
        assert!(message.contains("cannot open"), "{message}");
    }
}
