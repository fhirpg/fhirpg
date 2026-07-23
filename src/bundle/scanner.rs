//! A minimal byte-level JSON scanner.
//!
//! Just enough to walk a JSON document's structure without materializing it,
//! which is what lets a multi-gigabyte FHIR Bundle be read with memory bounded
//! by its largest single entry (spec invariant 6).
//!
//! `serde_json` has no pull parser, so there is no way to stream *into* an
//! array with it. This scanner navigates to the array and hands each element to
//! `serde_json` as a complete, bounded slice — so the fiddly parsing stays with
//! the real parser, and this only has to find boundaries.
//!
//! Two consumers: [`crate::bundle::detect`] finds `resourceType` in the root
//! object, and [`crate::bundle::reader`] walks a Bundle's `entry[]`.

use std::io::{BufReader, Read};

use crate::error::{Error, Result};

/// A minimal byte-level JSON scanner, enough to walk one object's keys.
///
/// Buffered internally: the scanner reads a byte at a time, which would be a
/// syscall per byte over a bare `File`.
pub struct Scanner<R: Read> {
    bytes: std::io::Bytes<BufReader<R>>,
    peeked: Option<u8>,
    pub source: String,
}

impl<R: Read> Scanner<R> {
    pub fn new(reader: R, source: &str) -> Self {
        Self {
            bytes: BufReader::new(reader).bytes(),
            peeked: None,
            source: source.to_owned(),
        }
    }

    pub fn next_byte(&mut self) -> Result<Option<u8>> {
        if let Some(b) = self.peeked.take() {
            return Ok(Some(b));
        }
        match self.bytes.next() {
            None => Ok(None),
            Some(Ok(b)) => Ok(Some(b)),
            Some(Err(e)) => Err(Error::bundle(&self.source, format!("cannot read: {e}"))),
        }
    }

    pub fn peek_byte(&mut self) -> Result<Option<u8>> {
        if self.peeked.is_none() {
            self.peeked = self.next_byte()?;
        }
        Ok(self.peeked)
    }

    pub fn skip_whitespace(&mut self) -> Result<()> {
        while let Some(b) = self.peek_byte()? {
            if b.is_ascii_whitespace() {
                self.next_byte()?;
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Reads a JSON string, assuming the opening quote is next.
    pub fn read_string(&mut self) -> Result<String> {
        match self.next_byte()? {
            Some(b'"') => {}
            _ => return Err(Error::bundle(&self.source, "expected a string")),
        }

        let mut raw = Vec::new();
        loop {
            match self.next_byte()? {
                None => return Err(Error::bundle(&self.source, "unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => {
                    let escape = self
                        .next_byte()?
                        .ok_or_else(|| Error::bundle(&self.source, "unterminated escape"))?;
                    match escape {
                        b'"' => raw.push(b'"'),
                        b'\\' => raw.push(b'\\'),
                        b'/' => raw.push(b'/'),
                        b'b' => raw.push(0x08),
                        b'f' => raw.push(0x0c),
                        b'n' => raw.push(b'\n'),
                        b'r' => raw.push(b'\r'),
                        b't' => raw.push(b'\t'),
                        b'u' => {
                            // Only the key comparison and the resourceType value
                            // matter here, and both are ASCII in practice. Keep
                            // the four hex digits verbatim rather than decoding
                            // surrogate pairs: it cannot match "resourceType"
                            // or a resource type name either way.
                            for _ in 0..4 {
                                match self.next_byte()? {
                                    Some(b) => raw.push(b),
                                    None => {
                                        return Err(Error::bundle(
                                            &self.source,
                                            "unterminated \\u escape",
                                        ));
                                    }
                                }
                            }
                        }
                        other => raw.push(other),
                    }
                }
                Some(b) => raw.push(b),
            }
        }

        String::from_utf8(raw)
            .map_err(|e| Error::bundle(&self.source, format!("a string is not valid UTF-8: {e}")))
    }

    /// Skips one JSON value, however deeply nested.
    pub fn skip_value(&mut self) -> Result<()> {
        let mut depth: usize = 0;

        loop {
            match self.peek_byte()? {
                None => return Err(Error::bundle(&self.source, "unexpected end of input")),
                Some(b'"') => {
                    self.read_string()?;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Some(b'{' | b'[') => {
                    self.next_byte()?;
                    depth += 1;
                }
                Some(b'}' | b']') => {
                    // A closing brace at depth 0 belongs to the parent object,
                    // so leave it for the caller.
                    if depth == 0 {
                        return Ok(());
                    }
                    self.next_byte()?;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Some(b',') => {
                    if depth == 0 {
                        return Ok(());
                    }
                    self.next_byte()?;
                }
                Some(_) => {
                    // A scalar: number, true, false, or null.
                    self.next_byte()?;
                    if depth == 0 {
                        // Run to the end of the token.
                        while let Some(b) = self.peek_byte()? {
                            if b == b',' || b == b'}' || b == b']' || b.is_ascii_whitespace() {
                                break;
                            }
                            self.next_byte()?;
                        }
                        return Ok(());
                    }
                }
            }
        }
    }
}

impl<R: Read> Scanner<R> {
    /// Consumes the opening `{` of the root object.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Bundle`] if the stream is empty or does not begin with
    /// a JSON object.
    pub fn enter_root_object(&mut self) -> Result<()> {
        self.skip_whitespace()?;
        match self.next_byte()? {
            Some(b'{') => Ok(()),
            Some(other) => Err(Error::bundle(
                &self.source,
                format!(
                    "expected a JSON object at the root, found {:?}",
                    char::from(other)
                ),
            )),
            None => Err(Error::bundle(&self.source, "the file is empty")),
        }
    }

    /// Reads the next key of the object currently being walked.
    ///
    /// Returns `None` at the object's closing brace, which it consumes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Bundle`] if the object is malformed or unclosed.
    pub fn next_key(&mut self) -> Result<Option<String>> {
        loop {
            self.skip_whitespace()?;
            match self.peek_byte()? {
                Some(b'}') => {
                    self.next_byte()?;
                    return Ok(None);
                }
                Some(b',') => {
                    self.next_byte()?;
                }
                Some(b'"') => break,
                Some(other) => {
                    return Err(Error::bundle(
                        &self.source,
                        format!("expected a key, found {:?}", char::from(other)),
                    ));
                }
                None => return Err(Error::bundle(&self.source, "the object is unclosed")),
            }
        }

        let key = self.read_string()?;
        self.skip_whitespace()?;
        match self.next_byte()? {
            Some(b':') => Ok(Some(key)),
            _ => Err(Error::bundle(
                &self.source,
                format!("expected ':' after the key {key:?}"),
            )),
        }
    }

    /// Walks the root object to `key`, leaving the stream on its value.
    ///
    /// Returns `false` if the object ended without the key. Values before it
    /// are skipped, not buffered, so the cost is bounded by how far in the key
    /// sits rather than by the document's size.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Bundle`] if the document is malformed.
    pub fn seek_root_key(&mut self, key: &str) -> Result<bool> {
        self.enter_root_object()?;
        while let Some(found) = self.next_key()? {
            self.skip_whitespace()?;
            if found == key {
                return Ok(true);
            }
            self.skip_value()?;
        }
        Ok(false)
    }

    /// Reads one complete JSON value and returns its raw text.
    ///
    /// The mirror of [`Scanner::skip_value`], capturing instead of discarding.
    /// Memory is bounded by this one value — for a FHIR Bundle that is one
    /// `entry`, which is what keeps a huge bundle readable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Bundle`] if the value is malformed or truncated.
    pub fn read_raw_value(&mut self) -> Result<Vec<u8>> {
        self.skip_whitespace()?;
        let mut out = Vec::new();
        let mut depth: usize = 0;

        loop {
            match self.peek_byte()? {
                None => return Err(Error::bundle(&self.source, "unexpected end of input")),
                Some(b'"') => {
                    self.capture_string(&mut out)?;
                    if depth == 0 {
                        return Ok(out);
                    }
                }
                Some(b @ (b'{' | b'[')) => {
                    self.next_byte()?;
                    out.push(b);
                    depth += 1;
                }
                Some(b @ (b'}' | b']')) => {
                    if depth == 0 {
                        return Ok(out);
                    }
                    self.next_byte()?;
                    out.push(b);
                    depth -= 1;
                    if depth == 0 {
                        return Ok(out);
                    }
                }
                Some(b',') => {
                    if depth == 0 {
                        return Ok(out);
                    }
                    self.next_byte()?;
                    out.push(b',');
                }
                Some(b) => {
                    self.next_byte()?;
                    out.push(b);
                    if depth == 0 {
                        // Run to the end of the scalar token.
                        while let Some(next) = self.peek_byte()? {
                            if next == b',' || next == b'}' || next == b']' || next.is_ascii_whitespace()
                            {
                                break;
                            }
                            self.next_byte()?;
                            out.push(next);
                        }
                        return Ok(out);
                    }
                }
            }
        }
    }

    /// Copies a JSON string, quotes and escapes intact, into `out`.
    fn capture_string(&mut self, out: &mut Vec<u8>) -> Result<()> {
        match self.next_byte()? {
            Some(b'"') => out.push(b'"'),
            _ => return Err(Error::bundle(&self.source, "expected a string")),
        }
        loop {
            match self.next_byte()? {
                None => return Err(Error::bundle(&self.source, "unterminated string")),
                Some(b'"') => {
                    out.push(b'"');
                    return Ok(());
                }
                Some(b'\\') => {
                    out.push(b'\\');
                    match self.next_byte()? {
                        Some(next) => out.push(next),
                        None => {
                            return Err(Error::bundle(&self.source, "unterminated escape"));
                        }
                    }
                }
                Some(b) => out.push(b),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanner(input: &str) -> Scanner<std::io::Cursor<Vec<u8>>> {
        Scanner::new(std::io::Cursor::new(input.as_bytes().to_vec()), "<test>")
    }

    #[test]
    fn seeks_to_a_key_and_reports_a_missing_one() {
        let mut s = scanner(r#"{"a": 1, "entry": [1,2], "b": 2}"#);
        assert!(s.seek_root_key("entry").unwrap());

        let mut s = scanner(r#"{"a": 1, "b": {"entry": []}}"#);
        // Only the ROOT object is walked; a nested `entry` must not match.
        assert!(!s.seek_root_key("entry").unwrap());
    }

    #[test]
    fn reads_raw_values_of_every_shape() {
        for input in [
            r#"{"a":1}"#,
            "[1,2,3]",
            r#""a string""#,
            "42",
            "-1.5e10",
            "true",
            "null",
            r#"{"nested":{"deep":[{"x":"}"}]}}"#,
        ] {
            let mut s = scanner(input);
            let raw = s.read_raw_value().unwrap();
            let text = String::from_utf8(raw).unwrap();
            assert_eq!(text, input, "round trip of {input}");
            // And it is parseable, which is the point.
            serde_json::from_str::<serde_json::Value>(&text)
                .unwrap_or_else(|e| panic!("{input} did not parse: {e}"));
        }
    }

    #[test]
    fn raw_values_preserve_escapes_exactly() {
        let input = r#"{"a":"line\nbreak \"quoted\" \\ back é"}"#;
        let mut s = scanner(input);
        let text = String::from_utf8(s.read_raw_value().unwrap()).unwrap();
        assert_eq!(text, input);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["a"], "line\nbreak \"quoted\" \\ back é");
    }

    #[test]
    fn braces_inside_strings_do_not_end_a_value() {
        let mut s = scanner("{\"a\":\"}]}]\"} trailing");
        let text = String::from_utf8(s.read_raw_value().unwrap()).unwrap();
        assert_eq!(text, r#"{"a":"}]}]"}"#);
    }

    #[test]
    fn a_truncated_value_is_an_error_not_a_panic() {
        for input in [r#"{"a":"#, r#"{"a":"unterminated"#, "[1,2"] {
            let mut s = scanner(input);
            assert!(s.read_raw_value().is_err(), "{input} should fail");
        }
    }
}
