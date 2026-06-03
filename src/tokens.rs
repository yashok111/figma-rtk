//! Rough token estimation. Like RTK's savings figures, this is an estimate, not
//! an exact tokenizer count: ~4 bytes per token is a well-known heuristic for
//! English/JSON/code that is good enough for a savings meter.

pub fn est(bytes: usize) -> usize {
    bytes.div_ceil(4)
}
