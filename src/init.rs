//! `frtk init` — point Claude Code's Figma MCP server at the proxy by editing
//! the figma plugin's `.mcp.json`. The user runs this, so it sidesteps the
//! self-modification guard that blocks the agent from editing startup config.
//! A `.frtk-backup` is written so `--uninstall` restores the original exactly.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _};
use serde_json::Value;

use crate::fsutil;

const CANONICAL_URL: &str = "https://mcp.figma.com/mcp";

/// Parse a version dir name into numeric components so "2.10.0" > "2.9.0"
/// (lexicographic sort would get this wrong).
fn version_key(p: &Path) -> Vec<u32> {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.split('.').map(|seg| seg.parse::<u32>().unwrap_or(0)).collect())
        .unwrap_or_default()
}

/// Locate the figma plugin's `.mcp.json` (highest installed version).
pub fn discover_mcp_file() -> Option<PathBuf> {
    let base = dirs::home_dir()?.join(".claude/plugins/cache/claude-plugins-official/figma");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&base)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join(".mcp.json").is_file())
        .collect();
    versions.sort_by_key(|p| version_key(p));
    versions.pop().map(|p| p.join(".mcp.json"))
}

/// Write `content` to `file` atomically via the shared fsutil helper.
fn write_atomic(file: &Path, content: &str) -> anyhow::Result<()> {
    fsutil::write_atomic(file, content.as_bytes())
}

fn backup_path(file: &Path) -> PathBuf {
    let mut s: OsString = file.as_os_str().to_owned();
    s.push(".frtk-backup");
    PathBuf::from(s)
}

fn set_figma_url(v: &mut Value, url: &str) -> anyhow::Result<()> {
    let obj = v
        .get_mut("mcpServers")
        .and_then(|m| m.get_mut("figma"))
        .and_then(|f| f.as_object_mut())
        .ok_or_else(|| anyhow!("no mcpServers.figma object in the MCP config"))?;
    obj.insert("url".to_string(), Value::String(url.to_string()));
    Ok(())
}

fn write_json(file: &Path, v: &Value) -> anyhow::Result<()> {
    let mut s = serde_json::to_string_pretty(v)?;
    s.push('\n');
    write_atomic(file, &s)
}

/// Repoint `file`'s figma server at `http://127.0.0.1:<port>/mcp`, backing up
/// the original on first run.
pub fn install(file: &Path, port: u16) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("reading {}", file.display()))?;
    let backup = backup_path(file);
    if !backup.exists() {
        write_atomic(&backup, &content)?;
    }
    let mut v: Value = serde_json::from_str(&content)?;
    set_figma_url(&mut v, &format!("http://127.0.0.1:{port}/mcp"))?;
    write_json(file, &v)?;
    Ok(())
}

/// Restore the original config: from the backup if present, otherwise reset the
/// figma url to the canonical Figma endpoint.
pub fn uninstall(file: &Path) -> anyhow::Result<()> {
    let backup = backup_path(file);
    if backup.exists() {
        let content = std::fs::read_to_string(&backup)?;
        write_atomic(file, &content)?;
        std::fs::remove_file(&backup)?;
        return Ok(());
    }
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(file)?)?;
    set_figma_url(&mut v, CANONICAL_URL)?;
    write_json(file, &v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("frtk-init-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(".mcp.json");
        std::fs::write(
            &file,
            r#"{"mcpServers":{"figma":{"type":"http","url":"https://mcp.figma.com/mcp"}}}"#,
        )
        .unwrap();
        (dir, file)
    }

    #[test]
    fn install_sets_url_and_backs_up() {
        let (dir, file) = seed("install");
        install(&file, 7337).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["figma"]["url"], "http://127.0.0.1:7337/mcp");
        assert!(backup_path(&file).exists(), "backup created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_restores_original() {
        let (dir, file) = seed("uninstall");
        install(&file, 9000).unwrap();
        uninstall(&file).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["figma"]["url"], "https://mcp.figma.com/mcp");
        assert!(!backup_path(&file).exists(), "backup consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_key_orders_numerically() {
        // lexicographic would put 2.9.0 after 2.10.0; numeric must not.
        assert!(version_key(Path::new("x/2.10.0")) > version_key(Path::new("x/2.9.0")));
        assert!(version_key(Path::new("x/2.2.12")) > version_key(Path::new("x/2.2.9")));
    }

    #[test]
    fn install_is_idempotent_backup_preserved() {
        let (dir, file) = seed("idempotent");
        install(&file, 1).unwrap();
        let backup_before = std::fs::read_to_string(backup_path(&file)).unwrap();
        install(&file, 2).unwrap(); // second run must not overwrite backup with proxied url
        let backup_after = std::fs::read_to_string(backup_path(&file)).unwrap();
        assert_eq!(backup_before, backup_after, "backup keeps the original");
        assert!(backup_after.contains("mcp.figma.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
