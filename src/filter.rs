//! Declarative, user-authored, inline-tested compression filters (RTK's TOML
//! filter idea). A filter strips redundant structure from a tool's JSON result
//! at `aggressive`/`ultra` level. Because they can drop data the agent might
//! need, filters are (a) opt-in via level, (b) trust-gated when project-local
//! (see [`crate::trust`]), and (c) carry inline tests runnable via `frtk verify`.
//!
//! ```toml
//! [[filter]]
//! name = "design-context-lean"
//! tools = ["get_design_context"]
//! drop_keys = ["vectorPaths", "absoluteRenderBounds"]
//! drop_where = [{ key = "visible", equals = false }]
//! max_depth = 8
//!
//! [[filter.tests]]
//! name = "drops hidden nodes and vector paths"
//! input = '{"children":[{"visible":false},{"name":"keep","vectorPaths":[1]}]}'
//! absent = ["visible", "vectorPaths"]
//! present = ["keep"]
//! ```

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// A predicate for dropping an array element by one of its own top-level fields.
/// At least one of `equals` / `starts_with` should be set; if both are, the
/// element is dropped when EITHER matches. An empty predicate (neither set)
/// matches nothing, so a malformed rule fails safe (drops nothing).
#[derive(Debug, Clone, Deserialize)]
pub struct DropWhere {
    pub key: String,
    /// Drop when the field equals this value (int/float-equal aware).
    #[serde(default)]
    pub equals: Option<Value>,
    /// Drop when the field is a string starting with this prefix. Anchored at
    /// the start so it targets a stable leading marker without matching a
    /// substring buried in legitimate content.
    #[serde(default)]
    pub starts_with: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FilterTest {
    pub name: String,
    pub input: String,
    #[serde(default)]
    pub absent: Vec<String>,
    #[serde(default)]
    pub present: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Filter {
    pub name: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub drop_keys: Vec<String>,
    #[serde(default)]
    pub drop_where: Vec<DropWhere>,
    /// Replace objects/arrays at this depth or deeper with `"…"`. Root is depth
    /// 0, so `max_depth = 1` keeps only top-level keys.
    pub max_depth: Option<usize>,
    #[serde(default)]
    pub tests: Vec<FilterTest>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FilterFile {
    #[serde(default, rename = "filter")]
    filters: Vec<Filter>,
}

#[derive(Debug, Clone, Default)]
pub struct FilterSet {
    pub filters: Vec<Filter>,
}

impl FilterSet {
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    pub fn len(&self) -> usize {
        self.filters.len()
    }

    /// Parse filters from a TOML string.
    pub fn parse(s: &str) -> anyhow::Result<FilterSet> {
        let ff: FilterFile = toml::from_str(s)?;
        Ok(FilterSet { filters: ff.filters })
    }

    /// Load filters from a TOML file (missing file = empty set).
    pub fn load(path: &Path) -> anyhow::Result<FilterSet> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FilterSet::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn extend(&mut self, other: FilterSet) {
        self.filters.extend(other.filters);
    }

    fn for_tool(&self, tool: &str) -> Vec<&Filter> {
        self.filters
            .iter()
            .filter(|f| f.tools.iter().any(|t| matches_tool(tool, t)))
            .collect()
    }

    /// Apply every filter matching `tool` to `text` (must be JSON). Returns the
    /// transformed, minified JSON, or `None` if no filter matched or the text
    /// is not JSON (caller then falls back to whitespace-only compression).
    /// Apply every filter matching `tool` to a JSON value in place, structurally
    /// — dropping whole array elements (`drop_where`), object keys (`drop_keys`),
    /// and truncating depth (`max_depth`) over the value's own structure. This is
    /// distinct from [`apply_text`](Self::apply_text), which parses a single text
    /// field as JSON: `apply_structural` operates on the result envelope itself,
    /// so a `get_design_context` filter can drop whole `content[]` boilerplate
    /// blocks (whose text is React code, not JSON) and the `_meta` key.
    pub fn apply_structural(&self, tool: &str, v: &mut Value) {
        for f in self.for_tool(tool) {
            apply_value(v, f, 0);
        }
    }

    pub fn apply_text(&self, tool: &str, text: &str) -> Option<String> {
        let matched = self.for_tool(tool);
        if matched.is_empty() {
            return None;
        }
        let mut v: Value = serde_json::from_str(text.trim()).ok()?;
        for f in matched {
            apply_value(&mut v, f, 0);
        }
        serde_json::to_string(&v).ok()
    }
}

/// Match a wire tool name against a filter's declared tool: exact, or the wire
/// name carries an MCP namespace prefix ending in `__<tool>`. The `__` boundary
/// stops a short name like "a" or "metadata" matching "get_metadata".
/// Zero-alloc: no heap allocation; uses `strip_suffix` + a slice check.
pub(crate) fn matches_tool(wire: &str, filter_tool: &str) -> bool {
    wire == filter_tool
        || wire
            .strip_suffix(filter_tool)
            .is_some_and(|prefix| prefix.ends_with("__"))
}

/// JSON value equality that treats integers and floats of equal value as equal,
/// so a TOML `equals = 0` matches a JSON `0.0` (TOML has no int/float ambiguity).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn matches_drop_where(el: &Value, f: &Filter) -> bool {
    // Shallow: only the array element's own top-level field is examined. A rule
    // matches when the field equals `equals` OR (string) starts with `starts_with`.
    f.drop_where.iter().any(|w| {
        el.get(&w.key).is_some_and(|v| {
            let by_eq = w.equals.as_ref().is_some_and(|e| json_eq(v, e));
            let by_prefix = w
                .starts_with
                .as_ref()
                .is_some_and(|p| v.as_str().is_some_and(|s| s.starts_with(p.as_str())));
            by_eq || by_prefix
        })
    })
}

/// Recursively apply one filter's rules to a JSON value, in place.
pub fn apply_value(v: &mut Value, f: &Filter, depth: usize) {
    if let Some(md) = f.max_depth {
        // Root is depth 0; objects/arrays at depth >= max_depth are truncated.
        if depth >= md {
            *v = Value::String("…".to_string());
            return;
        }
    }
    match v {
        Value::Object(map) => {
            map.retain(|k, _| !f.drop_keys.iter().any(|d| d == k));
            for val in map.values_mut() {
                apply_value(val, f, depth + 1);
            }
        }
        Value::Array(arr) => {
            arr.retain(|el| !matches_drop_where(el, f));
            for el in arr.iter_mut() {
                apply_value(el, f, depth + 1);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub filter: String,
    pub test: String,
    pub passed: bool,
    pub detail: String,
}

/// Run every filter's inline tests.
pub fn run_tests(filters: &[Filter]) -> Vec<TestResult> {
    let mut out = Vec::new();
    for f in filters {
        for t in &f.tests {
            out.push(run_one(f, t));
        }
    }
    out
}

fn run_one(f: &Filter, t: &FilterTest) -> TestResult {
    let mk = |passed, detail: String| TestResult {
        filter: f.name.clone(),
        test: t.name.clone(),
        passed,
        detail,
    };
    let mut v: Value = match serde_json::from_str(&t.input) {
        Ok(v) => v,
        Err(e) => return mk(false, format!("input is not valid JSON: {e}")),
    };
    apply_value(&mut v, f, 0);
    let out = serde_json::to_string(&v).unwrap_or_default();
    let mut problems = Vec::new();
    for a in &t.absent {
        if out.contains(a.as_str()) {
            problems.push(format!("should be absent but present: {a}"));
        }
    }
    for p in &t.present {
        if !out.contains(p.as_str()) {
            problems.push(format!("should be present but missing: {p}"));
        }
    }
    mk(problems.is_empty(), problems.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(toml: &str) -> Filter {
        FilterSet::parse(toml).unwrap().filters.pop().unwrap()
    }

    #[test]
    fn drop_keys_removes_recursively() {
        let f = filter("[[filter]]\nname=\"x\"\ntools=[\"t\"]\ndrop_keys=[\"vectorPaths\"]\n");
        let mut v: Value =
            serde_json::from_str(r#"{"a":{"vectorPaths":[1,2],"name":"n"},"vectorPaths":9}"#).unwrap();
        apply_value(&mut v, &f, 0);
        assert_eq!(v, serde_json::json!({"a":{"name":"n"}}));
    }

    #[test]
    fn drop_where_drops_hidden_array_elements() {
        let f = filter(
            "[[filter]]\nname=\"x\"\ntools=[\"t\"]\ndrop_where=[{key=\"visible\",equals=false}]\n",
        );
        let mut v: Value = serde_json::from_str(
            r#"{"children":[{"visible":false,"id":1},{"visible":true,"id":2}]}"#,
        )
        .unwrap();
        apply_value(&mut v, &f, 0);
        assert_eq!(v, serde_json::json!({"children":[{"visible":true,"id":2}]}));
    }

    #[test]
    fn max_depth_truncates_at_depth_or_deeper() {
        // root=depth0; max_depth=2 keeps two levels, truncates the third.
        let f2 = filter("[[filter]]\nname=\"x\"\ntools=[\"t\"]\nmax_depth=2\n");
        let mut v: Value = serde_json::from_str(r#"{"a":{"b":{"c":1}}}"#).unwrap();
        apply_value(&mut v, &f2, 0);
        assert_eq!(v, serde_json::json!({"a":{"b":"…"}}));
        // max_depth=1 keeps only top-level keys.
        let f1 = filter("[[filter]]\nname=\"x\"\ntools=[\"t\"]\nmax_depth=1\n");
        let mut v: Value = serde_json::from_str(r#"{"a":{"b":1}}"#).unwrap();
        apply_value(&mut v, &f1, 0);
        assert_eq!(v, serde_json::json!({"a":"…"}));
    }

    #[test]
    fn matches_tool_double_namespaced_and_negative() {
        // Positive: a double-namespaced wire name must match the bare tool name.
        assert!(
            matches_tool("mcp__plugin_figma_figma__get_metadata", "get_metadata"),
            "double-namespace must match"
        );
        // Negative: a name that only ends with the tool name but has no __ boundary must NOT match.
        assert!(
            !matches_tool("evil_get_metadata", "get_metadata"),
            "no __ boundary must NOT match"
        );
        // Negative: partial suffix without __ must not match.
        assert!(
            !matches_tool("set_metadata", "metadata"),
            "substring without __ must not match"
        );
        // Exact match still works.
        assert!(matches_tool("get_metadata", "get_metadata"), "exact match");
    }

    #[test]
    fn for_tool_matches_exact_and_namespaced_only() {
        let fs = FilterSet::parse("[[filter]]\nname=\"x\"\ntools=[\"get_metadata\"]\n").unwrap();
        assert_eq!(fs.for_tool("get_metadata").len(), 1);
        assert_eq!(fs.for_tool("mcp__plugin_figma_figma__get_metadata").len(), 1);
        assert_eq!(fs.for_tool("whoami").len(), 0);
        // short / substring names must NOT over-match
        let short = FilterSet::parse("[[filter]]\nname=\"x\"\ntools=[\"a\"]\n").unwrap();
        assert_eq!(short.for_tool("get_metadata").len(), 0);
        let sub = FilterSet::parse("[[filter]]\nname=\"x\"\ntools=[\"metadata\"]\n").unwrap();
        assert_eq!(sub.for_tool("set_metadata").len(), 0);
    }

    #[test]
    fn drop_where_matches_toml_int_against_json_float() {
        let f = filter(
            "[[filter]]\nname=\"x\"\ntools=[\"t\"]\ndrop_where=[{key=\"opacity\",equals=0}]\n",
        );
        let mut v: Value =
            serde_json::from_str(r#"{"a":[{"opacity":0.0,"id":1},{"opacity":1.0,"id":2}]}"#).unwrap();
        apply_value(&mut v, &f, 0);
        assert_eq!(v, serde_json::json!({"a":[{"opacity":1.0,"id":2}]}));
    }

    #[test]
    fn apply_text_minifies_and_returns_none_when_unmatched() {
        let fs = FilterSet::parse("[[filter]]\nname=\"x\"\ntools=[\"t\"]\ndrop_keys=[\"k\"]\n").unwrap();
        let out = fs.apply_text("t", r#"{ "k": 1, "keep": 2 }"#).unwrap();
        assert_eq!(out, r#"{"keep":2}"#);
        assert!(fs.apply_text("other", r#"{"k":1}"#).is_none());
    }

    #[test]
    fn drop_where_starts_with_drops_matching_string_elements() {
        let f = filter(
            "[[filter]]\nname=\"x\"\ntools=[\"t\"]\ndrop_where=[{key=\"text\",starts_with=\"BOILERPLATE\"}]\n",
        );
        let mut v: Value = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"BOILERPLATE: ignore me"},{"type":"text","text":"const keep = 1;"}]}"#,
        )
        .unwrap();
        apply_value(&mut v, &f, 0);
        // Only the prefixed element is dropped; the code element survives.
        assert_eq!(
            v,
            serde_json::json!({"content":[{"type":"text","text":"const keep = 1;"}]})
        );
    }

    #[test]
    fn drop_where_starts_with_ignores_nonstring_and_non_prefix() {
        let f = filter(
            "[[filter]]\nname=\"x\"\ntools=[\"t\"]\ndrop_where=[{key=\"text\",starts_with=\"X\"}]\n",
        );
        // numeric field (not a string) and a non-matching prefix both survive
        let mut v: Value =
            serde_json::from_str(r#"{"a":[{"text":5},{"text":"Yes"},{"text":"Xeno"}]}"#).unwrap();
        apply_value(&mut v, &f, 0);
        assert_eq!(v, serde_json::json!({"a":[{"text":5},{"text":"Yes"}]}));
    }

    #[test]
    fn drop_where_equals_still_works_after_becoming_optional() {
        let f = filter(
            "[[filter]]\nname=\"x\"\ntools=[\"t\"]\ndrop_where=[{key=\"visible\",equals=false}]\n",
        );
        let mut v: Value =
            serde_json::from_str(r#"{"c":[{"visible":false,"id":1},{"visible":true,"id":2}]}"#)
                .unwrap();
        apply_value(&mut v, &f, 0);
        assert_eq!(v, serde_json::json!({"c":[{"visible":true,"id":2}]}));
    }

    #[test]
    fn empty_drop_where_predicate_matches_nothing() {
        // A rule with neither equals nor starts_with must NOT drop everything.
        let f = filter("[[filter]]\nname=\"x\"\ntools=[\"t\"]\ndrop_where=[{key=\"text\"}]\n");
        let mut v: Value =
            serde_json::from_str(r#"{"a":[{"text":"keep1"},{"text":"keep2"}]}"#).unwrap();
        apply_value(&mut v, &f, 0);
        assert_eq!(v, serde_json::json!({"a":[{"text":"keep1"},{"text":"keep2"}]}));
    }

    #[test]
    fn apply_structural_drops_blocks_and_keys_over_envelope() {
        // Mirrors a get_design_context result envelope: a code block + boilerplate
        // text blocks + _meta. apply_structural drops the boilerplate blocks and
        // _meta while keeping the code block.
        let fs = FilterSet::parse(
            "[[filter]]\nname=\"dc\"\ntools=[\"get_design_context\"]\n\
             drop_keys=[\"_meta\"]\n\
             drop_where=[{key=\"text\",starts_with=\"SUPER CRITICAL\"}]\n",
        )
        .unwrap();
        let mut v: Value = serde_json::from_str(
            r#"{"_meta":{"mcpRequestId":"abc"},"content":[{"type":"text","text":"const x = 1;"},{"type":"text","text":"SUPER CRITICAL: convert the code"}]}"#,
        )
        .unwrap();
        fs.apply_structural("get_design_context", &mut v);
        assert_eq!(
            v,
            serde_json::json!({"content":[{"type":"text","text":"const x = 1;"}]})
        );
        // A non-matching tool leaves the value untouched.
        let mut v2: Value = serde_json::from_str(r#"{"_meta":{"x":1},"content":[]}"#).unwrap();
        fs.apply_structural("whoami", &mut v2);
        assert_eq!(v2, serde_json::json!({"_meta":{"x":1},"content":[]}));
    }

    #[test]
    fn inline_tests_run_pass_and_fail() {
        let fs = FilterSet::parse(
            "[[filter]]\nname=\"f\"\ntools=[\"t\"]\ndrop_keys=[\"secret\"]\n\
             [[filter.tests]]\nname=\"drops secret\"\ninput='{\"secret\":1,\"ok\":2}'\nabsent=[\"secret\"]\npresent=[\"ok\"]\n\
             [[filter.tests]]\nname=\"bad expectation\"\ninput='{\"secret\":1}'\npresent=[\"secret\"]\n",
        )
        .unwrap();
        let results = run_tests(&fs.filters);
        assert_eq!(results.len(), 2);
        assert!(results[0].passed, "first test should pass");
        assert!(!results[1].passed, "second expects a dropped key to be present");
    }
}
