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
    /// True when `apply_structural` or `apply_text` mutated the value (e.g.
    /// dropped `_meta` or a boilerplate block), even if `content[].text`
    /// byte lengths happen to be unchanged (e.g. non-text content blocks only).
    pub mutated: bool,
}

impl Savings {
    pub fn new(before: usize, after: usize) -> Self {
        Self {
            before,
            after: after.min(before),
            mutated: false,
        }
    }
    /// True when compression actually reduced content bytes.
    pub fn any(&self) -> bool {
        self.after < self.before
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
    let structural_mutated = if aggressive {
        filters.apply_structural(tool, result)
    } else {
        false
    };

    let mut text_mutated = false;
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
            // Track whether this block's compressed value came from apply_text
            // (a semantic filter), so we can set text_mutated only when the
            // filtered result is actually written back (length guard below).
            let mut from_filter = false;
            let compressed = if aggressive {
                match filters.apply_text(tool, text) {
                    Some(filtered) => {
                        from_filter = true;
                        filtered
                    }
                    None => {
                        let c = compress_text(text);
                        // These transforms only make sense for get_design_context's
                        // generated JSX; gate them to that tool so a use_figma return
                        // that happens to be a raw HTML/JSX string is never touched.
                        // Strip Figma-ref attrs, then dedent the JSX (both lossy only
                        // re: Code Connect traceability / formatting — rendering is
                        // preserved; raw stays recoverable via tee/capture).
                        if ultra && crate::filter::matches_tool(tool, "get_design_context") {
                            if is_figma_node_tree(&c) {
                                // Sparse node-tree dump (a section/frame's metadata,
                                // not React): reduce each element to `<tag id="…">`.
                                strip_node_tree_attrs(&c)
                            } else {
                                compress_jsx_code(&strip_figma_node_attrs(&c))
                            }
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
                // apply_text ran a filter and the filtered result was actually
                // written back. Only set text_mutated here, inside the length guard,
                // so the flag reflects a real change to the forwarded bytes.
                if from_filter {
                    text_mutated = true;
                }
            }
        }
    }

    let after = crate::cache::content_text_len(result);
    let mut sv = Savings::new(before, after);
    // A structural filter may have mutated the envelope (dropped _meta, a whole
    // content block, etc.) even when before==0 (no text content) or the text byte
    // count did not decrease. A text filter (apply_text) similarly rewrites a
    // content[].text field even if the output happens to be the same byte length.
    // Track both separately so the caller can still emit a StatRec and forward
    // the mutated bytes.
    if structural_mutated || text_mutated {
        sv.mutated = true;
    }
    sv
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

/// Ultra-level only, get_design_context JSX. Removes the pretty-printer's leading
/// indentation from each line — rendering-insignificant for JSX/JS (it only changes
/// formatting, never the rendered output or any token/attribute/text). Lines inside
/// an open backtick template literal are left verbatim, because leading whitespace
/// there IS part of the string value. Only leading whitespace is ever removed, so
/// stripping all whitespace from the input and output yields identical strings.
///
/// On a template's *opening* line the guard is still false at trim time, but that
/// is correct: a line's leading whitespace is always code indentation *before* the
/// first backtick — the template value only begins at/after the backtick, which
/// `trim_start` (leading-only) never reaches. The guard can be mis-toggled by a
/// stray backtick in a double-quoted string or comment, but only ever toward
/// OVER-keeping indentation (a real template cannot contain an unescaped backtick —
/// it would close the template), so the failure direction is safe (less savings),
/// never corruption.
fn compress_jsx_code(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut in_template = false;
    for line in s.lines() {
        out.push(if in_template { line } else { line.trim_start() });
        if has_odd_unescaped_backticks(line) {
            in_template = !in_template;
        }
    }
    out.join("\n")
}

/// Detect get_design_context's sparse node-tree payload — a section/frame dump of
/// `<tag id="N:M" name=… x=… y=… width=… height=…/>` elements — so it can be told
/// apart from generated JSX (which `compress_jsx_code`/`strip_figma_node_attrs`
/// handle). The discriminator is the Figma node-id form `id="N:M"` plus bare
/// geometry attributes: real JSX/SVG carries `x=`/`width=` (inline SVG does) but
/// its `id` — when present — is an author string (`icon`, `clip0`), never
/// digits-colon-digits. Requiring `N:M` is what prevents a false positive that
/// would route real code through `strip_node_tree_attrs` and gut it. The signature
/// is matched anywhere in the payload (not just the first `>`), so a `>` inside the
/// root element's `name` value cannot defeat detection.
fn is_figma_node_tree(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with('<') && has_figma_id(t) && t.contains(" x=\"") && t.contains(" width=\"")
}

/// True if `s` carries a Figma node id — the first ` id="…"` whose value is `N:M`
/// (one-or-more digits, a colon, then a digit). Generated JSX/SVG ids never take
/// this shape, so it cleanly separates a node-tree from real code.
fn has_figma_id(s: &str) -> bool {
    let Some(pos) = s.find(" id=\"") else {
        return false;
    };
    let val = &s[pos + 5..];
    let Some(qend) = val.find('"') else {
        return false;
    };
    let Some((a, b)) = val[..qend].split_once(':') else {
        return false;
    };
    !a.is_empty()
        && a.bytes().all(|c| c.is_ascii_digit())
        && b.bytes().next().is_some_and(|c| c.is_ascii_digit())
}

/// Ultra-level only, get_design_context sparse node-tree. Reduces each element to
/// `<tag id="N:M">` / `<tag id="N:M"/>`, dropping every other attribute (name, x,
/// y, width, height, hidden, …). `id` is kept because the accompanying guidance
/// block tells the agent to drill into sub-nodes *by id*; the tag name and nesting
/// are kept so the structure survives. Quote-aware: an attribute value holding a
/// `>` or an escaped quote never ends a tag early. Lossy (geometry/names gone),
/// hence ultra-gated, with the raw payload recoverable via tee/capture. Byte-scan
/// is UTF-8 safe — every slice index lands on an ASCII delimiter (`<>"=/`, space).
fn strip_node_tree_attrs(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        // Inter-tag text (none in practice, but preserved if present).
        if bytes[i] != b'<' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        // Closing tag `</…>`: copy verbatim.
        if bytes.get(i + 1) == Some(&b'/') {
            let start = i;
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // include '>'
            }
            out.push_str(&s[start..i]);
            continue;
        }
        // Opening / self-closing tag: read the tag name.
        let name_start = i + 1;
        let mut j = name_start;
        while j < bytes.len()
            && !bytes[j].is_ascii_whitespace()
            && bytes[j] != b'>'
            && bytes[j] != b'/'
        {
            j += 1;
        }
        let name = &s[name_start..j];
        if name.is_empty() {
            // Not a real tag (a stray '<', or '<' followed by a delimiter): keep the
            // '<' verbatim rather than synthesising a spurious "<>" element.
            out.push('<');
            i += 1;
            continue;
        }
        // Scan attributes (quote-aware): capture `id`, detect self-close, find end.
        let mut id_val: Option<&str> = None;
        let mut self_close = false;
        let mut k = j;
        loop {
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            match bytes.get(k) {
                None => break,
                Some(b'>') => {
                    k += 1;
                    break;
                }
                Some(b'/') => {
                    if bytes.get(k + 1) == Some(&b'>') {
                        self_close = true;
                        k += 2;
                        break;
                    }
                    k += 1; // stray slash
                }
                Some(_) => {
                    let an_start = k;
                    while k < bytes.len()
                        && bytes[k] != b'='
                        && bytes[k] != b'>'
                        && !bytes[k].is_ascii_whitespace()
                    {
                        k += 1;
                    }
                    let attr = &s[an_start..k];
                    if bytes.get(k) == Some(&b'=') && bytes.get(k + 1) == Some(&b'"') {
                        let val_start = k + 2;
                        match find_unescaped_quote(&s[val_start..]) {
                            Some(rel) => {
                                if attr == "id" {
                                    id_val = Some(&s[val_start..val_start + rel]);
                                }
                                k = val_start + rel + 1;
                            }
                            None => k = bytes.len(), // malformed: stop
                        }
                    } else if an_start == k {
                        k += 1; // made no progress: advance to avoid a stall
                    }
                }
            }
        }
        out.push('<');
        out.push_str(name);
        if let Some(v) = id_val {
            out.push_str(" id=\"");
            out.push_str(v);
            out.push('"');
        }
        out.push_str(if self_close { "/>" } else { ">" });
        i = k;
    }
    out
}

/// Whether a line has an odd number of unescaped backticks (so it opens or closes
/// a multiline template literal). Byte-scan is UTF-8 safe: `` ` `` (0x60) and `\`
/// (0x5C) are ASCII and never occur as multibyte continuation bytes.
fn has_odd_unescaped_backticks(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut count = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // skip the escaped char (e.g. \`)
            b'`' => {
                count += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    count % 2 == 1
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

    /// Whitespace-only invariant: removing ALL whitespace from input and output
    /// must give identical strings — proves the transform touched only whitespace.
    fn ws_stripped(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn compress_jsx_code_dedents_preserves_content_and_template() {
        let code = "function H() {\n  return (\n    <div className=\"a b\">\n      <p>Привет — мир</p>\n      <p>{`keep  these  spaces`}</p>\n    </div>\n  );\n}";
        let out = compress_jsx_code(code);
        // leading indentation removed
        assert!(out.contains("\n<div className=\"a b\">"), "div dedented");
        assert!(out.contains("\n<p>Привет — мир</p>"), "text line dedented, content intact");
        // significant template spaces preserved verbatim
        assert!(out.contains("{`keep  these  spaces`}"), "template spaces preserved");
        // INVARIANT: only whitespace was touched
        assert_eq!(ws_stripped(code), ws_stripped(&out), "no non-whitespace changed");
        assert!(out.len() < code.len(), "smaller");
    }

    #[test]
    fn compress_jsx_code_preserves_multiline_template_indentation() {
        // A backtick template spanning lines: its inner indentation is significant
        // and must NOT be stripped.
        let code = "const x = `\n    indented inside template\n`;\n<div>\n  <p>hi</p>\n</div>";
        let out = compress_jsx_code(code);
        assert!(out.contains("\n    indented inside template\n"), "template indent kept");
        assert!(out.contains("\n<div>"), "non-template indent stripped");
        assert_eq!(ws_stripped(code), ws_stripped(&out));
    }

    #[test]
    fn compress_jsx_code_opening_template_line_keeps_value_spaces_drops_code_indent() {
        // The opening line is indented (code) AND the template value begins with
        // spaces right after the backtick. trim_start must drop the code indent but
        // never the post-backtick value spaces (they are not "leading").
        let code = "    const t = `  value spaces\n      more`;\n    return t;";
        let out = compress_jsx_code(code);
        assert!(
            out.starts_with("const t = `  value spaces"),
            "code indent dropped, post-backtick value spaces kept: {out:?}"
        );
        assert!(out.contains("\n      more`;"), "template-interior line verbatim");
        assert!(out.contains("\nreturn t;"), "trailing code dedented");
    }

    #[test]
    fn has_odd_unescaped_backticks_counts_correctly() {
        assert!(has_odd_unescaped_backticks("const x = `"));
        assert!(!has_odd_unescaped_backticks("const x = `y`;"));
        assert!(!has_odd_unescaped_backticks(r"a \` b"), "escaped backtick not counted");
        assert!(has_odd_unescaped_backticks("`"));
        assert!(!has_odd_unescaped_backticks("no ticks here"));
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

    #[test]
    fn is_figma_node_tree_detects_nodetree_not_code() {
        // Real sparse node-tree: detected via the id+geometry signature.
        assert!(is_figma_node_tree(
            r#"<section id="1:1" name="A" x="0" y="0" width="10" height="10"><text id="1:2" name="T" x="1" y="1" width="2" height="2" /></section>"#
        ));
        // Generated JSX (starts with const/export): not a node-tree.
        assert!(!is_figma_node_tree(
            "const a = \"u\";\nexport default function C() { return <div className=\"x\" />; }"
        ));
        // A bare HTML/JSX <section> uses className, no bare geometry: not a node-tree.
        assert!(!is_figma_node_tree(
            r#"<section className="hero"><p>hi</p></section>"#
        ));
        // JSX div with Figma data-attrs is not a node-tree (no bare x=/width=).
        assert!(!is_figma_node_tree(
            r#"<div className="c" data-node-id="1:2" data-name="H">hi</div>"#
        ));
    }

    #[test]
    fn strip_node_tree_attrs_keeps_id_and_tag_only() {
        // Drops name/x/y/width/height/hidden; keeps id + tag + nesting + self-close.
        // `name="A > B"` carries a '>' to prove the tag scan is quote-aware.
        let nt = r#"<section id="1:1" name="Создание" x="-4080" y="-1260" width="6540" height="100"><text id="1:2" name="Title" x="57" y="321" width="246" height="22" /><frame id="1:3" name="A > B" x="0" y="0" width="1" height="1" hidden="true"><vector id="1:4" name="v" x="0" y="0" width="1" height="1" /></frame></section>"#;
        let out = strip_node_tree_attrs(nt);
        assert_eq!(
            out,
            r#"<section id="1:1"><text id="1:2"/><frame id="1:3"><vector id="1:4"/></frame></section>"#
        );
        // Invariant: never grows.
        assert!(out.len() < nt.len());
    }

    #[test]
    fn compress_result_with_strips_node_tree_only_at_ultra_for_gdc() {
        let mk = || -> Value {
            serde_json::from_str(
                r#"{"content":[{"type":"text","text":"<section id=\"1:1\" name=\"A\" x=\"0\" y=\"0\" width=\"9\" height=\"9\"><text id=\"1:2\" name=\"T\" x=\"1\" y=\"1\" width=\"2\" height=\"2\" /></section>"}]}"#,
            )
            .unwrap()
        };
        let fs = FilterSet::default();

        // Ultra + get_design_context: node-tree reduced to id + tag.
        let mut u = mk();
        let sv = compress_result_with(&mut u, "get_design_context", Level::Ultra, &fs);
        assert_eq!(
            u["content"][0]["text"].as_str().unwrap(),
            r#"<section id="1:1"><text id="1:2"/></section>"#
        );
        assert!(sv.after < sv.before, "ultra reports savings");

        // Aggressive: node-tree untouched (geometry survives).
        let mut a = mk();
        compress_result_with(&mut a, "get_design_context", Level::Aggressive, &fs);
        assert!(
            a["content"][0]["text"].as_str().unwrap().contains("width=\"9\""),
            "aggressive keeps node-tree geometry"
        );

        // Ultra but a different tool: untouched (strip is gated to get_design_context).
        let mut w = mk();
        compress_result_with(&mut w, "get_metadata", Level::Ultra, &fs);
        assert!(
            w["content"][0]["text"].as_str().unwrap().contains("name=\"A\""),
            "node-tree strip is gated to get_design_context"
        );
    }

    #[test]
    fn is_figma_node_tree_rejects_jsx_svg_with_geometry() {
        // Review BLOCKER: an inline-SVG element carries id+x+width too, but its id is
        // an author string, not Figma's "N:M". It must NOT be taken for a node-tree
        // (else strip_node_tree_attrs would gut real code, dropping fill/className/…).
        assert!(!is_figma_node_tree(
            r#"<rect id="bg" x="0" y="0" width="100" height="50" fill="blue"/>"#
        ));
        assert!(!is_figma_node_tree(
            r#"<svg id="icon" x="0" y="0" width="24" height="24" viewBox="0 0 24 24"><path d="M0 0h24"/></svg>"#
        ));
        // A genuine node-tree (N:M id) is still detected.
        assert!(is_figma_node_tree(
            r#"<frame id="10:20" name="x" x="0" y="0" width="1" height="1" />"#
        ));
    }

    #[test]
    fn is_figma_node_tree_detected_even_if_root_name_has_gt() {
        // Finding 2: a '>' inside the root's name must not defeat detection.
        assert!(is_figma_node_tree(
            r#"<section id="1:1" name="A > B" x="0" y="0" width="9" height="9"><text id="1:2" /></section>"#
        ));
    }

    #[test]
    fn strip_node_tree_attrs_passes_through_trailing_lt() {
        // Finding 3: a stray trailing '<' must pass through, not become "<>".
        assert_eq!(
            strip_node_tree_attrs(r#"<frame id="1:1"/><"#),
            r#"<frame id="1:1"/><"#
        );
    }
}
