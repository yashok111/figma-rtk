//! Fixture capture: dump raw target tool-result bodies so phase-2 (aggressive,
//! payload-shape-aware) compression can be developed and TDD'd against real
//! Figma responses. Only the response body is written — never request headers
//! or the Bearer token.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::fsutil::{create_private, safe_name};

/// Write a raw payload to `dir/<tool>-<seq>.json`. Creates `dir` if missing.
/// `tool` is sanitized so it can't escape the directory; the file is owner-only.
pub fn save(dir: &Path, tool: &str, seq: u64, raw: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}-{seq:04}.json", safe_name(tool)));
    let mut f = create_private(&path)?;
    f.write_all(raw)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("frtk-capture-test-{}", std::process::id()))
    }

    #[test]
    fn writes_payload_to_named_file() {
        let dir = tmp();
        let _ = std::fs::remove_dir_all(&dir);
        let path = save(&dir, "get_design_context", 0, b"{\"x\":1}").unwrap();
        assert!(path.ends_with("get_design_context-0000.json"));
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"x\":1}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitizes_tool_name() {
        let dir = tmp().join("sani");
        let _ = std::fs::remove_dir_all(&dir);
        let path = save(&dir, "../evil/name", 3, b"x").unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(name, "___evil_name-0003.json");
        assert_eq!(path.parent().unwrap(), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
