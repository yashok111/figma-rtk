//! Conservative, near-lossless compression of tool-result text payloads.
//!
//! v1 deliberately avoids semantic edits that could drop data the agent needs.
//! The transforms only remove redundant whitespace: JSON is re-serialized
//! minified (lossless); XML indentation/newlines between tags are removed
//! (lossless for element-only markup; single spaces in content are kept);
//! other text gets trailing whitespace trimmed and blank-line runs collapsed.
//! Aggressive structural pruning (hidden layers, vector paths, instance dedup)
//! belongs to a later phase, once real payload shapes are confirmed.

use serde_json::Value;

use crate::config::Level;
use crate::filter::FilterSet;

#[derive(Default, Clone, Copy)]
pub struct Savings {
    pub before: usize,
    pub after: usize,
}

impl Savings {
    pub fn new(before: usize, after: usize) -> Self {
        Self {
            before,
            after: after.min(before),
        }
    }
    pub fn any(&self) -> bool {
        self.before > 0
    }
}

/// Compress every text item inside a tool `result` envelope in place, using the
/// near-lossless whitespace pass only (Standard level, no filters).
pub fn compress_result(result: &mut Value) -> Savings {
    compress_result_with(result, "", Level::Standard, &FilterSet::default())
}

/// As [`compress_result`], but at `Aggressive`/`Ultra` level also applies any
/// trusted TOML `filters` matching `tool` (structural field-stripping). Falls
/// back to the whitespace pass when no filter matches or the text isn't JSON.
pub fn compress_result_with(
    result: &mut Value,
    tool: &str,
    level: Level,
    filters: &FilterSet,
) -> Savings {
    // Measured over total `content[].text` bytes so that whole blocks dropped by
    // a structural filter (below) are credited to savings, not silently elided.
    let before = crate::cache::content_text_len(result);
    let aggressive = !matches!(level, Level::Standard);
    let ultra = matches!(level, Level::Ultra);

    // Aggressive/ultra: first apply trusted filters to the result envelope's own
    // structure — this can drop whole `content[]` blocks (e.g. get_design_context
    // boilerplate, whose text is React code, not JSON) and keys like `_meta`. The
    // per-item pass below then compresses whatever text blocks survive.
    if aggressive {
        filters.apply_structural(tool, result);
    }

    if let Some(content) = result.get_mut("content").and_then(|c| c.as_array_mut()) {
        for item in content {
            if item.get("type").and_then(|t| t.as_str()) != Some("text") {
                continue;
            }
            let Some(text) = item.get("text").and_then(|t| t.as_str()) else {
                continue;
            };
            // For a text block that is itself JSON (e.g. get_metadata), a filter
            // strips fields inside it; otherwise fall back to the whitespace pass.
            // At ultra, a non-JSON code block additionally has its Figma-reference
            // attributes (data-node-id / data-name) stripped — lossy, recoverable.
            let compressed = if aggressive {
                match filters.apply_text(tool, text) {
                    Some(filtered) => filtered,
                    None => {
                        let c = compress_text(text);
                        // The node-attr strip only makes sense for get_design_context's
                        // generated JSX; gate it to that tool so a use_figma return that
                        // happens to be a raw HTML/JSX string is never touched.
                        if ultra && crate::filter::matches_tool(tool, "get_design_context") {
                            strip_figma_node_attrs(&c)
                        } else {
                            c
                        }
                    }
                }
            } else {
                compress_text(text)
            };
            if compressed.len() < text.len() {
                item["text"] = Value::String(compressed);
            }
        }
    }

    let after = crate::cache::content_text_len(result);
    Savings::new(before, after)
}

/// Whitespace-only convenience wrapper over [`compress_payload`].
pub fn compress_any(s: &str) -> (String, Savings) {
    compress_payload(s, "", Level::Standard, &FilterSet::default())
}

/// Compress an arbitrary stdin payload for the `frtk compress` harness, with
/// `level` + `filters` applied. Accepts a full JSON-RPC tool-response envelope,
/// a bare `{content:[...]}` result, or raw text/JSON. Savings are measured over
/// the whole payload (bytes in vs bytes out).
pub fn compress_payload(
    s: &str,
    tool: &str,
    level: Level,
    filters: &FilterSet,
) -> (String, Savings) {
    if let Ok(mut v) = serde_json::from_str::<Value>(s) {
        if v.pointer("/result/content").is_some() {
            compress_result_with(v.get_mut("result").unwrap(), tool, level, filters);
            let out = serde_json::to_string(&v).unwrap_or_else(|_| s.to_string());
            let sv = Savings::new(s.len(), out.len());
            return (out, sv);
        }
        if v.get("content").is_some() {
            compress_result_with(&mut v, tool, level, filters);
            let out = serde_json::to_string(&v).unwrap_or_else(|_| s.to_string());
            let sv = Savings::new(s.len(), out.len());
            return (out, sv);
        }
        // Bare JSON (e.g. a captured get_metadata payload): apply filters directly.
        if !matches!(level, Level::Standard) {
            if let Some(filtered) = filters.apply_text(tool, s) {
                let sv = Savings::new(s.len(), filtered.len());
                return (filtered, sv);
            }
        }
    }
    let out = compress_text(s);
    let sv = Savings::new(s.len(), out.len());
    (out, sv)
}

pub fn compress_text(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if let Ok(min) = serde_json::to_string(&v) {
                return min;
            }
        }
    }
    if trimmed.starts_with('<') {
        return strip_xml_indent(s);
    }
    collapse_code_ws(s)
}

/// Drop whitespace runs that sit fully between `>` and `<` and contain a
/// newline (i.e. pretty-printer indentation). Single intra-line spaces and any
/// run holding real text content are left untouched.
fn strip_xml_indent(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        if c == '>' {
            let mut j = i + 1;
            let mut saw_newline = false;
            while j < chars.len() && chars[j].is_whitespace() {
                if chars[j] == '\n' || chars[j] == '\r' {
                    saw_newline = true;
                }
                j += 1;
            }
            if saw_newline && j < chars.len() && chars[j] == '<' && j > i + 1 {
                // Skip the whitespace run entirely.
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn collapse_code_ws(s: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut blank_run = 0u32;
    for raw in s.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push(String::new());
            }
        } else {
            blank_run = 0;
            out.push(line.to_string());
        }
    }
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Ultra-level only. Remove Figma reference attributes (`data-node-id`,
/// `data-name`) from generated JSX. They map a node back to the Figma file but
/// do not affect rendering or carry design tokens, so dropping them is lossy
/// only in that it loses Code Connect traceability — hence ultra-gated, with the
/// raw payload kept recoverable via tee/capture. Matched as ` <attr>="…"` (the
/// leading space plus the value up to the next `"`); the JSON object form
/// `"data-node-id":` uses different syntax and is never matched. Slicing happens
/// only at the ASCII marker / closing-quote, so multibyte values are safe.
fn strip_figma_node_attrs(s: &str) -> String {
    const MARKERS: [&str; 2] = [" data-node-id=\"", " data-name=\""];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let next = MARKERS
            .iter()
            .filter_map(|m| rest.find(*m).map(|i| (i, m.len())))
            .min_by_key(|&(i, _)| i);
        let Some((idx, mlen)) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..idx]);
        let after_marker = &rest[idx + mlen..];
        match find_unescaped_quote(after_marker) {
            Some(qpos) => rest = &after_marker[qpos + 1..],
            None => {
                // Malformed (no closing quote): keep the remainder verbatim.
                out.push_str(&rest[idx..]);
                break;
            }
        }
    }
    out
}

/// Byte offset of the first `"` not preceded by a backslash escape, so an
/// attribute value containing an escaped quote (`\"`) is not truncated early.
/// Byte-scanning is UTF-8 safe here: `"` (0x22) and `\` (0x5C) are ASCII and
/// never occur as continuation bytes of a multibyte char.
fn find_unescaped_quote(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // skip the escaped char (e.g. \")
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_minified_losslessly() {
        let pretty = "{\n  \"a\": 1,\n  \"b\": [1, 2, 3]\n}";
        let out = compress_text(pretty);
        assert_eq!(out, "{\"a\":1,\"b\":[1,2,3]}");
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            serde_json::from_str::<Value>(pretty).unwrap()
        );
    }

    #[test]
    fn xml_indentation_removed_content_kept() {
        let xml = "<frame name=\"Hero\">\n    <text>Button Label</text>\n</frame>";
        let out = compress_text(xml);
        assert_eq!(out, "<frame name=\"Hero\"><text>Button Label</text></frame>");
    }

    #[test]
    fn code_blank_runs_collapse() {
        let code = "const a = 1;   \n\n\n\nconst b = 2;\n";
        let out = compress_text(code);
        assert_eq!(out, "const a = 1;\n\nconst b = 2;");
    }

    #[test]
    fn compress_any_handles_envelope_and_meters_total() {
        let env = "{\n  \"result\": {\n    \"content\": [\n      {\"type\":\"text\",\"text\":\"{\\n  \\\"a\\\": 1\\n}\"}\n    ]\n  }\n}";
        let (out, sv) = compress_any(env);
        assert_eq!(sv.before, env.len());
        assert!(sv.after < sv.before, "envelope shrank");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["content"][0]["text"], Value::String("{\"a\":1}".into()));
    }

    #[test]
    fn compress_any_passes_through_raw_text() {
        let (out, _sv) = compress_any("just some text");
        assert_eq!(out, "just some text");
    }

    #[test]
    fn compress_payload_applies_filters_only_at_aggressive() {
        let fs = FilterSet::parse(
            "[[filter]]\nname=\"x\"\ntools=[\"get_design_context\"]\ndrop_keys=[\"vectorPaths\"]\n",
        )
        .unwrap();
        let env = r#"{"result":{"content":[{"type":"text","text":"{\"vectorPaths\":[1,2],\"name\":\"keep\"}"}]}}"#;

        let (agg, sv) = compress_payload(env, "get_design_context", Level::Aggressive, &fs);
        assert!(!agg.contains("vectorPaths"), "filter drops key at aggressive");
        assert!(agg.contains("keep"));
        assert!(sv.after < sv.before);

        let (std, _) = compress_payload(env, "get_design_context", Level::Standard, &fs);
        assert!(std.contains("vectorPaths"), "standard must not apply filters");
    }

    #[test]
    fn strip_figma_node_attrs_removes_both_attrs_keeps_rest() {
        let jsx = r#"<div className="x" data-node-id="136:2" data-name="Hero — FullHD (1920×1080)"><p data-node-id="136:6">Привет</p></div>"#;
        let out = strip_figma_node_attrs(jsx);
        assert_eq!(
            out,
            r#"<div className="x"><p>Привет</p></div>"#,
            "node-id + data-name removed (incl. leading space), unicode content kept"
        );
    }

    #[test]
    fn strip_figma_node_attrs_handles_escaped_quote_in_value() {
        // A node name with an escaped quote must not truncate the attribute early.
        let jsx = r#"<p data-name="say \"hi\" now" data-node-id="1:2">x</p>"#;
        assert_eq!(strip_figma_node_attrs(jsx), "<p>x</p>");
    }

    #[test]
    fn strip_figma_node_attrs_ignores_json_form_and_plain_text() {
        // JSON object syntax must NOT be touched (different from JSX attr syntax).
        let json = r#"{"data-node-id":"1:2","data-name":"keep"}"#;
        assert_eq!(strip_figma_node_attrs(json), json);
        // No attributes present -> unchanged.
        assert_eq!(strip_figma_node_attrs("const x = 1;"), "const x = 1;");
    }

    #[test]
    fn compress_result_with_strips_node_attrs_only_at_ultra() {
        let mk = || -> Value {
            serde_json::from_str(
                r#"{"content":[{"type":"text","text":"const a = 1;\n<div className=\"c\" data-node-id=\"1:2\" data-name=\"H\">hi</div>"}]}"#,
            )
            .unwrap()
        };
        let fs = FilterSet::default();

        // Ultra: Figma-ref attrs stripped.
        let mut ultra = mk();
        let sv = compress_result_with(&mut ultra, "get_design_context", Level::Ultra, &fs);
        let t = ultra["content"][0]["text"].as_str().unwrap();
        assert!(!t.contains("data-node-id"), "ultra strips data-node-id");
        assert!(!t.contains("data-name"), "ultra strips data-name");
        assert!(t.contains("className=\"c\""), "className kept");
        assert!(t.contains(">hi<"), "content kept");
        assert!(sv.after < sv.before, "ultra reports savings");

        // Aggressive: attrs kept (only TOML filters + whitespace apply).
        let mut agg = mk();
        compress_result_with(&mut agg, "get_design_context", Level::Aggressive, &fs);
        assert!(
            agg["content"][0]["text"].as_str().unwrap().contains("data-node-id"),
            "aggressive keeps node ids"
        );

        // Standard: attrs kept.
        let mut std = mk();
        compress_result_with(&mut std, "get_design_context", Level::Standard, &fs);
        assert!(
            std["content"][0]["text"].as_str().unwrap().contains("data-name"),
            "standard keeps data-name"
        );
    }

    #[test]
    fn compress_result_with_drops_boilerplate_blocks_at_aggressive() {
        // Mirrors a get_design_context result: code block + 3 boilerplate
        // instruction blocks + 1 design-token block + _meta.
        let fs = FilterSet::parse(
            "[[filter]]\nname=\"dc\"\ntools=[\"get_design_context\"]\n\
             drop_keys=[\"_meta\"]\n\
             drop_where=[\
               {key=\"text\",starts_with=\"SUPER CRITICAL\"},\
               {key=\"text\",starts_with=\"Node ids have been added\"},\
               {key=\"text\",starts_with=\"Images and SVGs will be stored\"}]\n",
        )
        .unwrap();
        let mk = || -> Value {
            serde_json::from_str(
                r#"{"_meta":{"mcpRequestId":"abc"},"content":[
                  {"type":"text","text":"export default function Hero() { return null; }"},
                  {"type":"text","text":"SUPER CRITICAL: The generated React+Tailwind code MUST be converted to match the target project."},
                  {"type":"text","text":"Node ids have been added to the code as data attributes, e.g. `data-node-id=\"1:2\"`."},
                  {"type":"text","text":"These styles are contained in the design: display/sub: Font(family: \"Unbounded\")."},
                  {"type":"text","text":"Images and SVGs will be stored as constants, e.g. const image = '...'."}
                ]}"#,
            )
            .unwrap()
        };

        // Aggressive: boilerplate blocks + _meta dropped, code + styles kept.
        let mut agg = mk();
        let sv = compress_result_with(&mut agg, "get_design_context", Level::Aggressive, &fs);
        let out = serde_json::to_string(&agg).unwrap();
        assert!(!out.contains("SUPER CRITICAL"), "instruction block dropped");
        assert!(!out.contains("Node ids have been added"), "node-id block dropped");
        assert!(!out.contains("Images and SVGs will be stored"), "images block dropped");
        assert!(!out.contains("mcpRequestId"), "_meta dropped");
        assert!(out.contains("export default function Hero"), "code block kept");
        assert!(out.contains("These styles are contained"), "design-token block kept");
        // Savings must credit the dropped boilerplate, not just per-item whitespace.
        assert!(sv.after < sv.before, "reports savings");
        assert!(
            sv.before - sv.after > 200,
            "dropped boilerplate counted in savings (got {} -> {})",
            sv.before,
            sv.after
        );

        // Standard: filters do not apply — every block survives untouched.
        let mut std = mk();
        let _ = compress_result_with(&mut std, "get_design_context", Level::Standard, &fs);
        let out_std = serde_json::to_string(&std).unwrap();
        assert!(out_std.contains("SUPER CRITICAL"), "standard keeps boilerplate");
        assert!(out_std.contains("mcpRequestId"), "standard keeps _meta");
    }

    #[test]
    fn compress_result_rewrites_text() {
        let mut v: Value = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"{\n  \"x\": 1\n}"}]}"#,
        )
        .unwrap();
        let sv = compress_result(&mut v);
        assert!(sv.before > sv.after);
        assert_eq!(v["content"][0]["text"], Value::String("{\"x\":1}".into()));
    }
}
