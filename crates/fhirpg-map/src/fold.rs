//! Search folding for FHIR `string` parameters (P6.6).
//!
//! FHIR requires `string` search to be case-insensitive **and**
//! accent-insensitive, worldwide. Doing that at query time means either an
//! expression index (which a parameterised `LIKE` will not use) or folding in
//! SQL and in Rust, two implementations that must agree for every codepoint.
//!
//! Instead the engine folds once, in Rust, at write time, into a companion
//! `_norm` column. Queries fold the search term with the *same* function and
//! compare against that column, so there is exactly one definition of "the
//! same string" in the system. The column is declared `COLLATE "C"` so that
//! ordering is by Unicode codepoint, which is what makes [`prefix_upper`]
//! sound as a range scan.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Fold a string for accent- and case-insensitive comparison.
///
/// Decomposes (NFD), drops combining marks, then lowercases. Lowercasing can
/// itself introduce marks — Turkish `İ` lowercases to `i` plus a combining dot
/// above — so marks are stripped again afterwards. The result is idempotent:
/// `fold(fold(s)) == fold(s)`.
pub fn fold(s: &str) -> String {
    let stripped: String = s.nfd().filter(|c| !is_combining_mark(*c)).collect();
    stripped
        .to_lowercase()
        .nfd()
        .filter(|c| !is_combining_mark(*c))
        .collect()
}

/// The least string strictly greater than every string having `prefix` as a
/// prefix, under codepoint order — or `None` when no such string exists
/// (empty prefix, or a prefix of all `char::MAX`), meaning the range is
/// unbounded above.
///
/// This turns a prefix match into `col >= prefix AND col < upper`, a plain
/// btree range scan. The planner does not have to recognise a `LIKE` pattern,
/// so it works with a bound parameter under a generic plan — which is exactly
/// where `LIKE $1` silently falls back to a sequential scan.
///
/// Codepoint order equals UTF-8 byte order, so a `COLLATE "C"` index on the
/// folded column orders the same way this function assumes.
pub fn prefix_upper(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        if let Some(next) = next_char(last) {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
        // `last` is char::MAX: nothing at this position sorts higher, so carry
        // by dropping it and incrementing the position before it.
    }
    None
}

/// The next scalar value after `c`, skipping the surrogate range, or `None`
/// at `char::MAX`.
fn next_char(c: char) -> Option<char> {
    let mut n = c as u32 + 1;
    if n == 0xD800 {
        n = 0xE000;
    }
    char::from_u32(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_and_accents() {
        assert_eq!(fold("MÜLLER"), "muller");
        assert_eq!(fold("Müller"), fold("Muller"));
        assert_eq!(fold("José"), "jose");
        assert_eq!(fold("ÅNGSTRÖM"), "angstrom");
        // Precomposed and decomposed spellings must fold alike; this is the
        // case an ILIKE comparison gets wrong.
        assert_eq!(fold("é"), fold("e\u{301}"));
    }

    #[test]
    fn folds_beyond_latin() {
        assert_eq!(fold("ΑΘΉΝΑ"), "αθηνα");
        assert_eq!(fold("ЙОСИФ"), "иосиф");
        // Scripts without case or marks pass through unchanged.
        assert_eq!(fold("東京"), "東京");
        assert_eq!(fold("مُحَمَّد"), "محمد");
    }

    #[test]
    fn fold_is_idempotent() {
        for s in ["MÜLLER", "José", "ΑΘΉΝΑ", "İstanbul", "e\u{301}", ""] {
            assert_eq!(fold(&fold(s)), fold(s), "not idempotent: {s:?}");
        }
    }

    #[test]
    fn turkish_dotted_i_loses_its_mark() {
        // to_lowercase('İ') yields "i\u{307}"; the second strip removes it.
        assert_eq!(fold("İstanbul"), "istanbul");
    }

    #[test]
    fn prefix_upper_bounds_the_range() {
        assert_eq!(prefix_upper("abc").unwrap(), "abd");
        assert_eq!(prefix_upper("ab\u{10FFFF}").unwrap(), "ac");
        assert_eq!(prefix_upper(""), None);
        assert_eq!(prefix_upper("\u{10FFFF}"), None);
        // Never lands inside the surrogate gap.
        assert_eq!(prefix_upper("\u{D7FF}").unwrap(), "\u{E000}");
    }

    #[test]
    fn prefix_upper_excludes_exactly_the_non_matches() {
        let prefix = "mul";
        let upper = prefix_upper(prefix).unwrap();
        let in_range = |s: &str| s >= prefix && s < upper.as_str();
        for s in ["mul", "muller", "mulz", "mul\u{10FFFF}"] {
            assert!(in_range(s), "{s:?} should be in range");
        }
        for s in ["mu", "mum", "mv", "n", "mula".trim_end_matches("mula")] {
            assert!(!in_range(s) || s.starts_with(prefix), "{s:?} leaked in");
        }
    }
}
