//! `frtk init` — point an agent's Figma MCP server at the proxy. Claude Code is
//! wired by editing the figma plugin's `.mcp.json`; Codex is wired by editing
//! `$CODEX_HOME/config.toml` (or `~/.codex/config.toml`). The user runs this, so
//! it sidesteps the self-modification guard that blocks the agent from editing
//! startup config. A `.frtk-backup` is written so `--uninstall` restores the
//! original exactly.

use std::ffi::OsString;
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _};
use clap::ValueEnum;
use serde_json::Value;
use toml::map::Map;

use crate::fsutil;

const CANONICAL_URL: &str = "https://mcp.figma.com/mcp";

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Target {
    Codex,
    Claude,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Codex => f.write_str("codex"),
            Target::Claude => f.write_str("claude"),
        }
    }
}

/// Parse a version dir name into numeric components so "2.10.0" > "2.9.0"
/// (lexicographic sort would get this wrong).
fn version_key(p: &Path) -> Vec<u32> {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|s| {
            s.split('.')
                .map(|seg| seg.parse::<u32>().unwrap_or(0))
                .collect()
        })
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

/// Locate Codex's user config file.
pub fn discover_codex_config() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        return Some(PathBuf::from(home).join("config.toml"));
    }
    let path = dirs::home_dir()?.join(".codex/config.toml");
    Some(path)
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

fn write_toml(file: &Path, v: &toml::Value) -> anyhow::Result<()> {
    let mut s = toml::to_string_pretty(v)?;
    if !s.ends_with('\n') {
        s.push('\n');
    }
    write_atomic(file, &s)
}

fn set_codex_figma_url(v: &mut toml::Value, url: &str) -> anyhow::Result<()> {
    let root = v
        .as_table_mut()
        .ok_or_else(|| anyhow!("Codex config root must be a TOML table"))?;
    let mcp_servers = root
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(Map::new()));
    let mcp_servers = mcp_servers
        .as_table_mut()
        .ok_or_else(|| anyhow!("mcp_servers must be a TOML table"))?;
    let figma = mcp_servers
        .entry("figma".to_string())
        .or_insert_with(|| toml::Value::Table(Map::new()));
    let figma = figma
        .as_table_mut()
        .ok_or_else(|| anyhow!("mcp_servers.figma must be a TOML table"))?;
    figma.insert("url".to_string(), toml::Value::String(url.to_string()));
    Ok(())
}

/// Repoint `file`'s figma server at `http://127.0.0.1:<port>/mcp`, backing up
/// the original on first run.
pub fn install(file: &Path, port: u16) -> anyhow::Result<()> {
    let content =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let backup = backup_path(file);
    if !backup.exists() {
        write_atomic(&backup, &content)?;
    }
    let mut v: Value = serde_json::from_str(&content)?;
    set_figma_url(&mut v, &format!("http://127.0.0.1:{port}/mcp"))?;
    write_json(file, &v)?;
    Ok(())
}

/// Repoint Codex's `mcp_servers.figma` entry at the local proxy, backing up the
/// original config on first run.
pub fn install_codex(file: &Path, port: u16) -> anyhow::Result<()> {
    let content = match std::fs::read_to_string(file) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let backup = backup_path(file);
    if !backup.exists() {
        write_atomic(&backup, &content)?;
    }
    let mut v: toml::Value =
        toml::from_str(&content).with_context(|| format!("parsing TOML {}", file.display()))?;
    set_codex_figma_url(&mut v, &format!("http://127.0.0.1:{port}/mcp"))?;
    write_toml(file, &v)?;
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

/// Restore Codex config from backup, or reset `mcp_servers.figma.url` to the
/// canonical Figma MCP URL when no backup exists.
pub fn uninstall_codex(file: &Path) -> anyhow::Result<()> {
    let backup = backup_path(file);
    if backup.exists() {
        let content = std::fs::read_to_string(&backup)?;
        write_atomic(file, &content)?;
        std::fs::remove_file(&backup)?;
        return Ok(());
    }
    let mut v: toml::Value = toml::from_str(&std::fs::read_to_string(file)?)?;
    set_codex_figma_url(&mut v, CANONICAL_URL)?;
    write_toml(file, &v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_claude(name: &str) -> (PathBuf, PathBuf) {
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

    fn seed_codex(name: &str, content: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("frtk-init-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, content).unwrap();
        (dir, file)
    }

    #[test]
    fn install_sets_url_and_backs_up() {
        let (dir, file) = seed_claude("install");
        install(&file, 7337).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["figma"]["url"], "http://127.0.0.1:7337/mcp");
        assert!(backup_path(&file).exists(), "backup created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_restores_original() {
        let (dir, file) = seed_claude("uninstall");
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
        let (dir, file) = seed_claude("idempotent");
        install(&file, 1).unwrap();
        let backup_before = std::fs::read_to_string(backup_path(&file)).unwrap();
        install(&file, 2).unwrap(); // second run must not overwrite backup with proxied url
        let backup_after = std::fs::read_to_string(backup_path(&file)).unwrap();
        assert_eq!(backup_before, backup_after, "backup keeps the original");
        assert!(backup_after.contains("mcp.figma.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_install_adds_figma_mcp_server_and_backs_up() {
        let (dir, file) = seed_codex(
            "codex-install",
            r#"
model = "gpt-5"

[mcp_servers.node_repl]
command = "/Applications/Codex.app/Contents/Resources/node_repl"
args = []
"#,
        );
        install_codex(&file, 7337).unwrap();
        let content = std::fs::read_to_string(&file).unwrap();
        let v: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(
            v["mcp_servers"]["figma"]["url"].as_str(),
            Some("http://127.0.0.1:7337/mcp")
        );
        assert_eq!(
            v["mcp_servers"]["node_repl"]["command"].as_str(),
            Some("/Applications/Codex.app/Contents/Resources/node_repl")
        );
        assert!(backup_path(&file).exists(), "backup created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_install_updates_existing_figma_server_and_preserves_backup() {
        let (dir, file) = seed_codex(
            "codex-idempotent",
            r#"
[mcp_servers.figma]
url = "https://mcp.figma.com/mcp"
"#,
        );
        install_codex(&file, 7337).unwrap();
        let backup_before = std::fs::read_to_string(backup_path(&file)).unwrap();
        install_codex(&file, 9000).unwrap();
        let backup_after = std::fs::read_to_string(backup_path(&file)).unwrap();
        let content = std::fs::read_to_string(&file).unwrap();
        let v: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(
            v["mcp_servers"]["figma"]["url"].as_str(),
            Some("http://127.0.0.1:9000/mcp")
        );
        assert_eq!(backup_before, backup_after, "backup keeps the original");
        assert!(backup_after.contains("mcp.figma.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_uninstall_restores_original_config() {
        let (dir, file) = seed_codex("codex-uninstall", "model = \"gpt-5\"\n");
        install_codex(&file, 7337).unwrap();
        uninstall_codex(&file).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "model = \"gpt-5\"\n"
        );
        assert!(!backup_path(&file).exists(), "backup consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_uninstall_without_backup_resets_figma_url() {
        let (dir, file) = seed_codex(
            "codex-uninstall-no-backup",
            r#"
[mcp_servers.figma]
url = "http://127.0.0.1:7337/mcp"
"#,
        );
        uninstall_codex(&file).unwrap();
        let content = std::fs::read_to_string(&file).unwrap();
        let v: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(
            v["mcp_servers"]["figma"]["url"].as_str(),
            Some("https://mcp.figma.com/mcp")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_codex_config_honors_codex_home_even_when_config_missing() {
        let dir = std::env::temp_dir().join(format!(
            "frtk-init-{}-codex-home-missing",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CODEX_HOME", &dir);

        assert_eq!(discover_codex_config(), Some(dir.join("config.toml")));

        std::env::remove_var("CODEX_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_install_creates_missing_config_file() {
        let dir = std::env::temp_dir().join(format!(
            "frtk-init-{}-codex-create-missing",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");

        install_codex(&file, 7337).unwrap();

        let content = std::fs::read_to_string(&file).unwrap();
        let v: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(
            v["mcp_servers"]["figma"]["url"].as_str(),
            Some("http://127.0.0.1:7337/mcp")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
