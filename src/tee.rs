//! Raw-payload recovery (RTK's `tee` idea). When enabled, the last raw upstream
//! body for each target tool is kept on disk so aggressive compression stays
//! safe — the full, uncompressed payload is always retrievable.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::TeeMode;
use crate::fsutil::{create_private, safe_name};

/// Save `raw` as the last-known body for `tool` (overwriting the previous one),
/// unless the mode is `Never`. Best-effort; returns the path written.
pub fn maybe_save(mode: TeeMode, dir: &Path, tool: &str, raw: &[u8]) -> Option<PathBuf> {
    if matches!(mode, TeeMode::Never) {
        return None;
    }
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("{}.raw", safe_name(tool)));
    let mut f = create_private(&path).ok()?;
    f.write_all(raw).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("frtk-tee-{}", std::process::id()))
    }

    #[test]
    fn never_writes_nothing() {
        let dir = tmp().join("never");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(maybe_save(TeeMode::Never, &dir, "get_metadata", b"x").is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn always_writes_and_overwrites() {
        let dir = tmp().join("always");
        let _ = std::fs::remove_dir_all(&dir);
        let p1 = maybe_save(TeeMode::Always, &dir, "get_metadata", b"first").unwrap();
        assert_eq!(std::fs::read(&p1).unwrap(), b"first");
        let p2 = maybe_save(TeeMode::Always, &dir, "get_metadata", b"second").unwrap();
        assert_eq!(p1, p2, "same tool overwrites the same file");
        assert_eq!(std::fs::read(&p2).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
