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
    #[must_use]
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
        self.observe_digest(key, digest)
    }

    /// Like [`observe`] but accepts an already-serialized byte slice, avoiding a
    /// redundant `serde_json::to_string` when the caller already holds the bytes.
    /// The serialized bytes MUST be valid UTF-8 (which they always are for JSON).
    /// The hash is computed via the `str` path so results are identical to calling
    /// [`observe`] with the same content — mixing the two methods on the same key
    /// always produces the correct `Unchanged` / `Changed` outcome.
    pub fn observe_bytes(&self, key: &str, bytes: &[u8]) -> Outcome {
        // SAFETY: serialized JSON is always valid UTF-8.
        let s = std::str::from_utf8(bytes).unwrap_or_default();
        self.observe(key, s)
    }

    fn observe_digest(&self, key: &str, digest: u64) -> Outcome {
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
#[must_use]
pub fn content_text_len(result: &Value) -> usize {
    result
        .get("content")
        .and_then(|c| c.as_array())
        .map_or(0, |arr| {
            arr.iter()
                .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                .map(str::len)
                .sum()
        })
}

/// If this `result` is byte-identical to the previous one for `key`, replace its
/// content with a sentinel and return the sentinel byte size; otherwise return
/// `None` and leave the result untouched. `original_bytes` is the pre-compression
/// content size, used only for the human-readable "bytes elided" figure.
#[must_use]
pub fn apply_delta(
    cache: &DeltaCache,
    key: &str,
    tool: &str,
    result: &mut Value,
    original_bytes: usize,
) -> Option<usize> {
    let serialized = serde_json::to_string(result).unwrap_or_default();
    apply_delta_preserialized(cache, key, tool, result, serialized.as_bytes(), original_bytes)
}

/// Like [`apply_delta`] but reuses an already-serialized representation of
/// `result` for identity hashing, avoiding a redundant `serde_json::to_string`
/// when the caller already holds the serialized bytes. The serialized bytes MUST
/// match `result` faithfully — if they differ, the identity hash will not reflect
/// the actual result content.
#[must_use]
pub fn apply_delta_preserialized(
    cache: &DeltaCache,
    key: &str,
    tool: &str,
    result: &mut Value,
    serialized: &[u8],
    original_bytes: usize,
) -> Option<usize> {
    if cache.observe_bytes(key, serialized) != Outcome::Unchanged {
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
    fn observe_bytes_same_outcome_as_observe_str() {
        // observe_bytes and observe must agree on identity when called on the SAME
        // DeltaCache for the same key — mixing both methods must never produce a
        // wrong outcome (e.g. Changed when byte content is actually identical).
        let c = DeltaCache::new(8);
        let payload = r#"{"content":[{"type":"text","text":"hello world"}]}"#;
        // observe records the first sighting.
        assert_eq!(c.observe("k", payload), Outcome::First);
        // observe_bytes on the same cache + key + bytes must see Unchanged.
        assert_eq!(
            c.observe_bytes("k", payload.as_bytes()),
            Outcome::Unchanged,
            "observe_bytes must agree with observe for byte-identical content"
        );
        // observe likewise sees Unchanged.
        assert_eq!(c.observe("k", payload), Outcome::Unchanged);
        // Advance with observe, then observe_bytes sees Changed.
        let changed = r#"{"content":[{"type":"text","text":"different"}]}"#;
        assert_eq!(c.observe("k", changed), Outcome::Changed);
        assert_eq!(
            c.observe_bytes("k", changed.as_bytes()),
            Outcome::Unchanged,
            "observe_bytes after observe(changed) sees the new baseline as Unchanged"
        );
        // Now flip back via observe_bytes — observe must see Changed.
        assert_eq!(c.observe_bytes("k", payload.as_bytes()), Outcome::Changed);
        assert_eq!(c.observe("k", payload), Outcome::Unchanged);
    }

    #[test]
    fn apply_delta_preserialized_same_outcome_as_apply_delta() {
        // apply_delta_preserialized must collapse an identical re-read to a sentinel
        // with the same content as apply_delta, proving the pre-serialized path
        // avoids the extra serde_json::to_string without changing observable behavior.
        let c1 = DeltaCache::new(8);
        let c2 = DeltaCache::new(8);
        let big = "<node>".repeat(100);
        let r = serde_json::json!({"content":[{"type":"text","text": big.clone()}]});
        let serialized = serde_json::to_string(&r).unwrap();

        // First call via both paths: neither should collapse.
        let mut r1a = r.clone();
        let ob = content_text_len(&r1a);
        assert!(apply_delta(&c1, "k", "get_metadata", &mut r1a, ob).is_none());

        let mut r1b = r.clone();
        let ob2 = content_text_len(&r1b);
        assert!(apply_delta_preserialized(&c2, "k", "get_metadata", &mut r1b, serialized.as_bytes(), ob2).is_none());

        // Second call: both should collapse to a sentinel.
        let mut r2a = r.clone();
        let _serialized2a = serde_json::to_string(&r2a).unwrap(); // unused by apply_delta (serializes internally)
        let ob_a = content_text_len(&r2a);
        let after_a = apply_delta(&c1, "k", "get_metadata", &mut r2a, ob_a).unwrap();
        let sentinel_a = r2a["content"][0]["text"].as_str().unwrap().to_string();

        let mut r2b = r.clone();
        let serialized2b = serde_json::to_string(&r2b).unwrap();
        let ob_b = content_text_len(&r2b);
        let after_b = apply_delta_preserialized(&c2, "k", "get_metadata", &mut r2b, serialized2b.as_bytes(), ob_b).unwrap();
        let sentinel_b = r2b["content"][0]["text"].as_str().unwrap().to_string();

        // Both must produce the same sentinel content and the same byte-after count.
        assert_eq!(after_a, after_b, "same sentinel size from both paths");
        assert_eq!(sentinel_a, sentinel_b, "same sentinel text from both paths");
        assert!(after_a < ob_a, "sentinel smaller than original");
    }

    #[test]
    fn apply_delta_passes_changed_content() {
        let c = DeltaCache::new(8);
        let mut r1 = serde_json::json!({"content":[{"type":"text","text":"v1"}]});
        let n1 = content_text_len(&r1);
        let _ = apply_delta(&c, "k", "t", &mut r1, n1);
        let mut r2 = serde_json::json!({"content":[{"type":"text","text":"v2-different"}]});
        let n2 = content_text_len(&r2);
        assert!(apply_delta(&c, "k", "t", &mut r2, n2).is_none());
        assert_eq!(r2["content"][0]["text"], "v2-different");
    }
}
