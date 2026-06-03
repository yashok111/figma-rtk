//! Rough token estimation. Like RTK's savings figures, this is an estimate, not
//! an exact tokenizer count: ~4 bytes per token is a well-known heuristic for
//! English/JSON/code that is good enough for a savings meter.

pub fn est(bytes: usize) -> usize {
    bytes.div_ceil(4)
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
