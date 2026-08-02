//! Minimal JSON-RPC / MCP awareness: correlate tools/call requests with their
//! responses by id, and rewrite the responses of the heavy read tools.

use std::collections::HashMap;

use serde_json::Value;

use crate::cache::{self, DeltaCache};
use crate::compress;
use crate::config::Level;
use crate::filter::FilterSet;
use crate::stats::StatRec;
use crate::tokens;

/// Tools whose responses are worth compressing. Matched against the wire tool
/// name (Figma's own names, e.g. "get_design_context").
pub const TARGET_TOOLS: &[&str] = &[
    "get_design_context",
    "get_metadata",
    // Flat design-token map (token -> value); losslessly minified + delta-cached
    // (tokens rarely change, so re-reads of the same node collapse to a sentinel).
    "get_variable_defs",
    // node -> { codeConnectSrc, codeConnectName, … } map — exact shape unverified
    // (no captured fixture; Enterprise seat required). Added blind; the generic
    // JSON compression path applies.
    "get_code_connect_map",
    // The general-purpose write/inspect tool: it JSON-serializes its return value,
    // and read-only discovery calls (findAll / children dumps) return large node
    // trees. Minified losslessly + delta-cached; write returns (small id lists)
    // compress harmlessly.
    "use_figma",
    // Screenshot tool: by default the response is a TEXT block holding a
    // short-lived URL (+ a curl note); an inline image content block appears only
    // when enableBase64Response=true. Text is minified; image blocks (when
    // present) pass through byte-for-byte but their payload size is metered for
    // volume tracking (image_bytes in Savings).
    "get_screenshot",
    // Context-for-code-connect and suggestions: JSON payloads with component
    // metadata; losslessly minified via the standard text pass.
    "get_context_for_code_connect",
    "get_code_connect_suggestions",
];

fn is_target(name: &str) -> bool {
    // Exact wire name, or MCP-namespaced (…__get_metadata). The `__` boundary
    // stops unrelated names like "evil_get_metadata" being treated as targets.
    // Delegates to matches_tool which is zero-alloc (no heap allocation).
    TARGET_TOOLS
        .iter()
        .any(|t| crate::filter::matches_tool(name, t))
}

pub fn id_to_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// A target tool-call: its tool name and a cache key derived from tool +
/// arguments (so re-reads of the same node share a delta-cache slot).
#[derive(Clone, Debug)]
pub struct Target {
    pub tool: String,
    pub key: String,
}

fn cache_key(tool: &str, args: &Value) -> String {
    // FNV-1a over the serialized args — deterministic across processes so that
    // primed entries loaded from disk can match live observe calls after restart.
    let serialized = serde_json::to_string(args).unwrap_or_default();
    let h = crate::cache::fnv1a(serialized.as_bytes());
    format!("{tool}|{h:016x}")
}

/// Scan a JSON-RPC request body (single message or batch array) and return a
/// map of request id -> Target, restricted to tools we want to compress.
pub fn extract_targets(body: &[u8]) -> HashMap<String, Target> {
    let mut map = HashMap::new();
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return map;
    };
    let items: Vec<&Value> = match &v {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    for it in items {
        if it.get("method").and_then(|m| m.as_str()) != Some("tools/call") {
            continue;
        }
        let Some(name) = it.pointer("/params/name").and_then(|n| n.as_str()) else {
            continue;
        };
        if !is_target(name) {
            continue;
        }
        if let Some(id) = it.get("id") {
            let args = it
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(Value::Null);
            map.insert(
                id_to_string(id),
                Target {
                    tool: name.to_string(),
                    key: cache_key(name, &args),
                },
            );
        }
    }
    map
}

/// Transform a parsed JSON-RPC value (single or batch), compressing the results
/// of any message whose id maps to a target tool. Returns the savings records.
pub fn transform_value(
    v: &mut Value,
    ids: &HashMap<String, Target>,
    level: Level,
    filters: &FilterSet,
    cache: Option<&DeltaCache>,
) -> Vec<StatRec> {
    let mut recs = Vec::new();
    match v {
        Value::Array(a) => {
            for m in a.iter_mut() {
                recs.extend(transform_msg(m, ids, level, filters, cache));
            }
        }
        other => {
            recs.extend(transform_msg(other, ids, level, filters, cache));
        }
    }
    recs
}

fn transform_msg(
    msg: &mut Value,
    ids: &HashMap<String, Target>,
    level: Level,
    filters: &FilterSet,
    cache: Option<&DeltaCache>,
) -> Vec<StatRec> {
    let Some(id) = msg.get("id") else {
        return vec![];
    };
    let Some(target) = ids.get(&id_to_string(id)).cloned() else {
        return vec![];
    };
    let Some(result) = msg.get_mut("result") else {
        return vec![];
    };
    // Original upstream content size, before compression — used for the delta
    // sentinel's "bytes elided" figure and its savings record.
    let orig_before = cache::content_text_len(result);
    // Capture the before-text (concatenated) for content-aware token estimation.
    // content_text() is a non-breaking sibling of content_text_len — it re-uses
    // the same traversal logic without modifying any existing callers.
    let before_text = cache::content_text(result);
    let tok_before = tokens::est_str(&before_text);

    let sv = compress::compress_result_with(result, &target.tool, level, filters);

    // Delta cache: collapse a byte-identical re-read to a sentinel.
    if let Some(cache) = cache {
        if let Some(after) =
            cache::apply_delta(cache, &target.key, &target.tool, result, orig_before)
        {
            // After delta-collapse: content is now the sentinel string.
            let after_text = cache::content_text(result);
            let tok_after = tokens::est_str(&after_text);
            return vec![StatRec {
                tool: target.tool,
                before: orig_before,
                after,
                ts: 0,
                tok_before: Some(tok_before),
                tok_after: Some(tok_after),
                // upstream_ms and level are stamped by proxy.rs after the
                // upstream call returns; leave them at their defaults here.
                upstream_ms: 0,
                level: String::new(),
            }];
        }
    }
    if sv.mutated || sv.any() {
        // After compression: capture the post-compress text for token estimation.
        let after_text = cache::content_text(result);
        let tok_after = tokens::est_str(&after_text);
        let mut recs = vec![StatRec {
            tool: target.tool.clone(),
            before: sv.before,
            after: sv.after,
            ts: 0,
            tok_before: Some(tok_before),
            tok_after: Some(tok_after),
            // upstream_ms and level are stamped by proxy.rs after the
            // upstream call returns; leave them at their defaults here.
            upstream_ms: 0,
            level: String::new(),
        }];
        // Mixed text+image: also emit a volume record for the image bytes so the
        // ledger captures the full response size. The image record has
        // before==after==image_bytes and tok_before==tok_after==Some(0) so that:
        //   (a) it never inflates the "tokens saved" figure (vision tokens are
        //       priced on pixels, not byte/4), and
        //   (b) print_gain can detect it as a volume record and print the note.
        if sv.image_bytes > 0 {
            recs.push(StatRec {
                tool: target.tool,
                before: sv.image_bytes,
                after: sv.image_bytes,
                ts: 0,
                tok_before: Some(0),
                tok_after: Some(0),
                upstream_ms: 0,
                level: String::new(),
            });
        }
        recs
    } else if sv.image_bytes > 0 {
        // Image-only (or image + already-compact text): no text savings, but
        // we still emit a StatRec so the ledger tracks screenshot volume.
        // before==after==image_bytes so the record shows 0 bytes saved.
        // Token fields are set EQUAL (both 0) so that
        //   tok_before.saturating_sub(tok_after) == 0
        // and print_gain never shows a bogus per-tool token figure derived
        // from dividing raw base64 bytes by 4 (vision tokens are priced on
        // pixels, not text-token equivalents).
        vec![StatRec {
            tool: target.tool,
            before: sv.image_bytes,
            after: sv.image_bytes,
            ts: 0,
            tok_before: Some(0),
            tok_after: Some(0),
            upstream_ms: 0,
            level: String::new(),
        }]
    } else {
        vec![]
    }
}

/// Split an SSE body into event blocks, recognising `\r\n\r\n`, `\n\n`, and
/// `\r\r` as block separators (RFC 7230 / text/event-stream). Each returned
/// slice is a borrow of the original `body`; the separator bytes are NOT
/// included. We deliberately do NOT globally replace `\r\n` → `\n` first,
/// because non-target blocks must be forwarded verbatim (replacing line
/// endings would mutate them).
fn split_sse_blocks(body: &str) -> Vec<&str> {
    let mut blocks: Vec<&str> = Vec::new();
    let bytes = body.as_bytes();
    let len = bytes.len();
    let mut start = 0;
    let mut i = 0;
    while i < len {
        // \r\n\r\n  (4 bytes)
        if i + 3 < len
            && bytes[i] == b'\r'
            && bytes[i + 1] == b'\n'
            && bytes[i + 2] == b'\r'
            && bytes[i + 3] == b'\n'
        {
            blocks.push(&body[start..i]);
            i += 4;
            start = i;
        // \n\n  (2 bytes)
        } else if i + 1 < len && bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            blocks.push(&body[start..i]);
            i += 2;
            start = i;
        // \r\r  (2 bytes)
        } else if i + 1 < len && bytes[i] == b'\r' && bytes[i + 1] == b'\r' {
            blocks.push(&body[start..i]);
            i += 2;
            start = i;
        } else {
            i += 1;
        }
    }
    // Remainder after the last separator (may be empty).
    if start <= len {
        blocks.push(&body[start..]);
    }
    blocks
}

/// Detect the block separator style used in a body so we can reconstruct it
/// faithfully when reassembling non-mutated blocks. Returns `"\r\n\r\n"`,
/// `"\r\r"`, or `"\n\n"`.
fn sse_separator(body: &str) -> &'static str {
    let b = body.as_bytes();
    for i in 0..b.len().saturating_sub(3) {
        if b[i] == b'\r' && b[i + 1] == b'\n' && b[i + 2] == b'\r' && b[i + 3] == b'\n' {
            return "\r\n\r\n";
        }
    }
    for i in 0..b.len().saturating_sub(1) {
        if b[i] == b'\r' && b[i + 1] == b'\r' {
            return "\r\r";
        }
    }
    "\n\n"
}

/// Transform an SSE (`text/event-stream`) body. Each event block is preserved;
/// only the `data:` payload of messages addressed to a target tool is rewritten.
pub fn transform_sse(
    body: &str,
    ids: &HashMap<String, Target>,
    level: Level,
    filters: &FilterSet,
    cache: Option<&DeltaCache>,
) -> (String, Vec<StatRec>) {
    let mut recs = Vec::new();
    let mut out_blocks: Vec<String> = Vec::new();
    let sep = sse_separator(body);

    for block in split_sse_blocks(body) {
        if block.trim().is_empty() {
            out_blocks.push(block.to_string());
            continue;
        }

        let mut prefix_lines: Vec<&str> = Vec::new();
        let mut data_parts: Vec<&str> = Vec::new();
        for line in block.split('\n') {
            let lt = line.trim_end_matches('\r');
            if let Some(rest) = lt.strip_prefix("data:") {
                data_parts.push(rest.strip_prefix(' ').unwrap_or(rest));
            } else {
                prefix_lines.push(line);
            }
        }

        if data_parts.is_empty() {
            out_blocks.push(block.to_string());
            continue;
        }

        let payload = data_parts.join("\n");
        match serde_json::from_str::<Value>(&payload) {
            Ok(mut v) => {
                let r = transform_value(&mut v, ids, level, filters, cache);
                if r.is_empty() {
                    out_blocks.push(block.to_string());
                } else {
                    recs.extend(r);
                    let min = serde_json::to_string(&v).unwrap_or(payload);
                    let mut nb = String::new();
                    for p in &prefix_lines {
                        nb.push_str(p);
                        nb.push('\n');
                    }
                    nb.push_str("data: ");
                    nb.push_str(&min);
                    out_blocks.push(nb);
                }
            }
            Err(_) => out_blocks.push(block.to_string()),
        }
    }

    (out_blocks.join(sep), recs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(tool: &str) -> Target {
        Target {
            tool: tool.to_string(),
            key: format!("{tool}|k"),
        }
    }

    #[test]
    fn is_target_double_namespace_and_negative() {
        // Double-namespaced wire name must be treated as a target.
        assert!(
            is_target("mcp__plugin_figma_figma__get_metadata"),
            "double-namespace must be a target"
        );
        // A name sharing the suffix but lacking __ boundary must NOT be a target.
        assert!(
            !is_target("evil_get_metadata"),
            "no __ boundary must NOT be a target"
        );
    }

    #[test]
    fn extracts_target_call_id() {
        let req = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"get_metadata","arguments":{}}}"#;
        let ids = extract_targets(req);
        assert_eq!(ids.get("7").map(|t| t.tool.as_str()), Some("get_metadata"));
    }

    #[test]
    fn extracts_variable_defs_and_code_connect_targets() {
        let req =
            br#"{"id":1,"method":"tools/call","params":{"name":"get_variable_defs","arguments":{}}}"#;
        assert_eq!(
            extract_targets(req).get("1").map(|t| t.tool.as_str()),
            Some("get_variable_defs")
        );
        // MCP-namespaced wire name still matches via the `__` boundary.
        let req2 = br#"{"id":2,"method":"tools/call","params":{"name":"mcp__plugin_figma_figma__get_code_connect_map","arguments":{}}}"#;
        assert_eq!(
            extract_targets(req2).get("2").map(|t| t.tool.as_str()),
            Some("mcp__plugin_figma_figma__get_code_connect_map")
        );
    }

    #[test]
    fn compresses_variable_defs_result_in_sse() {
        // Real get_variable_defs shape: a flat token->value JSON string in the
        // content text. Adding it to TARGET_TOOLS gets it losslessly minified.
        let mut ids = HashMap::new();
        ids.insert("4".to_string(), target("get_variable_defs"));
        let pretty = "{\n  \"brand/white\": \"#ffffff\",\n  \"spacing/0\": \"0\"\n}";
        let body = format!(
            "event: message\ndata: {{\"id\":4,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}\n\n",
            serde_json::to_string(pretty).unwrap()
        );
        let (out, recs) = transform_sse(&body, &ids, Level::Standard, &FilterSet::default(), None);
        assert_eq!(recs.len(), 1, "metered");
        // The inner JSON is a string value nested in the outer SSE JSON, so its
        // quotes are re-escaped (\") in the data line. The minified form is present
        // and the pretty form (indentation `\n  ` and `": "` spacing) is gone —
        // together these prove minification actually happened (not a no-op match).
        assert!(
            out.contains(r##"{\"brand/white\":\"#ffffff\",\"spacing/0\":\"0\"}"##),
            "minified"
        );
        assert!(!out.contains(r#"\n  "#), "pretty indentation removed");
        assert!(!out.contains(r#"\": \""#), "colon-space spacing removed");
    }

    #[test]
    fn use_figma_node_tree_is_a_target_and_minified() {
        // use_figma JSON-serializes its return value; read-only discovery calls
        // return large node trees worth minifying (+ delta-cacheable on re-read).
        let req = br#"{"id":9,"method":"tools/call","params":{"name":"use_figma","arguments":{}}}"#;
        assert_eq!(
            extract_targets(req).get("9").map(|t| t.tool.as_str()),
            Some("use_figma")
        );

        let mut ids = HashMap::new();
        ids.insert("9".to_string(), target("use_figma"));
        let pretty = "{\n  \"id\": \"136:2\",\n  \"type\": \"FRAME\"\n}";
        let body = format!(
            "event: message\ndata: {{\"id\":9,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}\n\n",
            serde_json::to_string(pretty).unwrap()
        );
        let (out, recs) = transform_sse(&body, &ids, Level::Standard, &FilterSet::default(), None);
        assert_eq!(recs.len(), 1, "metered");
        // Order-independent: fields preserved + pretty indentation gone (minified).
        assert!(out.contains(r#"\"id\":\"136:2\""#), "id field preserved");
        assert!(
            out.contains(r#"\"type\":\"FRAME\""#),
            "type field preserved"
        );
        assert!(
            !out.contains(r#"\n  "#),
            "pretty indentation removed (minified)"
        );
    }

    #[test]
    fn ignores_non_target_tools() {
        let req = br#"{"id":1,"method":"tools/call","params":{"name":"whoami"}}"#;
        assert!(extract_targets(req).is_empty());
    }

    #[test]
    fn compresses_matching_result_in_sse() {
        let mut ids = HashMap::new();
        ids.insert("3".to_string(), target("get_metadata"));
        let pretty = "<a>\n    <b/>\n</a>";
        let body = format!(
            "event: message\ndata: {{\"id\":3,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}\n\n",
            serde_json::to_string(pretty).unwrap()
        );
        let (out, recs) = transform_sse(&body, &ids, Level::Standard, &FilterSet::default(), None);
        assert_eq!(recs.len(), 1);
        assert!(out.contains("<a><b/></a>"));
        assert!(out.starts_with("event: message"));
    }

    #[test]
    fn delta_cache_collapses_identical_reread_in_sse() {
        let cache = DeltaCache::new(8);
        let mut ids = HashMap::new();
        ids.insert("3".to_string(), target("get_metadata"));
        let body = "event: message\ndata: {\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"<a><b/></a>\"}]}}\n\n";
        let (first, _) = transform_sse(
            body,
            &ids,
            Level::Standard,
            &FilterSet::default(),
            Some(&cache),
        );
        assert!(first.contains("<a><b/></a>"), "first read passes through");
        let (second, _) = transform_sse(
            body,
            &ids,
            Level::Standard,
            &FilterSet::default(),
            Some(&cache),
        );
        assert!(
            second.contains("Unchanged"),
            "identical re-read collapsed to sentinel"
        );
        assert!(!second.contains("<a><b/></a>"));
    }

    // ----- CORR-1: CRLF SSE block splitting -----

    #[test]
    fn split_sse_blocks_recognizes_crlf_lf_and_cr() {
        // \r\n\r\n  -- standard HTTP/1.1 SSE over TLS
        let crlf = "A\r\nB\r\n\r\nC\r\nD";
        let parts = split_sse_blocks(crlf);
        assert_eq!(parts, vec!["A\r\nB", "C\r\nD"], "CRLF blocks");

        // \n\n  -- bare LF (most test/debug scenarios)
        let lf = "A\nB\n\nC\nD";
        let parts = split_sse_blocks(lf);
        assert_eq!(parts, vec!["A\nB", "C\nD"], "LF blocks");

        // \r\r  -- bare CR (rarely used but spec-valid)
        let cr = "A\rB\r\rC\rD";
        let parts = split_sse_blocks(cr);
        assert_eq!(parts, vec!["A\rB", "C\rD"], "CR blocks");

        // Empty body
        assert_eq!(split_sse_blocks(""), vec![""]);
        // No separator: single block
        assert_eq!(split_sse_blocks("no sep"), vec!["no sep"]);
    }

    #[test]
    fn crlf_sse_body_gets_compressed_and_metered() {
        // A CRLF-delimited SSE stream must be parsed, compressed, and emit a
        // StatRec — previously the whole body was one block -> forwarded uncompressed.
        let mut ids = HashMap::new();
        ids.insert("5".to_string(), target("get_metadata"));
        let pretty = "{\n  \"a\": 1,\n  \"b\": 2\n}";
        // Build a CRLF SSE stream: event block + trailing CRLF-block separator
        let data_json = serde_json::to_string(&serde_json::json!({
            "id": 5,
            "result": {
                "content": [{"type": "text", "text": pretty}]
            }
        }))
        .unwrap();
        let body = format!("event: message\r\ndata: {data_json}\r\n\r\n");
        let (out, recs) = transform_sse(&body, &ids, Level::Standard, &FilterSet::default(), None);
        assert_eq!(recs.len(), 1, "must be metered — CRLF blocks now parsed");
        // The output must be reassembled with CRLF separators (verbatim pass-through
        // of non-target blocks and correct block rejoining).
        assert!(
            out.contains("\r\n\r\n") || out.ends_with("\r\n"),
            "CRLF separator preserved"
        );
        // Minified JSON present (pretty-printed form gone). The inner text value is
        // JSON-escaped inside the outer data: line, so check for the escaped form.
        assert!(
            out.contains(r#"{\"a\":1,\"b\":2}"#),
            "content minified (escaped in wire)"
        );
    }

    // ----- R0-3: new TARGET_TOOLS and image-block metering -----

    #[test]
    fn get_screenshot_is_a_target_and_produces_stat_rec() {
        // get_screenshot must be in TARGET_TOOLS (matched via __ boundary) and
        // produce a StatRec when the response contains an image block.
        let req =
            br#"{"id":10,"method":"tools/call","params":{"name":"get_screenshot","arguments":{}}}"#;
        assert_eq!(
            extract_targets(req).get("10").map(|t| t.tool.as_str()),
            Some("get_screenshot"),
            "get_screenshot must be extracted as a target"
        );

        // Also verify namespaced wire name matches.
        let req2 = br#"{"id":11,"method":"tools/call","params":{"name":"mcp__plugin_figma_figma__get_screenshot","arguments":{}}}"#;
        assert_eq!(
            extract_targets(req2).get("11").map(|t| t.tool.as_str()),
            Some("mcp__plugin_figma_figma__get_screenshot"),
            "namespaced get_screenshot must be a target"
        );

        // An image-only response: StatRec emitted, before==after==image_bytes, saved==0.
        let mut ids = HashMap::new();
        ids.insert("10".to_string(), target("get_screenshot"));
        let img_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let body = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&serde_json::json!({
                "id": 10,
                "result": {
                    "content": [{"type": "image", "data": img_data, "mediaType": "image/png"}]
                }
            }))
            .unwrap()
        );
        let (_out, recs) = transform_sse(&body, &ids, Level::Standard, &FilterSet::default(), None);
        assert_eq!(recs.len(), 1, "StatRec must be emitted for image block");
        let rec = &recs[0];
        let img_bytes = img_data.len();
        assert_eq!(rec.before, img_bytes, "before == image_bytes");
        assert_eq!(rec.after, img_bytes, "after == image_bytes (no savings)");
        // Token fields must be explicitly equal so no fake savings are shown.
        assert_eq!(
            rec.tok_before, rec.tok_after,
            "tok_before == tok_after for image records"
        );
    }

    #[test]
    fn get_context_for_code_connect_is_a_target() {
        let req = br#"{"id":20,"method":"tools/call","params":{"name":"get_context_for_code_connect","arguments":{}}}"#;
        assert_eq!(
            extract_targets(req).get("20").map(|t| t.tool.as_str()),
            Some("get_context_for_code_connect"),
            "get_context_for_code_connect must be a target"
        );
    }

    #[test]
    fn get_code_connect_suggestions_is_a_target() {
        let req = br#"{"id":21,"method":"tools/call","params":{"name":"get_code_connect_suggestions","arguments":{}}}"#;
        assert_eq!(
            extract_targets(req).get("21").map(|t| t.tool.as_str()),
            Some("get_code_connect_suggestions"),
            "get_code_connect_suggestions must be a target"
        );
    }

    #[test]
    fn image_block_stat_rec_has_zero_token_savings() {
        // An image block StatRec must show zero tokens saved (tok_before == tok_after)
        // so that print_gain does not print a bogus per-tool token figure.
        let mut ids = HashMap::new();
        ids.insert("30".to_string(), target("get_screenshot"));
        let img_data = "abc123base64data";
        let body = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&serde_json::json!({
                "id": 30,
                "result": {
                    "content": [{"type": "image", "data": img_data}]
                }
            }))
            .unwrap()
        );
        let (_out, recs) = transform_sse(&body, &ids, Level::Standard, &FilterSet::default(), None);
        assert_eq!(recs.len(), 1, "StatRec emitted");
        let rec = &recs[0];
        // tok_before and tok_after must be equal (both 0 or both the same non-zero value)
        // so that tok_before.saturating_sub(tok_after) == 0.
        let tb = rec.tok_before.unwrap_or(0);
        let ta = rec.tok_after.unwrap_or(0);
        assert_eq!(tb, ta, "no token savings for image-volume records");
        assert_eq!(tb.saturating_sub(ta), 0, "token saved must be 0");
    }

    #[test]
    fn sse_structural_mutation_drop_meta_image_block() {
        // SSE path: apply_structural drops _meta + non-text (image) blocks are
        // unchanged; the structural mutation must still cause a StatRec to be emitted
        // (CORR-2: mutated flag) and _meta must be absent in the output (CORR-1 path).
        let fs = FilterSet::parse(
            "[[filter]]\nname=\"dc\"\ntools=[\"get_design_context\"]\ndrop_keys=[\"_meta\"]\n",
        )
        .unwrap();
        let mut ids = HashMap::new();
        ids.insert("7".to_string(), target("get_design_context"));
        // Content: one image block only (no text) + _meta on the result envelope.
        // before==0 (content_text_len counts only text blocks), so without the
        // mutated flag, sv.any()==false and transform_msg returns None -> no StatRec.
        let body = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&serde_json::json!({
                "id": 7,
                "result": {
                    "_meta": {"mcpRequestId": "abc"},
                    "content": [{"type": "image", "url": "https://example.com/img.png"}]
                }
            }))
            .unwrap()
        );
        let (out, recs) = transform_sse(&body, &ids, Level::Aggressive, &fs, None);
        // _meta must have been dropped from the forwarded bytes.
        assert!(
            !out.contains("mcpRequestId"),
            "_meta dropped by structural filter"
        );
        // Deterministically 2 records here: one for the structural mutation (the
        // mutated flag), one for the image url volume (url has length > 0).
        assert_eq!(
            recs.len(),
            2,
            "structural-mutation record + image-volume record both emitted"
        );
    }

    // -----------------------------------------------------------------------
    // R2-A: cache_key stability (cross-process determinism via FNV-1a)
    // -----------------------------------------------------------------------

    #[test]
    fn cache_key_is_deterministic_across_calls() {
        let args = serde_json::json!({"nodeId": "1:23", "depth": 2});
        let k1 = cache_key("get_metadata", &args);
        let k2 = cache_key("get_metadata", &args);
        assert_eq!(
            k1, k2,
            "cache_key must be byte-stable across calls (FNV-1a)"
        );
    }

    #[test]
    fn cache_key_differs_for_different_tool_or_args() {
        let args = serde_json::json!({"nodeId": "1:23"});
        let k1 = cache_key("get_metadata", &args);
        let k2 = cache_key("get_design_context", &args);
        assert_ne!(k1, k2, "different tool -> different key");
        let args2 = serde_json::json!({"nodeId": "9:99"});
        let k3 = cache_key("get_metadata", &args2);
        assert_ne!(k1, k3, "different args -> different key");
    }

    // ----- CORR-3: Savings::any() tightened; no spurious StatRec -----

    #[test]
    fn savings_any_false_for_identical_content() {
        // any() must be false when content was not actually shrunk.
        use crate::compress::Savings;
        let s = Savings::new(100, 100); // before == after
        assert!(!s.any(), "any() must be false when content did not shrink");
        let s2 = Savings::new(0, 0); // both zero
        assert!(!s2.any(), "any() false for empty content");
        let s3 = Savings::new(100, 50); // shrank
        assert!(s3.any(), "any() true when after < before");
    }

    #[test]
    fn no_stat_rec_for_already_compact_json() {
        // Already-minified content: compress_result_with must not emit a StatRec
        // (no savings, no mutation) — the proxy invariant comment in proxy.rs.
        let mut ids = HashMap::new();
        ids.insert("1".to_string(), target("get_metadata"));
        // JSON already compact — no whitespace to remove, no filter applied.
        let compact = r#"{"a":1,"b":2}"#;
        let body = format!(
            "event: message\ndata: {{\"id\":1,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}\n\n",
            serde_json::to_string(compact).unwrap()
        );
        let (_out, recs) = transform_sse(&body, &ids, Level::Standard, &FilterSet::default(), None);
        assert!(
            recs.is_empty(),
            "no StatRec for already-compact content (got {:?})",
            recs
        );
    }

    // ----- R0-3 blocker fix: mixed text+image responses must record image volume -----

    #[test]
    fn mixed_text_and_image_image_bytes_reach_ledger() {
        // BLOCKER fix: when a response contains BOTH a compressible text block AND
        // an image block, the image volume must appear in the emitted StatRec.
        // Previously, sv.image_bytes was silently discarded when sv.mutated||sv.any()
        // was true — the else-if branch was never reached.
        let mut ids = HashMap::new();
        ids.insert("40".to_string(), target("get_screenshot"));
        let img_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        // Include a pretty-printed JSON text block (compressible — will shrink) AND
        // an image block. This exercises the `sv.mutated || sv.any()` branch while
        // also having sv.image_bytes > 0.
        let pretty_text = "{\n  \"caption\": \"screenshot\"\n}";
        let body = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&serde_json::json!({
                "id": 40,
                "result": {
                    "content": [
                        {"type": "text", "text": pretty_text},
                        {"type": "image", "data": img_data, "mediaType": "image/png"}
                    ]
                }
            }))
            .unwrap()
        );
        let (_out, recs) = transform_sse(&body, &ids, Level::Standard, &FilterSet::default(), None);
        // Two StatRecs: one for text savings, one for the image volume.
        assert_eq!(
            recs.len(),
            2,
            "separate StatRecs for text savings and image volume"
        );

        // Find the image-volume record: tok_before == Some(0) && tok_after == Some(0)
        // and before == after (by the image-volume invariant).
        let img_rec = recs
            .iter()
            .find(|r| r.tok_before == Some(0) && r.tok_after == Some(0) && r.before == r.after)
            .expect("image-volume StatRec must be emitted");
        let img_len = img_data.len();
        assert_eq!(
            img_rec.before, img_len,
            "image-volume StatRec.before must equal image_bytes ({})",
            img_len
        );

        // The text record: tok_before != tok_after (text was actually compressed).
        let text_rec = recs
            .iter()
            .find(|r| !(r.tok_before == Some(0) && r.tok_after == Some(0) && r.before == r.after))
            .expect("text-savings StatRec must also be emitted");
        assert!(
            text_rec.before > 0,
            "text StatRec.before must be > 0 (text was present)"
        );
    }
}
