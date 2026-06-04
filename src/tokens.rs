//! Rough token estimation. Like RTK's savings figures, this is an estimate, not
//! an exact tokenizer count: ~4 bytes per token is a well-known heuristic for
//! English/JSON/code that is good enough for a savings meter.
//!
//! `est_str` improves on the flat 4 B/tok heuristic by classifying the content:
//! whitespace-dominant content tokenizes at ~10 B/tok; attribute/punctuation-dense
//! content at ~2 B/tok; non-ASCII (Cyrillic, CJK, etc.) at ~6 B/tok (each word
//! is roughly one token, not one byte/4); and typical JSON/code at ~3.5 B/tok.
//!
//! DELIBERATE NON-DECISION: we do not embed `tiktoken-rs` (cl100k vocab — wrong
//! for Claude) nor any reverse-engineered Claude tokenizer (brittle, heavy
//! HF-tokenizers build dep, drifts per model). A content-aware heuristic is good
//! enough for the savings meter, avoids a multi-MB build dep, and never drifts.

pub fn est(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

/// Content-aware token estimate for a string slice. Each byte contributes a
/// per-class weight (in milli-tokens) and the weights are summed; the estimate is
/// `ceil(sum / 1000)`. The weights encode the same intent as the old bucket
/// divisors — whitespace ~10 B/tok, ASCII punctuation/attr chars ~2 B/tok,
/// non-ASCII (Cyrillic/CJK) ~6 B/tok, everything else ~3.5 B/tok — but as an
/// additive sum rather than a single dominant-regime divisor.
///
/// Why a weighted sum, not a dominant divisor: the old approach was NON-MONOTONIC
/// across a compression boundary. A whitespace-dominant pretty payload classified
/// at ~10 B/tok, minified into a punctuation-dense one classified at ~2 B/tok,
/// could report MORE tokens after SHRINKING the bytes — so `frtk gain` showed
/// negative savings on a real reduction. A non-negative per-class sum is monotonic:
/// removing characters (e.g. stripping whitespace) can never raise the estimate.
///
/// Single pass over the bytes. Returns 0 when `s` is empty.
///
/// DELIBERATE NON-DECISION (unchanged): no `tiktoken-rs` / reverse-engineered
/// Claude tokenizer — see the module doc comment.
pub fn est_str(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    // Per-class milli-token weights (1000 = one token).
    const WS: usize = 100; //  ~10 B/tok — indentation / blank lines tokenize cheaply
    const PUNCT: usize = 500; // ~2 B/tok — `=`, `"`, `;`, `{`, `}` are heavy splits
    const NON_ASCII: usize = 167; // ~6 B/tok per byte — Cyrillic/CJK ~1 tok/char @ 2-4 B
    const OTHER: usize = 286; // ~3.5 B/tok — general JSON/code/text

    let mut milli = 0usize;
    for &b in s.as_bytes() {
        milli += if b.is_ascii_whitespace() {
            WS
        } else if !b.is_ascii() {
            NON_ASCII
        } else if matches!(
            b,
            b'=' | b'"' | b'\'' | b';' | b'{' | b'}' | b'/' | b'<' | b'>' | b',' | b'[' | b']' | b'(' | b')' | b':'
        ) {
            PUNCT
        } else {
            OTHER
        };
    }
    milli.div_ceil(1000)
}

/// Tokens saved: estimated token count of `before` minus estimated count of
/// `after`. Saturates at zero (never negative).
pub fn tok_saved(before: usize, after: usize) -> usize {
    est(before).saturating_sub(est(after))
}

/// Percentage of bytes saved relative to `before`. Returns `0.0` when
/// `before == 0` to avoid division by zero.
pub fn pct_saved(before: usize, after: usize) -> f64 {
    if before == 0 {
        return 0.0;
    }
    100.0 * before.saturating_sub(after) as f64 / before as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- est_str tests (TDD: written before implementation) ---

    #[test]
    fn whitespace_is_cheap() {
        // Mostly-whitespace content (lots of indentation): est_str should yield
        // far fewer tokens than est(bytes) which assumes 4 B/tok uniformly.
        // 60 spaces + 40 chars of content = 100 bytes, ~60% whitespace.
        let s = "    ".repeat(15) + "const x = 1;"; // 60 spaces + 12 chars = 72 bytes
        let flat = est(s.len());
        let smart = est_str(&s);
        assert!(
            smart < flat,
            "whitespace-dominant: est_str ({smart}) should be less than est() ({flat})"
        );
    }

    #[test]
    fn dense_attrs_expensive() {
        // Punctuation/attribute-dense content: est_str should yield MORE tokens
        // than est(bytes) (each punct char is a tokenizer boundary).
        // Minified JSON attributes: lots of `"`, `{`, `}`, `,`, `:`.
        let s = r#"{"a":"b","c":"d","e":"f","g":"h","i":"j","k":"l","m":"n"}"#;
        let flat = est(s.len());
        let smart = est_str(s);
        assert!(
            smart > flat,
            "attr-dense: est_str ({smart}) should exceed est() ({flat}) for punct-heavy content"
        );
    }

    #[test]
    fn cyrillic_per_word() {
        // Cyrillic text: each word is ~1 token but 2-4 UTF-8 bytes per char.
        // est(bytes) over-counts significantly; est_str should be lower.
        let s = "Привет мир это тест Кириллица токен оценка слов";
        // Cyrillic chars are 2 bytes each, so byte-count is ~2x char-count.
        let flat = est(s.len());
        let smart = est_str(s);
        assert!(
            smart < flat,
            "Cyrillic: est_str ({smart}) should be less than est() ({flat}), not byte/4"
        );
        // Sanity: roughly word-count (8 words), ±3x tolerance.
        let word_count = s.split_whitespace().count();
        assert!(
            smart <= word_count * 3,
            "Cyrillic: est_str ({smart}) should be in the same ballpark as word count ({word_count})"
        );
    }

    #[test]
    fn empty_string_returns_zero() {
        assert_eq!(est_str(""), 0, "empty string must return 0");
    }

    #[test]
    fn compression_never_inverts_token_estimate() {
        // REGRESSION: the old dominant-regime divisor inverted across a compression
        // boundary — a whitespace-dominant pretty payload (classified ~10 B/tok)
        // minified into a punctuation-dense one (classified ~2 B/tok) made est_str
        // report MORE tokens after shrinking bytes (e.g. 95 B -> 10 tok vs 47 B ->
        // 24 tok), so `frtk gain` showed negative savings. A real byte reduction must
        // never raise the token estimate.
        let pretty = "{\n  \"brand/white\": \"#ffffff\",\n  \"spacing/0\": \"0\",\n  \"radius/sm\": \"4px\"\n}";
        let minified = "{\"brand/white\":\"#ffffff\",\"spacing/0\":\"0\",\"radius/sm\":\"4px\"}";
        assert!(minified.len() < pretty.len(), "minified must be fewer bytes");
        let tok_pretty = est_str(pretty);
        let tok_min = est_str(minified);
        assert!(
            tok_min <= tok_pretty,
            "minifying ({} B -> {} B) must not raise the token estimate ({tok_pretty} -> {tok_min})",
            pretty.len(),
            minified.len(),
        );
    }

    #[test]
    fn est_str_monotonic_under_whitespace_strip() {
        // The estimator must be monotonic: removing whitespace can only lower (or
        // hold) the token count, never raise it. This is the invariant the weighted
        // per-class sum guarantees and the old bucket approach violated.
        for s in [
            "    indented   code   with   gaps  ;",
            "{\n\t\"a\": 1,\n\t\"b\": [1, 2, 3]\n}",
            "Привет   мир\n\n  это  тест",
            "<a>\n    <b/>\n    <c/>\n</a>",
        ] {
            let stripped: String = s.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                est_str(&stripped) <= est_str(s),
                "stripping whitespace from {s:?} raised the estimate ({} -> {})",
                est_str(s),
                est_str(&stripped),
            );
        }
    }

    // --- existing tests below ---

    #[test]
    fn tok_saved_normal() {
        // 400 bytes before -> 100 est tokens; 100 bytes after -> 25 est tokens; saved = 75
        assert_eq!(tok_saved(400, 100), 75);
    }

    #[test]
    fn tok_saved_no_saving() {
        // after >= before: no tokens saved
        assert_eq!(tok_saved(100, 200), 0);
    }

    #[test]
    fn tok_saved_zero_before() {
        // before == 0: no div-by-zero, result is 0
        assert_eq!(tok_saved(0, 0), 0);
        assert_eq!(tok_saved(0, 100), 0);
    }

    #[test]
    fn pct_saved_zero_before_no_panic() {
        // Must return 0.0 without panicking.
        assert_eq!(pct_saved(0, 0), 0.0);
        assert_eq!(pct_saved(0, 10), 0.0);
    }

    #[test]
    fn pct_saved_normal() {
        // 200 bytes -> 100 bytes = 50%
        let p = pct_saved(200, 100);
        assert!((p - 50.0).abs() < 0.01, "expected ~50%, got {p}");
    }

    #[test]
    fn pct_saved_no_saving() {
        // after >= before: 0%
        assert_eq!(pct_saved(100, 100), 0.0);
    }
}
