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

// ---------------------------------------------------------------------------
// Ultra text-transform registry
// ---------------------------------------------------------------------------

/// A named, tool-gated Ultra text transform applied when no TOML filter matched
/// a content block. Pure-functional; lossy but tee-recoverable. Registered in
/// ULTRA_TRANSFORMS; the content loop is agnostic to which tools exist.
trait TextTransformer: Sync {
    fn applies(&self, tool: &str, text: &str) -> bool;
    fn transform(&self, text: &str) -> String;
    /// Human-readable identifier, used for logging and diagnostics.
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
}

/// Ultra transform for `get_design_context`: strips Figma-ref node attributes
/// from generated JSX, or reduces a sparse node-tree to id+tag only.
/// The `applies` check is text-agnostic (tool-match only); the two sub-cases
/// (node-tree vs JSX) are resolved inside `transform`, exactly mirroring the
/// original if/else nesting.
struct DesignContextJsx;

impl TextTransformer for DesignContextJsx {
    fn applies(&self, tool: &str, _text: &str) -> bool {
        crate::filter::matches_tool(tool, "get_design_context")
    }

    fn transform(&self, text: &str) -> String {
        if is_figma_node_tree(text) {
            strip_node_tree_attrs(text)
        } else {
            compress_jsx_code(&strip_figma_node_attrs(text))
        }
    }

    fn name(&self) -> &'static str {
        "DesignContextJsx"
    }
}

/// Ultra transform for `get_metadata`: strips positional geometry attributes
/// (x, y, width, height) from metadata XML. The `applies` check includes the
/// `starts_with('<')` guard that was in the original else-if branch.
struct MetadataPosAttrs;

impl TextTransformer for MetadataPosAttrs {
    fn applies(&self, tool: &str, text: &str) -> bool {
        crate::filter::matches_tool(tool, "get_metadata") && text.starts_with('<')
    }

    fn transform(&self, text: &str) -> String {
        strip_metadata_pos_attrs(text)
    }

    fn name(&self) -> &'static str {
        "MetadataPosAttrs"
    }
}

/// Registry of Ultra text transforms. Order is significant: first match wins.
/// `get_design_context` is listed before `get_metadata` to preserve the
/// first-match order of the original if/else chain.
static ULTRA_TRANSFORMS: &[&dyn TextTransformer] = &[&DesignContextJsx, &MetadataPosAttrs];

/// Walk the registry and return the first matching transform's output, or
/// return `text` unchanged when no transform applies. Called at Ultra level
/// only; the `ultra` gate lives at the call site so Standard/Aggressive paths
/// remain byte-identical.
fn apply_ultra_transform(tool: &str, text: String) -> String {
    for t in ULTRA_TRANSFORMS {
        if t.applies(tool, &text) {
            return t.transform(&text);
        }
    }
    text
}

#[must_use]
#[derive(Default, Clone, Copy)]
pub struct Savings {
    pub before: usize,
    pub after: usize,
    /// True when `apply_structural` or `apply_text` mutated the value (e.g.
    /// dropped `_meta` or a boilerplate block), even if `content[].text`
    /// byte lengths happen to be unchanged (e.g. non-text content blocks only).
    pub mutated: bool,
    /// Total payload bytes from image content blocks (type=="image"). Images
    /// pass through byte-for-byte and are NOT compressed, but their volume is
    /// tracked here so the ledger can record screenshot sizes. This field does
    /// NOT contribute to `before`/`after` (those cover only text blocks).
    pub image_bytes: usize,
}

impl Savings {
    pub fn new(before: usize, after: usize) -> Self {
        Self {
            before,
            after: after.min(before),
            mutated: false,
            image_bytes: 0,
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
    compress_result_with_min_priority(result, tool, level, filters, 0.0)
}

/// Like [`compress_result_with`] but also accepts a `min_priority` threshold for
/// the MCP annotation pre-filter (Aggressive+ only). At Standard level or when
/// `min_priority == 0.0`, the annotation pass is always a no-op.
pub fn compress_result_with_min_priority(
    result: &mut Value,
    tool: &str,
    level: Level,
    filters: &FilterSet,
    min_priority: f64,
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

    // Aggressive/ultra: MCP annotation pre-filter (2025-06-18 spec §4.3).
    // Drop content items that are explicitly NOT addressed to the assistant or
    // fall below the priority floor. This is a TRUE no-op when:
    //   (a) level is Standard, or
    //   (b) the content item has no `annotations` field (Figma does not emit
    //       annotations yet), or
    //   (c) min_priority == 0.0 AND audience is absent or contains "assistant".
    // Image and resource_link items are NEVER dropped regardless of annotations.
    let annotation_mutated = if aggressive {
        drop_annotation_excluded(result, min_priority)
    } else {
        false
    };

    let mut text_mutated = false;
    let mut image_bytes = 0usize;
    if let Some(content) = result.get_mut("content").and_then(|c| c.as_array_mut()) {
        for item in content {
            // Image blocks pass through byte-for-byte but their payload length
            // is metered so the ledger can track screenshot / vision volume.
            // Prefer the "data" field (base64 payload); fall back to "url" or
            // "resource" (reference form). The item is never mutated here.
            if item.get("type").and_then(|t| t.as_str()) == Some("image") {
                let payload_len = item
                    .get("data")
                    .and_then(|d| d.as_str())
                    .map(|s| s.len())
                    .or_else(|| item.get("url").and_then(|u| u.as_str()).map(|s| s.len()))
                    .or_else(|| {
                        item.get("resource")
                            .and_then(|r| r.as_str())
                            .map(|s| s.len())
                    })
                    .unwrap_or(0);
                image_bytes += payload_len;
                continue;
            }
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
                        // Ultra: dispatch through the ULTRA_TRANSFORMS registry.
                        // Standard/Aggressive return plain `c` (whitespace-only pass).
                        if ultra {
                            apply_ultra_transform(tool, c)
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
    if structural_mutated || annotation_mutated || text_mutated {
        sv.mutated = true;
    }
    sv.image_bytes = image_bytes;
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
            let _ = compress_result_with(v.get_mut("result").unwrap(), tool, level, filters);
            let out = serde_json::to_string(&v).unwrap_or_else(|_| s.to_string());
            let sv = Savings::new(s.len(), out.len());
            return (out, sv);
        }
        if v.get("content").is_some() {
            let _ = compress_result_with(&mut v, tool, level, filters);
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
///
/// UTF-8 safe: the delimiters (`>`, `<`, `\n`, `\r`, space, tab) are all
/// single-byte ASCII codepoints and never appear as continuation bytes of a
/// multibyte sequence, so per-byte matching cannot split a multibyte character.
/// The only bytes we skip are confirmed whitespace ASCII bytes; the only bytes
/// we copy are exact byte-index slices of the original `&str`, so multibyte
/// content (e.g. Cyrillic, CJK) is always copied whole-codepoint.
fn strip_xml_indent(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        // Copy up to and including the next `>` (or end of input).
        let start = i;
        while i < bytes.len() && bytes[i] != b'>' {
            i += 1;
        }
        if i >= bytes.len() {
            // No more `>` — copy the remainder and we're done.
            out.push_str(&s[start..]);
            break;
        }
        // Include the `>` itself.
        i += 1; // now i points one past the `>`
        out.push_str(&s[start..i]);

        // Scan the whitespace run after `>`.
        let ws_start = i;
        let mut saw_newline = false;
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
            if bytes[i] == b'\n' || bytes[i] == b'\r' {
                saw_newline = true;
            }
            i += 1;
        }
        // Drop the whitespace iff it contained a newline AND the run ends at `<`.
        // If not, write it back verbatim.
        if saw_newline && i < bytes.len() && bytes[i] == b'<' && i > ws_start {
            // Skip — the whitespace run is pure indentation; continue from `<`.
        } else {
            out.push_str(&s[ws_start..i]);
        }
    }
    out
}

fn collapse_code_ws(s: &str) -> String {
    // Write directly into an output String, skipping leading blank lines until
    // the first non-blank one. Blank runs of >1 are collapsed to a single blank.
    // Trailing blank lines are omitted. No per-line heap allocation, no Vec, no
    // O(n) remove(0) for the leading-blank strip.
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0u32;
    let mut started = false; // true once the first non-blank line has been written

    for raw in s.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            if started {
                blank_run += 1;
                if blank_run == 1 {
                    // Tentatively push a blank separator; it may be trailing and
                    // will be trimmed below if no further content line follows.
                    out.push('\n');
                }
            }
            // Before the first content line: skip (strip leading blanks).
        } else {
            started = true;
            blank_run = 0;
            // If a blank separator was already pushed (blank_run==1 path above),
            // the '\n' delimiter we push below comes immediately after it, giving
            // the canonical "\n\n" (one blank line). If this is the very first
            // content line, `out` is empty and we skip the leading newline.
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    // Strip any trailing blank lines: `out` ends with '\n' only when the last
    // pushed item was a blank separator (blank_run >= 1 path). Trim those.
    while out.ends_with('\n') {
        out.pop();
    }
    out
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

/// Ultra-level only, get_metadata XML. Strips the four positional attributes
/// `x`, `y`, `width`, `height` from every element. These are layout details
/// the agent re-derives from get_design_context; for navigation it needs only
/// id/name/type/hidden. This is LOSSY (geometry gone), hence ultra-gated; the
/// raw payload stays recoverable via tee/capture.
///
/// Attribute boundary rule: a target attr is recognised ONLY as ` <name>="…"`
/// — a SPACE, then the exact attribute name, then `="`. This ensures:
///   • `height` does NOT match `line-height` (no space before `height`).
///   • `width`  does NOT match `max-width`  (no space before `width`).
///   • `x`/`y`  do NOT match `xml:…` (space + `x` + `=`, not `x` + any char).
///   Values are XML-escaped — a literal `"` inside a value is encoded as
///   `&quot;`, so the first raw `"` after `="` is always the true end-delimiter.
///   The scan skips bytes from the leading space through the closing `"` inclusive.
///
/// PRECONDITION: all attribute values use double-quote delimiters (true for
/// Figma-generated metadata XML; not validated at runtime). A single-quoted
/// attribute whose value contained the literal bytes ` x="` is the one unsafe
/// shape — Figma never emits that, and Ultra is tee-gated so the raw stays
/// recoverable regardless.
///
/// Byte-scan is UTF-8 safe: every index lands on an ASCII delimiter or is taken
/// from a whole-slice copy of the original `&str` (multibyte chars are never
/// split — the markers and `"` are all single-byte ASCII codepoints).
fn strip_metadata_pos_attrs(xml: &str) -> String {
    // The four targets, each as the leading-space + name + =\" prefix.
    const MARKERS: [&[u8]; 4] = [b" x=\"", b" y=\"", b" width=\"", b" height=\""];
    let bytes = xml.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    'outer: while i < n {
        // Look for the nearest marker starting at `i`.
        let mut best: Option<(usize, usize)> = None; // (position, marker_len)
        for marker in &MARKERS {
            let mlen = marker.len();
            if i + mlen > n {
                continue;
            }
            // Simple substring scan from position i.
            let mut j = i;
            while j + mlen <= n {
                if bytes[j..j + mlen] == **marker {
                    if best.is_none_or(|(bi, _)| j < bi) {
                        best = Some((j, mlen));
                    }
                    break;
                }
                j += 1;
            }
        }
        let Some((pos, mlen)) = best else {
            // No more markers — copy the rest verbatim.
            out.push_str(&xml[i..]);
            break 'outer;
        };
        // Copy everything up to (but not including) the marker's leading space.
        out.push_str(&xml[i..pos]);
        // Skip past the marker (space + name + =") and then skip to the closing ".
        let after_open = pos + mlen; // index of the first byte of the value
                                     // Scan for the closing double-quote.  Values are XML-escaped so this is
                                     // always a real delimiter (no unescaped `"` appears inside an XML attr value).
        let mut k = after_open;
        while k < n && bytes[k] != b'"' {
            k += 1;
        }
        // Skip the closing `"` as well.
        if k < n {
            k += 1;
        }
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

/// Aggressive+ annotation pre-filter (MCP spec 2025-06-18 §4.3).
///
/// Iterates `result["content"]` once and retains items, dropping those that
/// are explicitly addressed away from the assistant or below the priority floor.
/// The rule is intentionally conservative (true no-op by default):
///
/// Drop an item ONLY IF it HAS an `annotations` object AND either:
///   - `priority` is present AND `priority < min_priority`, OR
///   - `audience` is present, non-empty, and does NOT contain "assistant".
///
/// Items of type `"image"` or `"resource_link"` are **never** dropped,
/// regardless of their annotations, to preserve vision and resource data.
///
/// Returns `true` when at least one item was dropped (caller sets `sv.mutated`).
fn drop_annotation_excluded(result: &mut Value, min_priority: f64) -> bool {
    let Some(content) = result.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return false;
    };
    let before = content.len();
    content.retain(|item| {
        // Never drop image or resource_link items.
        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if item_type == "image" || item_type == "resource_link" {
            return true;
        }
        let Some(ann) = item.get("annotations") else {
            // No annotations field → keep.
            return true;
        };
        // Check priority floor.
        if let Some(priority) = ann.get("priority").and_then(|p| p.as_f64()) {
            if priority < min_priority {
                return false;
            }
        }
        // Check audience: if present, non-empty, and does not contain "assistant" → drop.
        if let Some(audience) = ann.get("audience").and_then(|a| a.as_array()) {
            if !audience.is_empty() && !audience.iter().any(|v| v.as_str() == Some("assistant")) {
                return false;
            }
        }
        true
    });
    content.len() < before
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
        assert_eq!(
            out,
            "<frame name=\"Hero\"><text>Button Label</text></frame>"
        );
        // Whitespace-only invariant: removing ALL whitespace from input and output
        // must give identical strings — the transform touched only whitespace chars.
        assert_eq!(
            ws_stripped(xml),
            ws_stripped(&out),
            "xml strip_indent must only remove whitespace, never other chars"
        );
    }

    #[test]
    fn xml_strip_indent_multibyte_content_preserved() {
        // Cyrillic characters are multibyte (2 bytes each in UTF-8). The byte scan
        // must not split them, and content inside elements must be preserved verbatim.
        let xml = "<root>\n    <item>Привет мир</item>\n    <item>Москва</item>\n</root>";
        let out = compress_text(xml);
        assert_eq!(
            out, "<root><item>Привет мир</item><item>Москва</item></root>",
            "Cyrillic content must survive XML indent stripping intact"
        );
        // Each Cyrillic char should still be readable as valid UTF-8
        assert!(
            out.is_ascii() || out.chars().all(|c| c != char::REPLACEMENT_CHARACTER),
            "no replacement characters (no char splits)"
        );
    }

    #[test]
    fn code_blank_runs_collapse() {
        let code = "const a = 1;   \n\n\n\nconst b = 2;\n";
        let out = compress_text(code);
        assert_eq!(out, "const a = 1;\n\nconst b = 2;");
    }

    #[test]
    fn code_collapse_all_blank() {
        // An entirely-blank input (or all whitespace) should produce an empty string.
        assert_eq!(collapse_code_ws(""), "");
        assert_eq!(collapse_code_ws("   \n   \n   "), "");
        assert_eq!(collapse_code_ws("\n\n\n"), "");
    }

    #[test]
    fn code_collapse_leading_blanks_stripped() {
        // Leading blank lines are stripped; content lines are preserved.
        let code = "\n\n\nconst x = 1;";
        let out = collapse_code_ws(code);
        assert_eq!(out, "const x = 1;");
    }

    #[test]
    fn code_collapse_trailing_blanks_stripped() {
        // Trailing blank lines are stripped.
        let code = "const x = 1;\n\n\n";
        let out = collapse_code_ws(code);
        assert_eq!(out, "const x = 1;");
    }

    #[test]
    fn code_collapse_many_leading_blanks() {
        // Many leading blanks (>1) are all removed; interior blank runs collapse to 1.
        let code = "\n\n\n\n\nconst a = 1;\n\n\nconst b = 2;\n\n";
        let out = collapse_code_ws(code);
        assert_eq!(out, "const a = 1;\n\nconst b = 2;");
    }

    #[test]
    fn code_collapse_single_line_trailing_spaces() {
        // A single content line with trailing spaces: spaces stripped, no trailing newline.
        assert_eq!(collapse_code_ws("hello   "), "hello");
        assert_eq!(collapse_code_ws("  hello  "), "  hello"); // leading preserved, trailing stripped
    }

    #[test]
    fn compress_any_handles_envelope_and_meters_total() {
        let env = "{\n  \"result\": {\n    \"content\": [\n      {\"type\":\"text\",\"text\":\"{\\n  \\\"a\\\": 1\\n}\"}\n    ]\n  }\n}";
        let (out, sv) = compress_any(env);
        assert_eq!(sv.before, env.len());
        assert!(sv.after < sv.before, "envelope shrank");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["result"]["content"][0]["text"],
            Value::String("{\"a\":1}".into())
        );
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
        assert!(
            !agg.contains("vectorPaths"),
            "filter drops key at aggressive"
        );
        assert!(agg.contains("keep"));
        assert!(sv.after < sv.before);

        let (std, _) = compress_payload(env, "get_design_context", Level::Standard, &fs);
        assert!(
            std.contains("vectorPaths"),
            "standard must not apply filters"
        );
    }

    #[test]
    fn strip_figma_node_attrs_removes_both_attrs_keeps_rest() {
        let jsx = r#"<div className="x" data-node-id="136:2" data-name="Hero — FullHD (1920×1080)"><p data-node-id="136:6">Привет</p></div>"#;
        let out = strip_figma_node_attrs(jsx);
        assert_eq!(
            out, r#"<div className="x"><p>Привет</p></div>"#,
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
        assert!(
            out.contains("\n<p>Привет — мир</p>"),
            "text line dedented, content intact"
        );
        // significant template spaces preserved verbatim
        assert!(
            out.contains("{`keep  these  spaces`}"),
            "template spaces preserved"
        );
        // INVARIANT: only whitespace was touched
        assert_eq!(
            ws_stripped(code),
            ws_stripped(&out),
            "no non-whitespace changed"
        );
        assert!(out.len() < code.len(), "smaller");
    }

    #[test]
    fn compress_jsx_code_preserves_multiline_template_indentation() {
        // A backtick template spanning lines: its inner indentation is significant
        // and must NOT be stripped.
        let code = "const x = `\n    indented inside template\n`;\n<div>\n  <p>hi</p>\n</div>";
        let out = compress_jsx_code(code);
        assert!(
            out.contains("\n    indented inside template\n"),
            "template indent kept"
        );
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
        assert!(
            out.contains("\n      more`;"),
            "template-interior line verbatim"
        );
        assert!(out.contains("\nreturn t;"), "trailing code dedented");
    }

    #[test]
    fn has_odd_unescaped_backticks_counts_correctly() {
        assert!(has_odd_unescaped_backticks("const x = `"));
        assert!(!has_odd_unescaped_backticks("const x = `y`;"));
        assert!(
            !has_odd_unescaped_backticks(r"a \` b"),
            "escaped backtick not counted"
        );
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
        let _ = compress_result_with(&mut agg, "get_design_context", Level::Aggressive, &fs);
        assert!(
            agg["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("data-node-id"),
            "aggressive keeps node ids"
        );

        // Standard: attrs kept.
        let mut std = mk();
        let _ = compress_result_with(&mut std, "get_design_context", Level::Standard, &fs);
        assert!(
            std["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("data-name"),
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
        assert!(
            !out.contains("Node ids have been added"),
            "node-id block dropped"
        );
        assert!(
            !out.contains("Images and SVGs will be stored"),
            "images block dropped"
        );
        assert!(!out.contains("mcpRequestId"), "_meta dropped");
        assert!(
            out.contains("export default function Hero"),
            "code block kept"
        );
        assert!(
            out.contains("These styles are contained"),
            "design-token block kept"
        );
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
        assert!(
            out_std.contains("SUPER CRITICAL"),
            "standard keeps boilerplate"
        );
        assert!(out_std.contains("mcpRequestId"), "standard keeps _meta");
    }

    #[test]
    fn image_block_is_metered_not_mutated() {
        // An image content block must be metered (its data length → image_bytes)
        // but NOT mutated — the bytes pass through byte-for-byte. before==after==image_bytes.
        let img_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let mut v: Value = serde_json::json!({
            "content": [{"type": "image", "data": img_data, "mediaType": "image/png"}]
        });
        let sv = compress_result_with(
            &mut v,
            "get_screenshot",
            Level::Standard,
            &FilterSet::default(),
        );
        assert_eq!(
            sv.image_bytes,
            img_data.len(),
            "image_bytes == data field length"
        );
        assert_eq!(
            sv.before, sv.after,
            "no text savings for image-only content (before==after==0)"
        );
        assert!(
            !sv.mutated,
            "a pure image block must NOT set sv.mutated — nothing was rewritten"
        );
        // The image item must be byte-for-byte identical.
        assert_eq!(
            v["content"][0]["data"].as_str(),
            Some(img_data),
            "image data unchanged"
        );
        assert_eq!(
            v["content"][0]["type"].as_str(),
            Some("image"),
            "type unchanged"
        );
    }

    #[test]
    fn image_block_url_field_metered_when_no_data() {
        // If "data" is absent but "url" is present, the url length is metered.
        let url = "https://example.com/screenshot.png";
        let mut v: Value = serde_json::json!({
            "content": [{"type": "image", "url": url}]
        });
        let sv = compress_result_with(
            &mut v,
            "get_screenshot",
            Level::Standard,
            &FilterSet::default(),
        );
        assert_eq!(
            sv.image_bytes,
            url.len(),
            "url length metered when data absent"
        );
        // Image item must still be unchanged.
        assert_eq!(v["content"][0]["url"].as_str(), Some(url), "url unchanged");
    }

    #[test]
    fn mixed_text_and_image_both_metered() {
        // A result with both a text block and an image block: text is compressed,
        // image_bytes is set for the image, and the image item is unchanged.
        let text = "{\n  \"a\": 1\n}";
        let img_data = "abc123base64";
        let mut v: Value = serde_json::json!({
            "content": [
                {"type": "text", "text": text},
                {"type": "image", "data": img_data}
            ]
        });
        let sv = compress_result_with(
            &mut v,
            "get_screenshot",
            Level::Standard,
            &FilterSet::default(),
        );
        assert_eq!(
            sv.image_bytes,
            img_data.len(),
            "image_bytes set for image block"
        );
        assert!(sv.before > sv.after, "text block compressed");
        assert_eq!(
            v["content"][0]["text"].as_str(),
            Some("{\"a\":1}"),
            "text compressed"
        );
        assert_eq!(
            v["content"][1]["data"].as_str(),
            Some(img_data),
            "image unchanged"
        );
    }

    #[test]
    fn compress_result_rewrites_text() {
        let mut v: Value =
            serde_json::from_str(r#"{"content":[{"type":"text","text":"{\n  \"x\": 1\n}"}]}"#)
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
        let _ = compress_result_with(&mut a, "get_design_context", Level::Aggressive, &fs);
        assert!(
            a["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("width=\"9\""),
            "aggressive keeps node-tree geometry"
        );

        // Ultra but a different tool: untouched (strip is gated to get_design_context).
        let mut w = mk();
        let _ = compress_result_with(&mut w, "get_metadata", Level::Ultra, &fs);
        assert!(
            w["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("name=\"A\""),
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

    // -----------------------------------------------------------------------
    // R2-B: Annotation pre-filter tests
    // -----------------------------------------------------------------------

    /// An item with NO annotations object must NEVER be dropped, even at Aggressive.
    #[test]
    fn annotation_no_annotations_field_never_dropped() {
        let mut v: Value = serde_json::json!({
            "content": [{"type": "text", "text": "hello"}]
        });
        let sv = compress_result_with(
            &mut v,
            "get_design_context",
            crate::config::Level::Aggressive,
            &FilterSet::default(),
        );
        assert_eq!(
            v["content"].as_array().unwrap().len(),
            1,
            "item without annotations must survive"
        );
        // No mutation from annotation pass (text was already compact, no other filters).
        let _ = sv;
    }

    /// At Standard level the annotation pass must be a true no-op — no drops even
    /// when the item has annotations that would be dropped at Aggressive.
    #[test]
    fn annotation_no_op_at_standard() {
        let mut v: Value = serde_json::json!({
            "content": [{"type": "text", "text": "x", "annotations": {"audience": ["user"], "priority": 1.0}}]
        });
        let sv = compress_result_with(
            &mut v,
            "get_design_context",
            crate::config::Level::Standard,
            &FilterSet::default(),
        );
        assert_eq!(
            v["content"].as_array().unwrap().len(),
            1,
            "standard: annotation drop must not fire"
        );
        let _ = sv;
    }

    /// At Aggressive, an item whose audience is ["user"] only (not "assistant") is dropped.
    #[test]
    fn annotation_drops_user_only_audience_at_aggressive() {
        let mut v: Value = serde_json::json!({
            "content": [
                {"type": "text", "text": "assistant text"},
                {"type": "text", "text": "user only", "annotations": {"audience": ["user"]}}
            ]
        });
        let _ = compress_result_with(
            &mut v,
            "get_design_context",
            crate::config::Level::Aggressive,
            &FilterSet::default(),
        );
        let arr = v["content"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            1,
            "user-only item dropped at aggressive: {arr:?}"
        );
        assert_eq!(arr[0]["text"].as_str().unwrap(), "assistant text");
    }

    /// An image item with audience=["user"] must NEVER be dropped regardless of annotations.
    #[test]
    fn annotation_never_drops_image_items() {
        let img_data = "abc123";
        let mut v: Value = serde_json::json!({
            "content": [
                {"type": "image", "data": img_data, "annotations": {"audience": ["user"]}}
            ]
        });
        let _ = compress_result_with(
            &mut v,
            "get_design_context",
            crate::config::Level::Aggressive,
            &FilterSet::default(),
        );
        assert_eq!(
            v["content"].as_array().unwrap().len(),
            1,
            "image items must never be dropped by annotation pass"
        );
        assert_eq!(
            v["content"][0]["data"].as_str().unwrap(),
            img_data,
            "image data unchanged"
        );
    }

    /// A resource_link item with audience=["user"] must NEVER be dropped regardless of annotations.
    #[test]
    fn annotation_never_drops_resource_link_items() {
        let mut v: Value = serde_json::json!({
            "content": [
                {"type": "resource_link", "uri": "figma://node/123", "annotations": {"audience": ["user"]}}
            ]
        });
        let _ = compress_result_with(
            &mut v,
            "get_design_context",
            crate::config::Level::Aggressive,
            &FilterSet::default(),
        );
        assert_eq!(
            v["content"].as_array().unwrap().len(),
            1,
            "resource_link items must never be dropped by annotation pass"
        );
        assert_eq!(
            v["content"][0]["uri"].as_str().unwrap(),
            "figma://node/123",
            "resource_link uri unchanged"
        );
    }

    /// At default config (min_priority=0.0), an item with priority=0.0 is NOT dropped.
    #[test]
    fn annotation_default_priority_floor_keeps_zero_priority_item() {
        let mut v: Value = serde_json::json!({
            "content": [
                {"type": "text", "text": "low prio", "annotations": {"priority": 0.0}}
            ]
        });
        let _ = compress_result_with(
            &mut v,
            "get_design_context",
            crate::config::Level::Aggressive,
            &FilterSet::default(),
        );
        assert_eq!(
            v["content"].as_array().unwrap().len(),
            1,
            "priority=0.0 must not be dropped at default min_priority=0.0"
        );
    }

    /// With min_priority=0.5, an item with priority=0.3 is dropped at Aggressive.
    #[test]
    fn annotation_below_priority_floor_dropped() {
        let fs = FilterSet::default();
        let mut v: Value = serde_json::json!({
            "content": [
                {"type": "text", "text": "keep", "annotations": {"priority": 0.8}},
                {"type": "text", "text": "drop", "annotations": {"priority": 0.3}}
            ]
        });
        let _ = compress_result_with_min_priority(
            &mut v,
            "get_design_context",
            crate::config::Level::Aggressive,
            &fs,
            0.5,
        );
        let arr = v["content"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            1,
            "priority<min_priority item must be dropped: {arr:?}"
        );
        assert_eq!(arr[0]["text"].as_str().unwrap(), "keep");
    }

    /// When an annotation drop occurs, sv.mutated must be true.
    #[test]
    fn annotation_drop_sets_mutated() {
        let mut v: Value = serde_json::json!({
            "content": [
                {"type": "text", "text": "x", "annotations": {"audience": ["user"]}}
            ]
        });
        let sv = compress_result_with(
            &mut v,
            "get_design_context",
            crate::config::Level::Aggressive,
            &FilterSet::default(),
        );
        assert!(sv.mutated, "annotation drop must set sv.mutated");
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

    // -----------------------------------------------------------------------
    // PERF-8: strip_metadata_pos_attrs — get_metadata XML geometry stripping
    // -----------------------------------------------------------------------

    /// Basic: drops x/y/width/height but keeps id, name, tag, hidden, self-close,
    /// nesting, and any other attributes.
    #[test]
    fn strip_metadata_pos_attrs_drops_geometry_keeps_rest() {
        let xml = r#"<frame id="1:1" name="Hero" x="10" y="20" width="1920" height="1080" hidden="false"><text id="1:2" name="Title" x="57" y="321" width="246" height="22" /></frame>"#;
        let out = strip_metadata_pos_attrs(xml);
        // Geometry attrs dropped.
        assert!(!out.contains(r#" x=""#), "x attr dropped");
        assert!(!out.contains(r#" y=""#), "y attr dropped");
        assert!(!out.contains(r#" width=""#), "width attr dropped");
        assert!(!out.contains(r#" height=""#), "height attr dropped");
        // Identity attrs kept.
        assert!(out.contains(r#"id="1:1""#), "id kept");
        assert!(out.contains(r#"name="Hero""#), "name kept");
        assert!(out.contains(r#"hidden="false""#), "hidden kept");
        assert!(out.contains(r#"id="1:2""#), "nested id kept");
        assert!(out.contains(r#"name="Title""#), "nested name kept");
        // Tag structure preserved.
        assert!(out.contains("<frame"), "opening tag kept");
        assert!(out.contains("</frame>"), "closing tag kept");
        assert!(out.contains("/>"), "self-close kept");
        // Result is strictly smaller.
        assert!(out.len() < xml.len(), "output is smaller");
    }

    /// Name safety: a node whose name literally contains tricky text like
    /// `name="box width=99 x=1"` (XML-escaped, so no real inner `"`) must survive
    /// with its name byte-identical; width/height/x/y as real attrs are still dropped.
    #[test]
    fn strip_metadata_pos_attrs_name_safety() {
        // The name value contains the literal text "width=99 x=1" — in the real XML
        // this would be encoded as `name="box width=99 x=1"` (no inner quotes because
        // the string has none; spaces and = are valid unescaped inside an XML attr value).
        let xml =
            r#"<frame id="2:1" name="box width=99 x=1" x="0" y="0" width="200" height="100" />"#;
        let out = strip_metadata_pos_attrs(xml);
        // The tricky name must survive byte-identical.
        assert!(
            out.contains(r#"name="box width=99 x=1""#),
            "name with tricky content survives: {out:?}"
        );
        // Real geometry attrs are dropped.
        // Specifically: ` x="0"`, ` y="0"`, ` width="200"`, ` height="100"` are gone.
        // We check via the attr-boundary pattern (space + exact-name + ="): e.g. ` x="0"`.
        assert!(!out.contains(r#" x="0""#), "real x attr dropped");
        assert!(!out.contains(r#" y="0""#), "real y attr dropped");
        assert!(!out.contains(r#" width="200""#), "real width attr dropped");
        assert!(
            !out.contains(r#" height="100""#),
            "real height attr dropped"
        );
    }

    /// Boundary correctness: `line-height` and `max-width` style attr names must NOT
    /// be stripped (the required leading space before the exact name provides the
    /// boundary — there is no space inside `line-height`).
    #[test]
    fn strip_metadata_pos_attrs_boundary_no_partial_match() {
        // line-height and max-width should be untouched.
        let xml = r#"<text id="3:1" name="Label" line-height="1.5" max-width="400" x="0" y="0" width="100" height="20" />"#;
        let out = strip_metadata_pos_attrs(xml);
        assert!(out.contains(r#"line-height="1.5""#), "line-height kept");
        assert!(out.contains(r#"max-width="400""#), "max-width kept");
        // Real geometry dropped.
        assert!(!out.contains(r#" x="0""#), "x dropped");
        assert!(!out.contains(r#" y="0""#), "y dropped");
        assert!(!out.contains(r#" width="100""#), "width dropped");
        assert!(!out.contains(r#" height="20""#), "height dropped");
    }

    /// Gating: Standard and Aggressive keep all attrs (only lossless indent-strip);
    /// Ultra strips geometry. And get_design_context payload is NOT affected.
    #[test]
    fn strip_metadata_pos_attrs_gating() {
        // A get_metadata-shaped XML with geometry attrs.
        let xml_text = r#"<frame id="4:1" name="Page" x="0" y="0" width="1920" height="1080" />"#;
        let mk = |xml: &str| -> Value {
            serde_json::json!({"content": [{"type": "text", "text": xml}]})
        };
        let fs = FilterSet::default();

        // Standard: geometry kept (only whitespace transforms).
        let mut std_v = mk(xml_text);
        let _ = compress_result_with(&mut std_v, "get_metadata", Level::Standard, &fs);
        let t_std = std_v["content"][0]["text"].as_str().unwrap();
        assert!(t_std.contains(r#" x="0""#), "standard keeps x");
        assert!(t_std.contains(r#" width="1920""#), "standard keeps width");

        // Aggressive: geometry kept.
        let mut agg_v = mk(xml_text);
        let _ = compress_result_with(&mut agg_v, "get_metadata", Level::Aggressive, &fs);
        let t_agg = agg_v["content"][0]["text"].as_str().unwrap();
        assert!(t_agg.contains(r#" x="0""#), "aggressive keeps x");
        assert!(t_agg.contains(r#" width="1920""#), "aggressive keeps width");

        // Ultra on get_metadata: geometry stripped.
        let mut ultra_v = mk(xml_text);
        let _ = compress_result_with(&mut ultra_v, "get_metadata", Level::Ultra, &fs);
        let t_ultra = ultra_v["content"][0]["text"].as_str().unwrap();
        assert!(
            !t_ultra.contains(r#" x=""#),
            "ultra strips x from get_metadata"
        );
        assert!(
            !t_ultra.contains(r#" width=""#),
            "ultra strips width from get_metadata"
        );
        assert!(t_ultra.contains(r#"id="4:1""#), "ultra keeps id");
        assert!(t_ultra.contains(r#"name="Page""#), "ultra keeps name");

        // Ultra on get_design_context with the same XML shape: NOT stripped by this branch.
        let mut gdc_v = mk(xml_text);
        let _ = compress_result_with(&mut gdc_v, "get_design_context", Level::Ultra, &fs);
        let t_gdc = gdc_v["content"][0]["text"].as_str().unwrap();
        // get_design_context ultra branch routes this through strip_node_tree_attrs
        // (because it has Figma id shape N:M) — the id is kept but no geometry.
        // The important thing is the get_metadata branch did NOT run.
        // (strip_node_tree_attrs drops geometry too, but via a different code path.)
        let _ = t_gdc; // outcome asserted separately in other tests
    }

    // -----------------------------------------------------------------------
    // ULTRA_TRANSFORMS registry dispatch parity test
    // -----------------------------------------------------------------------

    /// Verify that `apply_ultra_transform` produces the same output as the
    /// original inline if/else chain for:
    ///   (a) a get_design_context JSX sample,
    ///   (b) a get_metadata XML sample starting with '<',
    ///   (c) an unknown tool (passthrough).
    /// This test does NOT change or re-state any existing assertion; it only
    /// confirms the new dispatch path is observably equivalent.
    #[test]
    fn ultra_transforms_dispatch_matches_original_behaviour() {
        // (a) get_design_context JSX: strip_figma_node_attrs + compress_jsx_code
        let jsx = r#"<div className="x" data-node-id="1:2" data-name="H">hi</div>"#;
        let c = compress_text(jsx);
        let expected_gdc = compress_jsx_code(&strip_figma_node_attrs(&c));
        let got_gdc = apply_ultra_transform("mcp__plugin__get_design_context", c.clone());
        assert_eq!(got_gdc, expected_gdc, "gdc JSX dispatch mismatch");

        // (b) get_metadata XML starting with '<': strip_metadata_pos_attrs
        let xml = r#"<frame id="1:1" name="P" x="0" y="0" width="100" height="50" />"#;
        let c2 = compress_text(xml);
        let expected_meta = strip_metadata_pos_attrs(&c2);
        let got_meta = apply_ultra_transform("mcp__plugin__get_metadata", c2.clone());
        assert_eq!(got_meta, expected_meta, "metadata XML dispatch mismatch");

        // (c) Unknown tool: passthrough (text unchanged)
        let plain = "hello world";
        let c3 = compress_text(plain);
        let got_unknown = apply_ultra_transform("mcp__plugin__some_other_tool", c3.clone());
        assert_eq!(got_unknown, c3, "unknown tool must pass through unchanged");
    }

    /// Measure: a representative 10-node fixture at Ultra must achieve ≥35% reduction
    /// compared to the indent-stripped-only (Standard-level) output.
    #[test]
    fn strip_metadata_pos_attrs_measure_reduction() {
        // Build a ~10-node fixture mirroring the real shape: frame + 9 child nodes
        // each with id, name, x, y, width, height.
        let xml = r#"<frame id="10:1" name="Main" x="0" y="0" width="1920" height="1080"><frame id="10:2" name="Header" x="0" y="0" width="1920" height="80"><text id="10:3" name="Logo" x="24" y="20" width="120" height="40" /><text id="10:4" name="NavLink1" x="200" y="20" width="80" height="40" /><text id="10:5" name="NavLink2" x="300" y="20" width="80" height="40" /></frame><frame id="10:6" name="Hero" x="0" y="80" width="1920" height="600"><text id="10:7" name="Headline" x="240" y="200" width="800" height="60" /><text id="10:8" name="Subhead" x="240" y="280" width="600" height="40" /><rect id="10:9" name="CTA" x="240" y="360" width="200" height="52" /></frame><frame id="10:10" name="Footer" x="0" y="980" width="1920" height="100"><text id="10:11" name="Copyright" x="760" y="40" width="400" height="20" /></frame></frame>"#;

        // Simulate what compress_text does at Standard (lossless indent-strip).
        // The fixture has no indentation, so indent-strip is a no-op here —
        // meaning `standard_len == xml.len()`.  We use that as the baseline.
        let standard_out = compress_text(xml); // lossless only (same content, may differ in ws)
        let standard_len = standard_out.len();

        // Ultra strip.
        let ultra_out = strip_metadata_pos_attrs(&standard_out);
        let ultra_len = ultra_out.len();

        let reduction_pct = (standard_len - ultra_len) as f64 / standard_len as f64 * 100.0;
        assert!(
            reduction_pct >= 35.0,
            "expected ≥35% reduction over indent-stripped baseline, got {reduction_pct:.1}% \
             (standard_len={standard_len}, ultra_len={ultra_len})"
        );
        // Report the exact percentage so it appears in test output.
        eprintln!(
            "strip_metadata_pos_attrs reduction: {reduction_pct:.1}% \
                   ({standard_len} -> {ultra_len} bytes)"
        );
    }
}
