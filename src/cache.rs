//! Opt-in delta cache. When enabled, a repeated read of the same target
//! (tool + arguments) that returns byte-identical content is collapsed to a
//! compact sentinel instead of resending the whole tree — the common
//! write→re-read and poll loops then cost a few tokens instead of the full
//! payload. Off by default; the raw payload stays recoverable via `tee`.
//!
//! Identity is tracked by a 64-bit hash of the (already compressed) result, so
//! the cache stays tiny regardless of payload size. The hasher is `DefaultHasher`
//! (implementation-defined, seeded per process) — fine here because identity is
//! only ever compared within a single proxy run. A hash collision would wrongly
//! elide differing content; at 64 bits that probability is negligible, and `tee`
//! keeps the original recoverable.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    First,
    Unchanged,
    Changed,
}

pub struct DeltaCache {
    map: Mutex<HashMap<String, u64>>,
    cap: usize,
}

impl DeltaCache {
    pub fn new(cap: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            cap: cap.max(1),
        }
    }

    /// Record `text` under `key`, returning whether it is the first sighting,
    /// unchanged from last time, or changed. `Unchanged` does not mutate state.
    pub fn observe(&self, key: &str, text: &str) -> Outcome {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let digest = h.finish();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(key) {
            Some(&prev) if prev == digest => Outcome::Unchanged,
            Some(_) => {
                map.insert(key.to_string(), digest);
                Outcome::Changed
            }
            None => {
                // Bound: evict one arbitrary entry when full (not the whole map,
                // which would make every tracked key go cold at once).
                if map.len() >= self.cap {
                    if let Some(k) = map.keys().next().cloned() {
                        map.remove(&k);
                    }
                }
                map.insert(key.to_string(), digest);
                Outcome::First
            }
        }
    }
}

/// Sum of the byte lengths of all `content[].text` fields in a tool result.
pub fn content_text_len(result: &Value) -> usize {
    result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                .map(str::len)
                .sum()
        })
        .unwrap_or(0)
}

/// If this `result` is byte-identical to the previous one for `key`, replace its
/// content with a sentinel and return the sentinel byte size; otherwise return
/// `None` and leave the result untouched. `original_bytes` is the pre-compression
/// content size, used only for the human-readable "bytes elided" figure.
pub fn apply_delta(
    cache: &DeltaCache,
    key: &str,
    tool: &str,
    result: &mut Value,
    original_bytes: usize,
) -> Option<usize> {
    let serialized = serde_json::to_string(result).unwrap_or_default();
    if cache.observe(key, &serialized) != Outcome::Unchanged {
        return None;
    }
    let msg = format!(
        "[frtk] Unchanged: the {tool} call ran and returned content byte-identical \
         to your previous call for this target (~{original_bytes} bytes elided). This \
         is frtk's delta cache collapsing a duplicate result — NOT a skipped or cached \
         operation; any side effects still happened. Disable `[cache] delta` in config \
         to receive the full payload."
    );
    let after = msg.len();
    *result = serde_json::json!({ "content": [{ "type": "text", "text": msg }] });
    Some(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_first_unchanged_changed() {
        let c = DeltaCache::new(8);
        assert_eq!(c.observe("k", "aaa"), Outcome::First);
        assert_eq!(c.observe("k", "aaa"), Outcome::Unchanged);
        assert_eq!(c.observe("k", "bbb"), Outcome::Changed);
        assert_eq!(c.observe("k", "bbb"), Outcome::Unchanged);
    }

    #[test]
    fn distinct_keys_are_independent() {
        let c = DeltaCache::new(8);
        assert_eq!(c.observe("a", "x"), Outcome::First);
        assert_eq!(c.observe("b", "x"), Outcome::First);
        assert_eq!(c.observe("a", "x"), Outcome::Unchanged);
    }

    #[test]
    fn apply_delta_collapses_identical_reread() {
        let c = DeltaCache::new(8);
        let big = "<node>".repeat(500); // larger than the sentinel
        // first read: not collapsed
        let mut r1 = serde_json::json!({"content":[{"type":"text","text": big.clone()}]});
        let ob1 = content_text_len(&r1);
        assert!(apply_delta(&c, "get_metadata|h", "get_metadata", &mut r1, ob1).is_none());
        assert!(r1["content"][0]["text"].as_str().unwrap().contains("<node>"));
        // identical re-read: collapsed to sentinel, reporting the original size
        let mut r2 = serde_json::json!({"content":[{"type":"text","text": big.clone()}]});
        let ob2 = content_text_len(&r2);
        let after = apply_delta(&c, "get_metadata|h", "get_metadata", &mut r2, ob2).unwrap();
        assert!(after < ob2, "sentinel smaller than original content");
        let sentinel = r2["content"][0]["text"].as_str().unwrap();
        assert!(sentinel.contains("Unchanged"));
        assert!(sentinel.contains(&ob2.to_string()), "reports original byte count");
    }

    #[test]
    fn apply_delta_passes_changed_content() {
        let c = DeltaCache::new(8);
        let mut r1 = serde_json::json!({"content":[{"type":"text","text":"v1"}]});
        let n1 = content_text_len(&r1);
        apply_delta(&c, "k", "t", &mut r1, n1);
        let mut r2 = serde_json::json!({"content":[{"type":"text","text":"v2-different"}]});
        let n2 = content_text_len(&r2);
        assert!(apply_delta(&c, "k", "t", &mut r2, n2).is_none());
        assert_eq!(r2["content"][0]["text"], "v2-different");
    }
}
