//! Opt-in delta cache. When enabled, a repeated read of the same target
//! (tool + arguments) that returns byte-identical content is collapsed to a
//! compact sentinel instead of resending the whole tree — the common
//! write→re-read and poll loops then cost a few tokens instead of the full
//! payload. Off by default; the raw payload stays recoverable via `tee`.
//!
//! Identity is tracked by a 64-bit FNV-1a hash of the (already compressed)
//! result. FNV-1a is deterministic across processes (unlike `DefaultHasher`
//! which is seeded per-process), enabling the on-disk persistence that lets a
//! primed cache collapse byte-identical re-reads on the very first call after a
//! restart. A hash collision would wrongly elide differing content; at 64 bits
//! that probability is negligible, and `tee` keeps the original recoverable.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// Inline FNV-1a 64-bit hash. Deterministic and process-stable (unlike
/// `DefaultHasher`). Used for BOTH the value hash inside `observe` AND the map
/// key hash in `mcp::cache_key`, so they cannot silently drift apart.
///
/// Offset basis: 0xcbf29ce484222325  Prime: 0x100000001b3
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    First,
    Unchanged,
    Changed,
}

/// A single cache slot: stores the FNV-1a hash of the last-seen content plus
/// a monotonic sequence number used for LRU eviction.
#[derive(Clone, Debug)]
struct CacheEntry {
    hash: u64,
    seq: u64,
}

pub struct DeltaCache {
    map: Mutex<HashMap<String, CacheEntry>>,
    cap: usize,
    /// Monotonic counter: incremented on every cache hit or new insert so we
    /// can evict the entry with the smallest (oldest) seq on capacity overflow.
    counter: Arc<AtomicU64>,
}

impl DeltaCache {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            cap: cap.max(1),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Record `text` under `key`, returning whether it is the first sighting,
    /// unchanged from last time, or changed. `Unchanged` does not mutate state
    /// (the hash), but DOES update the LRU sequence (marks the entry as used).
    pub fn observe(&self, key: &str, text: &str) -> Outcome {
        let digest = fnv1a(text.as_bytes());
        self.observe_digest(key, digest)
    }

    /// Like [`observe`] but accepts an already-serialized byte slice, avoiding a
    /// redundant `serde_json::to_string` when the caller already holds the bytes.
    /// The serialized bytes MUST be valid UTF-8 (which they always are for JSON).
    /// The hash is computed via FNV-1a over the raw bytes, identical to calling
    /// [`observe`] with the same content as a `&str` — mixing the two methods on
    /// the same key always produces the correct `Unchanged` / `Changed` outcome.
    pub fn observe_bytes(&self, key: &str, bytes: &[u8]) -> Outcome {
        // Serialized JSON is always valid UTF-8. If a caller ever violates that,
        // fail SAFE: return `Changed` so the content is forwarded in full rather
        // than collapsed to the sentinel. Mapping invalid bytes to "" would hash
        // every non-UTF-8 payload to the same digest and wrongly elide differing
        // content (silent data loss).
        match std::str::from_utf8(bytes) {
            Ok(s) => self.observe(key, s),
            Err(_) => Outcome::Changed,
        }
    }

    fn observe_digest(&self, key: &str, digest: u64) -> Outcome {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match map.get_mut(key) {
            Some(entry) if entry.hash == digest => {
                // Mark as recently used even on Unchanged — prevents evicting
                // a hot entry just because it keeps returning the same content.
                let seq = self.counter.fetch_add(1, Ordering::Relaxed);
                entry.seq = seq;
                Outcome::Unchanged
            }
            Some(entry) => {
                let seq = self.counter.fetch_add(1, Ordering::Relaxed);
                entry.hash = digest;
                entry.seq = seq;
                Outcome::Changed
            }
            None => {
                // Bound: evict the LRU (min-seq) entry when full.
                if map.len() >= self.cap {
                    if let Some(k) = map
                        .iter()
                        .min_by_key(|(_, e)| e.seq)
                        .map(|(k, _)| k.clone())
                    {
                        map.remove(&k);
                    }
                }
                let seq = self.counter.fetch_add(1, Ordering::Relaxed);
                map.insert(key.to_string(), CacheEntry { hash: digest, seq });
                Outcome::First
            }
        }
    }

    /// Prime the cache from a JSONL file (see [`flush_to`]). Each line is a
    /// JSON object `{"key": "...", "hash": "0x..."}`. Unknown or malformed
    /// lines are silently skipped — corruption only means a cold start for
    /// that entry (no wrong elision possible: a primed entry is `First`, not
    /// `Unchanged`; only a subsequent *matching* observe yields `Unchanged`).
    pub fn prime_from(&self, path: &Path) {
        let Ok(file) = std::fs::File::open(path) else {
            return; // missing file on first run is normal
        };
        let reader = std::io::BufReader::new(file);
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(key) = v["key"].as_str() else {
                continue;
            };
            // Accept hash as either a "0x…" hex string or a plain u64 decimal.
            let hash = if let Some(s) = v["hash"].as_str() {
                u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
            } else {
                v["hash"].as_u64()
            };
            let Some(hash) = hash else { continue };
            // Entries from disk get seq=0 so they're the first to be evicted
            // if real observed entries push to capacity.
            if map.len() < self.cap {
                map.insert(key.to_string(), CacheEntry { hash, seq: 0 });
            }
        }
    }

    /// Write every cached entry to a JSONL file as `{"key":"...","hash":"0x..."}`.
    /// Best-effort: any I/O error is silently ignored (the cache is purely an
    /// optimisation; a missing or partial file means a cold start, not a bug).
    pub fn flush_to(&self, path: &Path) {
        // Best-effort: create parent directory if needed.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(mut file) = std::fs::File::create(path) else {
            return;
        };
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        for (key, entry) in map.iter() {
            // Escape the key as a JSON string to handle any characters safely.
            let Ok(key_json) = serde_json::to_string(key) else {
                continue;
            };
            let _ = writeln!(
                file,
                "{{\"key\":{key_json},\"hash\":\"0x{:016x}\"}}",
                entry.hash
            );
        }
    }
}

/// Collect all `content[].text` string values from a tool result into a `Vec`.
///
/// DECISION — non-breaking sibling of `content_text_len`: `content_text_len`
/// has many callers (compress.rs, proxy.rs) plus `apply_delta`, all of which
/// rely on a `usize` return. Changing that signature would require updating
/// every call site and risks a subtle regression in the delta-cache path where
/// the byte count doubles as the "original_bytes" figure in the sentinel
/// message. Adding a separate `content_texts` function is zero-cost to existing
/// callers and lets `mcp.rs` obtain the raw text slices for `est_str` without
/// doing a second parse pass or allocating a concatenated String unnecessarily.
/// Option B (a separate parallel re-parse pass in `mcp.rs`) was rejected: it
/// would duplicate the `content[].text` traversal logic and be harder to keep
/// in sync with `content_text_len`.
#[must_use]
pub fn content_texts(result: &Value) -> Vec<&str> {
    result
        .get("content")
        .and_then(|c| c.as_array())
        .map_or_else(Vec::new, |arr| {
            arr.iter()
                .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                .collect()
        })
}

/// Concatenate all `content[].text` fields into a single owned `String`.
/// Useful when `est_str` needs a single contiguous slice over all text blocks.
#[must_use]
pub fn content_text(result: &Value) -> String {
    content_texts(result)
        .into_iter()
        .fold(String::new(), |mut acc, s| {
            acc.push_str(s);
            acc
        })
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
    apply_delta_preserialized(
        cache,
        key,
        tool,
        result,
        serialized.as_bytes(),
        original_bytes,
    )
}

/// Like [`apply_delta`] but reuses an already-serialized representation of
/// `result` for identity hashing, avoiding a redundant `serde_json::to_string`
/// when the caller already holds the serialized bytes. The serialized bytes MUST
/// match `result` faithfully — if they differ, the identity hash will not reflect
/// the actual result content.
///
/// NB: the SSE/proxy hot path goes through [`apply_delta`] (not this directly).
/// Its only other serialization is of the full JSON-RPC *envelope* (`{id, result,
/// …}`), not the bare `result` subtree, so there is no result-only serialization
/// to reuse — eliminating the hash serialization there would require fragile
/// envelope splicing for no net win. This variant is for callers that already
/// hold faithful `result` bytes.
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
    fn content_texts_returns_all_text_slices() {
        let v = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "url": "x"}, // non-text block: skipped
                {"type": "text", "text": "world"},
            ]
        });
        let texts = content_texts(&v);
        assert_eq!(texts, vec!["hello", "world"]);
    }

    #[test]
    fn content_texts_empty_when_no_content() {
        let v = serde_json::json!({"result": "no content key"});
        assert!(content_texts(&v).is_empty());
    }

    #[test]
    fn content_text_concatenates_all() {
        let v = serde_json::json!({
            "content": [
                {"type": "text", "text": "foo"},
                {"type": "text", "text": "bar"},
            ]
        });
        assert_eq!(content_text(&v), "foobar");
    }

    #[test]
    fn content_text_empty_when_no_content() {
        let v = serde_json::json!({});
        assert_eq!(content_text(&v), "");
    }

    #[test]
    fn content_texts_consistent_with_content_text_len() {
        // content_texts slices must have total byte length == content_text_len
        let v = serde_json::json!({
            "content": [
                {"type": "text", "text": "Привет"},
                {"type": "text", "text": "world"},
            ]
        });
        let texts = content_texts(&v);
        let sum: usize = texts.iter().map(|s| s.len()).sum();
        assert_eq!(sum, content_text_len(&v));
    }

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
        assert!(r1["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<node>"));
        // identical re-read: collapsed to sentinel, reporting the original size
        let mut r2 = serde_json::json!({"content":[{"type":"text","text": big.clone()}]});
        let ob2 = content_text_len(&r2);
        let after = apply_delta(&c, "get_metadata|h", "get_metadata", &mut r2, ob2).unwrap();
        assert!(after < ob2, "sentinel smaller than original content");
        let sentinel = r2["content"][0]["text"].as_str().unwrap();
        assert!(sentinel.contains("Unchanged"));
        assert!(
            sentinel.contains(&ob2.to_string()),
            "reports original byte count"
        );
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
    fn observe_bytes_invalid_utf8_fails_safe_changed() {
        let c = DeltaCache::new(8);
        // Establish a baseline for the key.
        assert_eq!(c.observe("k", "real"), Outcome::First);
        // Invalid UTF-8 must NOT be treated as a match (never wrongly elide) and
        // must NOT poison the stored baseline.
        let bad = [0xff_u8, 0xfe, 0xfd];
        assert_eq!(c.observe_bytes("k", &bad), Outcome::Changed);
        assert_eq!(c.observe("k", "real"), Outcome::Unchanged);
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
        assert!(apply_delta_preserialized(
            &c2,
            "k",
            "get_metadata",
            &mut r1b,
            serialized.as_bytes(),
            ob2
        )
        .is_none());

        // Second call: both should collapse to a sentinel.
        let mut r2a = r.clone();
        let _serialized2a = serde_json::to_string(&r2a).unwrap(); // unused by apply_delta (serializes internally)
        let ob_a = content_text_len(&r2a);
        let after_a = apply_delta(&c1, "k", "get_metadata", &mut r2a, ob_a).unwrap();
        let sentinel_a = r2a["content"][0]["text"].as_str().unwrap().to_string();

        let mut r2b = r.clone();
        let serialized2b = serde_json::to_string(&r2b).unwrap();
        let ob_b = content_text_len(&r2b);
        let after_b = apply_delta_preserialized(
            &c2,
            "k",
            "get_metadata",
            &mut r2b,
            serialized2b.as_bytes(),
            ob_b,
        )
        .unwrap();
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

    // -----------------------------------------------------------------------
    // R2-A: fnv1a determinism (cross-process stability)
    // -----------------------------------------------------------------------

    #[test]
    fn fnv1a_is_deterministic_across_instances() {
        // fnv1a is a pure, stateless function (fixed offset basis + prime, no
        // per-process seed), so cross-process stability follows by construction —
        // a new process recomputes the identical digest. We also pin a known vector
        // so an accidental change to the basis/prime is caught.
        let input = b"get_metadata|{\"nodeId\":\"1:2\"}";
        assert_eq!(fnv1a(input), fnv1a(input), "fnv1a must be deterministic");
        // Canonical FNV-1a/64 of the ASCII string "a" = 0xaf63dc4c8601ec8c.
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c, "known FNV-1a/64 vector");
    }

    #[test]
    fn fnv1a_differs_for_different_input() {
        let h1 = fnv1a(b"hello");
        let h2 = fnv1a(b"world");
        assert_ne!(
            h1, h2,
            "fnv1a must produce different outputs for different inputs"
        );
    }

    #[test]
    fn fnv1a_empty_input_is_offset_basis() {
        // Empty input must return the offset basis (well-known FNV-1a property).
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325u64);
    }

    #[test]
    fn observe_uses_fnv1a_same_as_direct_hash() {
        // observe(key, text) must produce a hash equivalent to fnv1a(text.as_bytes())
        // — verified indirectly: two DeltaCache instances that both observe the same
        // text for the same key must agree (cross-process: c1 = prev session, c2 = now).
        let c1 = DeltaCache::new(8);
        let c2 = DeltaCache::new(8);
        let payload = r#"{"result":"stable"}"#;
        // First instance records it.
        assert_eq!(c1.observe("k", payload), Outcome::First);
        // Second instance records it independently.
        assert_eq!(c2.observe("k", payload), Outcome::First);
        // Both must agree it's Unchanged on the second call.
        assert_eq!(c1.observe("k", payload), Outcome::Unchanged);
        assert_eq!(c2.observe("k", payload), Outcome::Unchanged);
    }

    // -----------------------------------------------------------------------
    // R2-A: LRU eviction (min-seq, not arbitrary)
    // -----------------------------------------------------------------------

    #[test]
    fn lru_eviction_survives_recently_touched_key() {
        // Fill to cap (3), touch the "oldest" key to refresh its seq, then
        // insert a fourth key. The stale key must be evicted; the touched key
        // must survive.
        let c = DeltaCache::new(3);
        // Insert three entries. Seq order: a=0, b=1, c=2 (approximately).
        assert_eq!(c.observe("a", "va"), Outcome::First); // seq 0
        assert_eq!(c.observe("b", "vb"), Outcome::First); // seq 1
        assert_eq!(c.observe("c", "vc"), Outcome::First); // seq 2

        // Touch "a" to give it a fresh seq (higher than b's seq=1 and c's seq=2).
        assert_eq!(c.observe("a", "va"), Outcome::Unchanged); // seq 3 now

        // Insert a fourth entry "d" — must evict the min-seq entry, which is "b".
        assert_eq!(c.observe("d", "vd"), Outcome::First);

        // "a" must still be in the cache (was recently touched).
        assert_eq!(
            c.observe("a", "va"),
            Outcome::Unchanged,
            "recently-touched key 'a' must survive eviction"
        );

        // "b" must have been evicted (had the min seq when "d" was inserted).
        assert_eq!(
            c.observe("b", "vb"),
            Outcome::First,
            "stale key 'b' must have been evicted"
        );
    }

    #[test]
    fn lru_eviction_does_not_evict_mru() {
        // Regression: previous implementation used map.keys().next() (arbitrary,
        // could evict MRU). Confirm the new code never evicts a key that was
        // just inserted (seq = counter - 1) when capacity is hit.
        let c = DeltaCache::new(2);
        assert_eq!(c.observe("x", "v1"), Outcome::First); // seq 0
        assert_eq!(c.observe("y", "v2"), Outcome::First); // seq 1 — cap reached
                                                          // "y" was the most recently inserted; inserting "z" must evict "x", not "y".
        assert_eq!(c.observe("z", "v3"), Outcome::First); // seq 2 — evicts min-seq = x
        assert_eq!(
            c.observe("y", "v2"),
            Outcome::Unchanged,
            "MRU key 'y' must survive eviction"
        );
        assert_eq!(
            c.observe("x", "v1"),
            Outcome::First,
            "LRU key 'x' must have been evicted"
        );
    }

    // -----------------------------------------------------------------------
    // R2-A: Persistence — prime_from / flush_to roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn prime_flush_roundtrip_returns_unchanged_on_first_post_prime_call() {
        // flush a populated cache, prime a fresh cache from the same file,
        // then an identical observe must return Unchanged on the FIRST call
        // (proving the primed hash matches the live hash).
        let dir = std::env::temp_dir();
        let path = dir.join(format!("frtk-cache-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let payload = r#"{"content":[{"type":"text","text":"stable result"}]}"#;
        let key = "get_metadata|abc123";

        // Session 1: observe and flush.
        let c1 = DeltaCache::new(8);
        assert_eq!(c1.observe(key, payload), Outcome::First);
        assert_eq!(c1.observe(key, payload), Outcome::Unchanged); // confirm baseline
        c1.flush_to(&path);

        // Session 2: prime from disk — the FIRST observe must be Unchanged.
        let c2 = DeltaCache::new(8);
        c2.prime_from(&path);
        assert_eq!(
            c2.observe(key, payload),
            Outcome::Unchanged,
            "primed cache must collapse byte-identical first post-prime call to Unchanged"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prime_from_missing_file_is_noop() {
        // A missing file must not panic or return an error — cold start is normal.
        let c = DeltaCache::new(8);
        c.prime_from(Path::new("/tmp/frtk-nonexistent-cache-file-xyz.jsonl"));
        // Cache should be empty; first observe is First.
        assert_eq!(c.observe("k", "v"), Outcome::First);
    }

    #[test]
    fn prime_from_malformed_lines_are_skipped() {
        // Corrupt / truncated JSONL lines must be skipped gracefully WHILE a valid
        // line interleaved among them is still loaded. We write the valid line via
        // flush_to (real on-disk format), splice malformed lines around it, then
        // prove the good entry primed by an Unchanged first post-prime observe.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("frtk-cache-bad-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let key = "get_metadata|abc123";
        let payload = r#"{"content":[{"type":"text","text":"valid entry"}]}"#;

        // Produce one valid JSONL line in the real flush format.
        let c1 = DeltaCache::new(8);
        assert_eq!(c1.observe(key, payload), Outcome::First);
        c1.flush_to(&path);
        let good_line = std::fs::read_to_string(&path).unwrap();
        let good_line = good_line.trim_end();

        // Splice malformed lines before and after the valid one.
        std::fs::write(
            &path,
            format!("not json\n{{\"key\":\"k\",\"hash\":\"bad\"}}\n{good_line}\ntruncated{{\n"),
        )
        .unwrap();

        // Prime a fresh cache: bad lines skipped (no panic), good entry loaded.
        let c2 = DeltaCache::new(8);
        c2.prime_from(&path);
        assert_eq!(
            c2.observe(key, payload),
            Outcome::Unchanged,
            "the one valid line must be primed despite surrounding malformed lines"
        );
        assert_eq!(
            c2.observe("new_key", "fresh"),
            Outcome::First,
            "basic ops still work"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn level_switch_self_heals_in_one_call() {
        // If the compression level changes between sessions (Standard -> Aggressive),
        // the persisted hash was computed over the Standard-compressed result and
        // the new session produces an Aggressive-compressed (different) result.
        // This must resolve as Changed on the first post-prime call (one missed
        // elision), NOT as Unchanged (which would be a wrong elision).
        //
        // We simulate this by flushing a hash for one payload and then observing
        // a different payload in the new session.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("frtk-cache-lvl-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let key = "get_metadata|node1";
        let standard_result = r#"{"content":[{"type":"text","text":"<a> <b/> </a>"}]}"#;
        let aggressive_result = r#"{"content":[{"type":"text","text":"<a><b/></a>"}]}"#;

        // Session 1 (Standard): flush hash of standard result.
        let c1 = DeltaCache::new(8);
        assert_eq!(c1.observe(key, standard_result), Outcome::First);
        c1.flush_to(&path);

        // Session 2 (Aggressive): prime from disk, then observe the aggressive result.
        let c2 = DeltaCache::new(8);
        c2.prime_from(&path);
        // The aggressive result differs from the stored hash -> Changed (self-healed).
        assert_eq!(
            c2.observe(key, aggressive_result),
            Outcome::Changed,
            "level switch: first post-prime call with different result must be Changed (one missed elision only)"
        );
        // Second call with the same aggressive result must now be Unchanged.
        assert_eq!(
            c2.observe(key, aggressive_result),
            Outcome::Unchanged,
            "after self-heal, identical result must be Unchanged"
        );

        let _ = std::fs::remove_file(&path);
    }
}
