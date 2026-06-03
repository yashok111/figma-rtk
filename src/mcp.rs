//! Minimal JSON-RPC / MCP awareness: correlate tools/call requests with their
//! responses by id, and rewrite the responses of the heavy read tools.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use serde_json::Value;

use crate::cache::{self, DeltaCache};
use crate::compress;
use crate::config::Level;
use crate::filter::FilterSet;
use crate::stats::StatRec;

/// Tools whose responses are worth compressing. Matched against the wire tool
/// name (Figma's own names, e.g. "get_design_context").
pub const TARGET_TOOLS: &[&str] = &[
    "get_design_context",
    "get_metadata",
    // Flat design-token map (token -> value); losslessly minified + delta-cached
    // (tokens rarely change, so re-reads of the same node collapse to a sentinel).
    "get_variable_defs",
    // node -> { codeConnectSrc, codeConnectName } map. Added blind (capture needs
    // a Figma Developer/Enterprise seat); the generic JSON compression path applies.
    "get_code_connect_map",
    // The general-purpose write/inspect tool: it JSON-serializes its return value,
    // and read-only discovery calls (findAll / children dumps) return large node
    // trees. Minified losslessly + delta-cached; write returns (small id lists)
    // compress harmlessly.
    "use_figma",
];

fn is_target(name: &str) -> bool {
    // Exact wire name, or MCP-namespaced (…__get_metadata). The `__` boundary
    // stops unrelated names like "evil_get_metadata" being treated as targets.
    TARGET_TOOLS
        .iter()
        .any(|t| name == *t || name.ends_with(&format!("__{t}")))
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
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(args).unwrap_or_default().hash(&mut h);
    format!("{tool}|{:x}", h.finish())
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
            let args = it.pointer("/params/arguments").cloned().unwrap_or(Value::Null);
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
                if let Some(r) = transform_msg(m, ids, level, filters, cache) {
                    recs.push(r);
                }
            }
        }
        other => {
            if let Some(r) = transform_msg(other, ids, level, filters, cache) {
                recs.push(r);
            }
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
) -> Option<StatRec> {
    let id = msg.get("id")?;
    let target = ids.get(&id_to_string(id))?.clone();
    let result = msg.get_mut("result")?;
    // Original upstream content size, before compression — used for the delta
    // sentinel's "bytes elided" figure and its savings record.
    let orig_before = cache::content_text_len(result);
    let sv = compress::compress_result_with(result, &target.tool, level, filters);
    // Delta cache: collapse a byte-identical re-read to a sentinel.
    if let Some(cache) = cache {
        if let Some(after) = cache::apply_delta(cache, &target.key, &target.tool, result, orig_before)
        {
            return Some(StatRec {
                tool: target.tool,
                before: orig_before,
                after,
                ts: 0,
            });
        }
    }
    if sv.any() {
        Some(StatRec {
            tool: target.tool,
            before: sv.before,
            after: sv.after,
            ts: 0,
        })
    } else {
        None
    }
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

    for block in body.split("\n\n") {
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

    (out_blocks.join("\n\n"), recs)
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
        assert!(out.contains(r##"{\"brand/white\":\"#ffffff\",\"spacing/0\":\"0\"}"##), "minified");
        assert!(!out.contains(r#"\n  "#), "pretty indentation removed");
        assert!(!out.contains(r#"\": \""#), "colon-space spacing removed");
    }

    #[test]
    fn use_figma_node_tree_is_a_target_and_minified() {
        // use_figma JSON-serializes its return value; read-only discovery calls
        // return large node trees worth minifying (+ delta-cacheable on re-read).
        let req =
            br#"{"id":9,"method":"tools/call","params":{"name":"use_figma","arguments":{}}}"#;
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
        assert!(out.contains(r#"\"type\":\"FRAME\""#), "type field preserved");
        assert!(!out.contains(r#"\n  "#), "pretty indentation removed (minified)");
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
        let (first, _) = transform_sse(body, &ids, Level::Standard, &FilterSet::default(), Some(&cache));
        assert!(first.contains("<a><b/></a>"), "first read passes through");
        let (second, _) = transform_sse(body, &ids, Level::Standard, &FilterSet::default(), Some(&cache));
        assert!(second.contains("Unchanged"), "identical re-read collapsed to sentinel");
        assert!(!second.contains("<a><b/></a>"));
    }
}
