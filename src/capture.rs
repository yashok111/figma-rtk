//! Fixture capture: dump raw target tool-result bodies so phase-2 (aggressive,
//! payload-shape-aware) compression can be developed and TDD'd against real
//! Figma responses. Only the response body is written — never request headers
//! or the Bearer token.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::fsutil::{create_private_new, safe_name};

/// Write a raw payload to the next free `dir/<tool>-NNNN.json` slot. The index is
/// chosen by scanning existing files for that tool, so it survives a process
/// restart without overwriting earlier fixtures (a plain per-process counter
/// restarted at 0 and clobbered them). Exclusive create + advance-on-collision
/// makes it safe under concurrent writers too. `tool` is sanitized so it can't
/// escape `dir`; the file is owner-only.
pub fn save_next(dir: &Path, tool: &str, raw: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = safe_name(tool);
    let mut seq = next_seq(dir, &name);
    // Bound the scan so a pathological directory (every index taken, or entries
    // that always collide) can't spin forever — capture is best-effort.
    for _ in 0..MAX_PROBE {
        let path = dir.join(format!("{name}-{seq:04}.json"));
        match create_private_new(&path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(raw) {
                    // Don't leave a zero/partial fixture behind on a write error.
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                return Ok(path);
            }
            // Lost the race for this index (or a leftover file) — try the next.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if seq == u64::MAX {
                    break; // cannot advance further
                }
                seq += 1;
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free capture index",
    ))
}

/// Upper bound on index probes in [`save_next`] (far above any real fixture count).
const MAX_PROBE: u64 = 1_000_000;

/// Highest existing `<name>-NNNN.json` index in `dir`, plus one (0 if none).
fn next_seq(dir: &Path, name: &str) -> u64 {
    let prefix = format!("{name}-");
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|f| {
            f.strip_prefix(&prefix)
                .and_then(|r| r.strip_suffix(".json"))
                .and_then(|n| n.parse::<u64>().ok())
        })
        .max()
        .map_or(0, |m| m.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Per-test unique dir (sibling, never nested) so parallel tests can't delete
    // each other's directory via remove_dir_all.
    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("frtk-capture-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn writes_payload_to_named_file() {
        let dir = tmp("writes");
        let _ = std::fs::remove_dir_all(&dir);
        let path = save_next(&dir, "get_design_context", b"{\"x\":1}").unwrap();
        assert!(path.ends_with("get_design_context-0000.json"));
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"x\":1}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitizes_tool_name() {
        let dir = tmp("sani");
        let _ = std::fs::remove_dir_all(&dir);
        let path = save_next(&dir, "../evil/name", b"x").unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(name, "___evil_name-0000.json");
        assert_eq!(path.parent().unwrap(), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_next_advances_per_tool_and_never_overwrites() {
        let dir = tmp("next");
        let _ = std::fs::remove_dir_all(&dir);
        let p0 = save_next(&dir, "get_design_context", b"a").unwrap();
        let p1 = save_next(&dir, "get_design_context", b"b").unwrap();
        // A 3rd call here stands in for a process restart: the index is re-derived
        // by scanning the dir, so it must continue, not clobber 0000.
        let p2 = save_next(&dir, "get_design_context", b"c").unwrap();
        assert!(p0.ends_with("get_design_context-0000.json"));
        assert!(p1.ends_with("get_design_context-0001.json"));
        assert!(p2.ends_with("get_design_context-0002.json"));
        assert_eq!(std::fs::read(&p0).unwrap(), b"a", "earlier fixture intact");
        assert_eq!(std::fs::read(&p1).unwrap(), b"b", "earlier fixture intact");
        // A different tool keeps its own independent sequence.
        let q0 = save_next(&dir, "get_metadata", b"m").unwrap();
        assert!(q0.ends_with("get_metadata-0000.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
