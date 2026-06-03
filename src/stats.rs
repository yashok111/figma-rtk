//! Token-savings ledger. Each compressed response appends one JSON line to
//! `$XDG_DATA_HOME/figma-rtk/ledger.jsonl`; `frtk gain` aggregates it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::tokens;

#[derive(Serialize, Deserialize, Clone)]
pub struct StatRec {
    pub tool: String,
    pub before: usize,
    pub after: usize,
    #[serde(default)]
    pub ts: u64,
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
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    for r in recs.iter_mut() {
        r.ts = ts;
        if let Ok(line) = serde_json::to_string(r) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn print_gain(history: bool) -> anyhow::Result<()> {
    let path = ledger_path();
    let data = std::fs::read_to_string(&path).unwrap_or_default();
    let recs: Vec<StatRec> = data
        .lines()
        .filter_map(|l| serde_json::from_str::<StatRec>(l).ok())
        .collect();

    if recs.is_empty() {
        println!("No savings recorded yet.");
        println!("Start the proxy (`frtk serve`), point Claude Code at it (`frtk config`),");
        println!("then use the Figma read tools.");
        return Ok(());
    }

    let (mut tot_before, mut tot_after, mut calls) = (0usize, 0usize, 0usize);
    let mut by_tool: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for r in &recs {
        tot_before += r.before;
        tot_after += r.after;
        calls += 1;
        let e = by_tool.entry(r.tool.clone()).or_default();
        e.0 += r.before;
        e.1 += r.after;
        e.2 += 1;
    }

    let saved_bytes = tot_before.saturating_sub(tot_after);
    let tok_before = tokens::est(tot_before);
    let tok_after = tokens::est(tot_after);
    let tok_saved = tok_before.saturating_sub(tok_after);
    let pct = if tot_before > 0 {
        100.0 * saved_bytes as f64 / tot_before as f64
    } else {
        0.0
    };

    println!("figma-rtk gain");
    println!("──────────────────────────────────────────────");
    println!("compressed calls : {calls}");
    println!("bytes  before    : {tot_before}");
    println!("bytes  after     : {tot_after}");
    println!("tokens saved (~) : {tok_saved}  ({tok_before} -> {tok_after})");
    println!("reduction        : {pct:.1}%");
    println!();
    println!("by tool:");
    for (tool, (b, a, c)) in &by_tool {
        let tp = if *b > 0 {
            100.0 * (b.saturating_sub(*a)) as f64 / *b as f64
        } else {
            0.0
        };
        let ts = tokens::est(*b).saturating_sub(tokens::est(*a));
        println!("  {tool:<22} calls {c:>4}  ~{ts:>8} tok  ({tp:.1}%)");
    }

    if history {
        println!();
        println!("recent calls:");
        for r in recs.iter().rev().take(20) {
            let ts = tokens::est(r.before).saturating_sub(tokens::est(r.after));
            println!("  [{}] {:<22} ~{} tok saved", r.ts, r.tool, ts);
        }
    }

    Ok(())
}
