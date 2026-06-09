//! Token-savings ledger. Each compressed response appends one JSON line to
//! `$XDG_DATA_HOME/figma-rtk/ledger.jsonl`; `frtk gain` aggregates it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::tokens;

/// Default price per token in USD. Anchored to Claude Sonnet (input tier).
/// Users may override via `FRTK_TOKEN_PRICE` env var.
///
/// NOTE: this is intentionally approximate — input price varies ~3x across
/// Haiku / Sonnet / Opus. Use it as a rough order-of-magnitude estimate only.
pub const TOKEN_PRICE_DEFAULT: f64 = 3e-6;

/// Resolve the per-token price from an optional env value string.
/// Falls back to `TOKEN_PRICE_DEFAULT` if `val` is `None` or fails to parse
/// as a positive `f64`.
pub fn resolve_token_price(val: Option<&str>) -> f64 {
    val.and_then(|s| s.parse::<f64>().ok())
        .filter(|&p| p > 0.0)
        .unwrap_or(TOKEN_PRICE_DEFAULT)
}

/// Convert a token count to a dollar cost at the given per-token price.
pub fn tokens_to_dollars(tokens: usize, price_per_token: f64) -> f64 {
    tokens as f64 * price_per_token
}

/// Parse a duration string like "90s", "30m", "1h", "1d" into seconds.
/// The suffix MUST be one of: s, m, h, d. An unknown, missing, or empty
/// suffix is an error.
pub fn parse_duration(s: &str) -> anyhow::Result<u64> {
    if s.is_empty() {
        anyhow::bail!("empty duration string");
    }
    // Split on the last *char* (not byte) so a multi-byte trailing char (e.g.
    // "5ñ") yields a clean error instead of panicking in `split_at`.
    let suffix = s.chars().next_back().expect("non-empty checked above");
    let num_part = &s[..s.len() - suffix.len_utf8()];
    let n: u64 = num_part
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration {:?}: expected <integer><s|m|h|d>", s))?;
    let multiplier = match suffix {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        other => anyhow::bail!(
            "unknown duration suffix {:?} in {:?}: use s/m/h/d",
            other,
            s
        ),
    };
    Ok(n * multiplier)
}

/// Filter a slice of records to only those whose `ts` is >= `cutoff_secs`.
/// Records with `ts == 0` (legacy) are always included (they predate the field).
pub fn filter_by_cutoff(recs: &[StatRec], cutoff_secs: u64) -> Vec<&StatRec> {
    recs.iter()
        .filter(|r| r.ts == 0 || r.ts >= cutoff_secs)
        .collect()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StatRec {
    pub tool: String,
    pub before: usize,
    pub after: usize,
    #[serde(default)]
    pub ts: u64,
    /// Content-aware token estimate for the before-text (computed via
    /// `tokens::est_str` at compress time). `None` for legacy ledger records
    /// written before this field existed — callers must fall back to
    /// `tokens::est(before)` in that case. Use `Option<usize>` + `serde(default)`
    /// so old JSONL lines without the field deserialise without error.
    #[serde(default)]
    pub tok_before: Option<usize>,
    /// Content-aware token estimate for the after-text (post-compress). `None`
    /// for legacy records; fall back to `tokens::est(after)`.
    #[serde(default)]
    pub tok_after: Option<usize>,
    /// Total upstream time from request send to full response body received, in
    /// milliseconds. Includes headers arrival and body transfer — not only the
    /// header round-trip. Zero for records written before this field existed
    /// (serde default).
    #[serde(default)]
    pub upstream_ms: u64,
    /// Compression level in effect when this record was written (e.g.
    /// `"Standard"`, `"Aggressive"`, `"Ultra"`). Empty string for legacy
    /// records (serde default).
    #[serde(default)]
    pub level: String,
}

fn ledger_path() -> PathBuf {
    // `FRTK_LEDGER` lets tests (and power users) redirect the ledger.
    if let Ok(p) = std::env::var("FRTK_LEDGER") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("figma-rtk")
        .join("ledger.jsonl")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append a batch of savings records. Best-effort: a failed write never breaks
/// the proxy path.
pub fn record_all(mut recs: Vec<StatRec>) {
    if recs.is_empty() {
        return;
    }
    let ts = now_secs();
    let path = ledger_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    for r in recs.iter_mut() {
        r.ts = ts;
        if let Ok(line) = serde_json::to_string(r) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn print_gain(history: bool, since: Option<u64>) -> anyhow::Result<()> {
    let path = ledger_path();
    let data = std::fs::read_to_string(&path).unwrap_or_default();
    let all_recs: Vec<StatRec> = data
        .lines()
        .filter_map(|l| serde_json::from_str::<StatRec>(l).ok())
        .collect();

    // Apply --since cutoff if requested.
    let cutoff = since.map(|dur_secs| now_secs().saturating_sub(dur_secs));
    let recs: Vec<&StatRec> = match cutoff {
        Some(c) => filter_by_cutoff(&all_recs, c),
        None => all_recs.iter().collect(),
    };

    if recs.is_empty() {
        if all_recs.is_empty() {
            println!("No savings recorded yet.");
            println!("Start the proxy (`frtk serve`), point your agent at it (`frtk config`),");
            println!("then use the Figma read tools.");
        } else {
            // The ledger has data, but nothing inside the --since window.
            println!("No records in the requested window (try a wider --since).");
        }
        return Ok(());
    }

    // Resolve per-token price from env (or use default).
    let price = resolve_token_price(std::env::var("FRTK_TOKEN_PRICE").ok().as_deref());

    // Byte totals are always summed (for the byte columns).
    // Token totals are computed per-record: use the stored content-aware fields
    // when present (new records), fall back to est(bytes) for legacy records.
    let (mut tot_before, mut tot_after, mut calls) = (0usize, 0usize, 0usize);
    let (mut tok_tot_before, mut tok_tot_after) = (0usize, 0usize);
    // by_tool: (bytes_before, bytes_after, call_count, tok_before, tok_after)
    let mut by_tool: BTreeMap<String, (usize, usize, usize, usize, usize)> = BTreeMap::new();
    // Track whether any image/volume records exist (tok_before == tok_after == 0
    // and before == after, meaning no savings — these are volume-tracking entries).
    let mut has_image_records = false;
    for r in &recs {
        tot_before += r.before;
        tot_after += r.after;
        let rtb = r.tok_before.unwrap_or_else(|| tokens::est(r.before));
        let rta = r.tok_after.unwrap_or_else(|| tokens::est(r.after));
        tok_tot_before += rtb;
        tok_tot_after += rta;
        calls += 1;
        // Detect image/volume records: tok_before == Some(0) && tok_after == Some(0)
        // && before == after (set by transform_msg for image-only responses). The
        // `before > 0` guard excludes empty (0-byte) structural-mutation records,
        // which also satisfy the equality but carry no image volume to note.
        if r.tok_before == Some(0) && r.tok_after == Some(0) && r.before == r.after && r.before > 0
        {
            has_image_records = true;
        }
        let e = by_tool.entry(r.tool.clone()).or_default();
        e.0 += r.before;
        e.1 += r.after;
        e.2 += 1;
        e.3 += rtb;
        e.4 += rta;
    }

    let tok_saved = tok_tot_before.saturating_sub(tok_tot_after);
    let dollars_saved = tokens_to_dollars(tok_saved, price);
    let pct = tokens::pct_saved(tot_before, tot_after);

    println!("figma-rtk gain");
    println!("──────────────────────────────────────────────");
    println!("compressed calls : {calls}");
    println!("bytes  before    : {tot_before}");
    println!("bytes  after     : {tot_after}");
    println!(
        "tokens saved (~) : {tok_saved}  ({tok_tot_before} -> {tok_tot_after})  ~${dollars_saved:.4}"
    );
    println!("reduction        : {pct:.1}%");
    println!();
    println!("by tool:");
    for (tool, (b, a, c, tb, ta)) in &by_tool {
        let tp = tokens::pct_saved(*b, *a);
        let ts = tb.saturating_sub(*ta);
        let tool_dollars = tokens_to_dollars(ts, price);
        println!("  {tool:<22} calls {c:>4}  ~{ts:>8} tok  ~${tool_dollars:.4}  ({tp:.1}%)");
    }

    println!();
    println!("note: $ is a ROUGH estimate anchored to one model tier (Sonnet input ~$3/Mtok).");
    println!(
        "      Actual cost varies ~3x across Haiku/Sonnet/Opus. Override via FRTK_TOKEN_PRICE."
    );

    if has_image_records {
        println!();
        println!("note: image/screenshot bytes are VOLUME, not byte/4 token cost —");
        println!("      vision tokens are priced on pixels, so they must not be read");
        println!("      as token savings. Image records show 0 tokens saved by design.");
    }

    if history {
        println!();
        println!("recent calls:");
        for r in recs.iter().rev().take(20) {
            let tb = r.tok_before.unwrap_or_else(|| tokens::est(r.before));
            let ta = r.tok_after.unwrap_or_else(|| tokens::est(r.after));
            let ts = tb.saturating_sub(ta);
            println!("  [{}] {:<22} ~{} tok saved", r.ts, r.tool, ts);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A StatRec with no token fields (simulates a legacy ledger line written
    /// before tok_before/tok_after were added).
    fn legacy_rec(tool: &str, before: usize, after: usize) -> StatRec {
        StatRec {
            tool: tool.to_string(),
            before,
            after,
            ts: 0,
            tok_before: None,
            tok_after: None,
            upstream_ms: 0,
            level: String::new(),
        }
    }

    /// A StatRec with explicit token fields (new-style record).
    fn token_rec(tool: &str, before: usize, after: usize, tb: usize, ta: usize) -> StatRec {
        StatRec {
            tool: tool.to_string(),
            before,
            after,
            ts: 0,
            tok_before: Some(tb),
            tok_after: Some(ta),
            upstream_ms: 0,
            level: String::new(),
        }
    }

    #[test]
    fn legacy_fallback_uses_est_bytes() {
        // A record with tok_before/tok_after == None must still aggregate via
        // tokens::est(bytes). Verify the None fields deserialise from JSON without
        // the key present (serde default).
        let json = r#"{"tool":"get_metadata","before":400,"after":100,"ts":0}"#;
        let r: StatRec = serde_json::from_str(json).unwrap();
        assert!(r.tok_before.is_none(), "legacy record has no tok_before");
        assert!(r.tok_after.is_none(), "legacy record has no tok_after");
        // est fallback: 400 bytes / 4 = 100 tokens
        let tb = r.tok_before.unwrap_or_else(|| tokens::est(r.before));
        let ta = r.tok_after.unwrap_or_else(|| tokens::est(r.after));
        assert_eq!(tb, 100);
        assert_eq!(ta, 25);
    }

    #[test]
    fn new_style_record_uses_stored_tokens() {
        // When tok_before / tok_after are present, they must be preferred over
        // est(bytes) — even when the byte-based estimate would differ.
        let r = token_rec("get_metadata", 400, 100, 80, 18);
        let tb = r.tok_before.unwrap_or_else(|| tokens::est(r.before));
        let ta = r.tok_after.unwrap_or_else(|| tokens::est(r.after));
        assert_eq!(tb, 80, "stored tok_before used");
        assert_eq!(ta, 18, "stored tok_after used");
    }

    #[test]
    fn mixed_legacy_and_new_aggregate_correctly() {
        // A mix of legacy (None) and new (Some) records must aggregate correctly:
        // legacy records contribute via est(bytes), new records via their stored
        // fields. Both contribute their byte counts.
        let recs = vec![
            legacy_rec("get_metadata", 400, 100), // est(400)=100, est(100)=25
            token_rec("get_metadata", 800, 200, 60, 15), // stored: 60, 15
        ];
        let mut tok_b = 0usize;
        let mut tok_a = 0usize;
        for r in &recs {
            tok_b += r.tok_before.unwrap_or_else(|| tokens::est(r.before));
            tok_a += r.tok_after.unwrap_or_else(|| tokens::est(r.after));
        }
        assert_eq!(tok_b, 100 + 60, "legacy est + new stored");
        assert_eq!(tok_a, 25 + 15, "legacy est + new stored");
    }

    #[test]
    fn stat_rec_roundtrips_with_token_fields() {
        // Serialise and deserialise a new-style record; fields must survive.
        let r = token_rec("get_design_context", 1000, 300, 75, 20);
        let json = serde_json::to_string(&r).unwrap();
        let back: StatRec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tok_before, Some(75));
        assert_eq!(back.tok_after, Some(20));
    }

    #[test]
    fn stat_rec_roundtrips_with_new_timing_fields() {
        // A StatRec with upstream_ms and level must survive a JSON round-trip.
        let r = StatRec {
            tool: "get_metadata".to_string(),
            before: 500,
            after: 120,
            ts: 0,
            tok_before: Some(40),
            tok_after: Some(10),
            upstream_ms: 87,
            level: "Standard".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: StatRec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.upstream_ms, 87, "upstream_ms must survive round-trip");
        assert_eq!(back.level, "Standard", "level must survive round-trip");
    }

    #[test]
    fn image_volume_record_contributes_zero_token_savings() {
        // An image/volume record has tok_before==Some(0), tok_after==Some(0),
        // before==after. When aggregated it must contribute 0 token savings so
        // it never inflates the "tokens saved" figure in print_gain.
        let img_rec = StatRec {
            tool: "get_screenshot".to_string(),
            before: 5000,
            after: 5000,
            ts: 0,
            tok_before: Some(0),
            tok_after: Some(0),
            upstream_ms: 0,
            level: String::new(),
        };
        let rtb = img_rec
            .tok_before
            .unwrap_or_else(|| tokens::est(img_rec.before));
        let rta = img_rec
            .tok_after
            .unwrap_or_else(|| tokens::est(img_rec.after));
        assert_eq!(rtb, 0, "image record tok_before == 0");
        assert_eq!(rta, 0, "image record tok_after == 0");
        assert_eq!(rtb.saturating_sub(rta), 0, "zero token savings");
    }

    #[test]
    fn old_json_without_timing_fields_deserializes_with_defaults() {
        // A JSON line written before upstream_ms / level existed must still
        // deserialise cleanly, with those fields zero / empty.
        let json = r#"{"tool":"get_metadata","before":400,"after":100,"ts":0}"#;
        let r: StatRec = serde_json::from_str(json).unwrap();
        assert_eq!(r.upstream_ms, 0, "missing upstream_ms defaults to 0");
        assert_eq!(r.level, "", "missing level defaults to empty string");
    }

    // --- price / dollar helpers ---

    #[test]
    fn tokens_to_dollars_basic() {
        // 1 000 000 tokens at 3e-6 USD/tok = $3.00
        let d = tokens_to_dollars(1_000_000, 3e-6);
        assert!((d - 3.0).abs() < 1e-9, "expected $3.00, got {d}");
    }

    #[test]
    fn tokens_to_dollars_zero() {
        assert_eq!(tokens_to_dollars(0, 3e-6), 0.0);
    }

    #[test]
    fn resolve_token_price_defaults() {
        // None -> TOKEN_PRICE_DEFAULT
        let p = resolve_token_price(None);
        assert!(
            (p - TOKEN_PRICE_DEFAULT).abs() < 1e-15,
            "expected default price"
        );
    }

    #[test]
    fn resolve_token_price_valid_override() {
        let p = resolve_token_price(Some("1e-5"));
        assert!((p - 1e-5).abs() < 1e-15, "expected 1e-5, got {p}");
    }

    #[test]
    fn resolve_token_price_invalid_falls_back_to_default() {
        // Garbage string -> default
        let p = resolve_token_price(Some("not_a_number"));
        assert!(
            (p - TOKEN_PRICE_DEFAULT).abs() < 1e-15,
            "expected default price on bad input"
        );
    }

    #[test]
    fn resolve_token_price_zero_falls_back_to_default() {
        // Zero price is nonsensical -> default
        let p = resolve_token_price(Some("0"));
        assert!(
            (p - TOKEN_PRICE_DEFAULT).abs() < 1e-15,
            "expected default price for zero"
        );
    }

    #[test]
    fn resolve_token_price_negative_falls_back_to_default() {
        let p = resolve_token_price(Some("-1e-6"));
        assert!(
            (p - TOKEN_PRICE_DEFAULT).abs() < 1e-15,
            "expected default for negative"
        );
    }

    // --- parse_duration ---

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("90s").unwrap(), 90);
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("30m").unwrap(), 30 * 60);
    }

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration("1h").unwrap(), 3600);
    }

    #[test]
    fn parse_duration_days() {
        assert_eq!(parse_duration("1d").unwrap(), 86400);
    }

    #[test]
    fn parse_duration_unknown_suffix_is_error() {
        assert!(
            parse_duration("10x").is_err(),
            "unknown suffix should be an error"
        );
    }

    #[test]
    fn parse_duration_no_suffix_is_error() {
        // "42" has no suffix (last char is a digit, not s/m/h/d)
        assert!(
            parse_duration("42").is_err(),
            "missing suffix should be an error"
        );
    }

    #[test]
    fn parse_duration_empty_is_error() {
        assert!(
            parse_duration("").is_err(),
            "empty string should be an error"
        );
    }

    #[test]
    fn parse_duration_multibyte_suffix_errs_not_panics() {
        // A multi-byte trailing char must yield an error, never panic in split_at
        // on a non-char-boundary byte index.
        assert!(
            parse_duration("5ñ").is_err(),
            "multibyte suffix should be an error"
        );
        assert!(
            parse_duration("5日").is_err(),
            "CJK suffix should be an error"
        );
    }

    // --- filter_by_cutoff ---

    fn make_rec(ts: u64) -> StatRec {
        StatRec {
            tool: "t".to_string(),
            before: 100,
            after: 50,
            ts,
            tok_before: None,
            tok_after: None,
            upstream_ms: 0,
            level: String::new(),
        }
    }

    #[test]
    fn filter_by_cutoff_keeps_recent() {
        let recs = vec![make_rec(1000), make_rec(2000), make_rec(3000)];
        let filtered = filter_by_cutoff(&recs, 1500);
        assert_eq!(filtered.len(), 2, "only records at ts>=1500 should survive");
        assert_eq!(filtered[0].ts, 2000);
        assert_eq!(filtered[1].ts, 3000);
    }

    #[test]
    fn filter_by_cutoff_excludes_old() {
        let recs = vec![make_rec(100), make_rec(200)];
        let filtered = filter_by_cutoff(&recs, 500);
        assert!(filtered.is_empty(), "all old records should be excluded");
    }

    #[test]
    fn filter_by_cutoff_legacy_ts_zero_always_included() {
        // ts==0 records (legacy, no timestamp) must always be kept.
        let recs = vec![make_rec(0), make_rec(100)];
        let filtered = filter_by_cutoff(&recs, 9999);
        assert_eq!(filtered.len(), 1, "legacy ts==0 is always included");
        assert_eq!(filtered[0].ts, 0);
    }
}
