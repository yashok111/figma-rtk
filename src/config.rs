//! Optional TOML config at `$XDG_CONFIG/figma-rtk/config.toml` (override with
//! `FRTK_CONFIG`). Absent config = sane defaults, so the proxy works with none.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Compression aggressiveness. `Standard` is the near-lossless whitespace pass
/// (phase 1). `Aggressive`/`Ultra` additionally apply trusted TOML filters and
/// depth limiting (phase 2 — plumbed here, teeth added with the filter engine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Standard,
    Aggressive,
    Ultra,
}

/// What raw payloads to retain for recovery (the `tee` idea from RTK).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TeeMode {
    #[default]
    Never,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TeeConfig {
    pub mode: TeeMode,
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CacheConfig {
    /// Collapse byte-identical re-reads of the same target to a sentinel.
    pub delta: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub level: Level,
    /// Tool names to leave uncompressed (pass straight through).
    pub exclude_tools: Vec<String>,
    pub tee: TeeConfig,
    pub cache: CacheConfig,
    /// MCP annotation pre-filter: drop content items whose `annotations.priority`
    /// is below this floor (Aggressive+ only). Default 0.0 = no-op (nothing
    /// dropped). Figma does not emit annotations yet, so this is future-ready.
    pub min_priority: f64,
}

impl Config {
    /// Return true when `tool` (a possibly-namespaced wire name such as
    /// `mcp__plugin_figma_figma__get_metadata`) matches any entry in
    /// `exclude_tools` (which stores bare names like `get_metadata`).
    /// Delegates to [`crate::filter::matches_tool`] so the `__` boundary rule
    /// is enforced and a short name cannot accidentally match a longer one.
    pub fn is_excluded(&self, tool: &str) -> bool {
        self.exclude_tools
            .iter()
            .any(|t| crate::filter::matches_tool(tool, t))
    }
}

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("FRTK_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("figma-rtk")
        .join("config.toml")
}

pub fn load() -> Config {
    load_from(&config_path())
}

pub fn create() -> anyhow::Result<PathBuf> {
    let p = config_path();
    create_at(&p)?;
    Ok(p)
}

fn load_from(path: &Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn create_at(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        anyhow::bail!(
            "config already exists at {} — edit it or delete it first",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&Config::default())?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("frtk-cfg-{}-{name}", std::process::id()))
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.level, Level::Standard);
        assert_eq!(c.tee.mode, TeeMode::Never);
        assert!(c.exclude_tools.is_empty());
        // R2-B: min_priority must default to 0.0 (annotation pre-filter no-op).
        assert_eq!(c.min_priority, 0.0_f64);
    }

    #[test]
    fn min_priority_parses_explicit_value() {
        let toml = "min_priority = 0.5\n";
        let c: Config = toml::from_str(toml).unwrap();
        assert!((c.min_priority - 0.5_f64).abs() < f64::EPSILON);
    }

    #[test]
    fn min_priority_absent_yields_zero() {
        // An old config file with no min_priority field must parse successfully
        // and yield 0.0 (true no-op).
        let toml = "level = \"aggressive\"\n";
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.min_priority, 0.0_f64);
    }

    #[test]
    fn missing_file_yields_default() {
        let c = load_from(Path::new("/no/such/figma-rtk/config.toml"));
        assert_eq!(c.level, Level::Standard);
    }

    #[test]
    fn create_then_load_roundtrips() {
        let p = tmp("roundtrip.toml");
        let _ = std::fs::remove_file(&p);
        create_at(&p).unwrap();
        let c = load_from(&p);
        assert_eq!(c.level, Level::Standard);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn create_refuses_to_clobber() {
        let p = tmp("noclobber.toml");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, "level = \"ultra\"\n").unwrap();
        assert!(create_at(&p).is_err(), "must not overwrite existing config");
        assert_eq!(load_from(&p).level, Level::Ultra, "user config intact");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn parses_explicit_values() {
        let toml = "level = \"aggressive\"\nexclude_tools = [\"whoami\"]\n[tee]\nmode = \"always\"\n[cache]\ndelta = true\n";
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.level, Level::Aggressive);
        assert!(c.is_excluded("whoami"));
        assert_eq!(c.tee.mode, TeeMode::Always);
        assert!(c.cache.delta);
    }

    #[test]
    fn is_excluded_matches_namespaced_wire_name() {
        let c = Config {
            exclude_tools: vec!["get_metadata".to_string()],
            ..Default::default()
        };
        // Bare name: exact match.
        assert!(c.is_excluded("get_metadata"), "bare name must match");
        // Namespaced wire name must also match via the __ boundary.
        assert!(
            c.is_excluded("mcp__plugin_figma_figma__get_metadata"),
            "namespaced wire name must match bare exclude entry"
        );
        // A name sharing the suffix but lacking __ boundary must NOT match.
        assert!(
            !c.is_excluded("evil_get_metadata"),
            "no __ boundary must NOT match"
        );
        // An unrelated tool must not be excluded.
        assert!(!c.is_excluded("get_design_context"), "unrelated tool must not be excluded");
    }

    #[test]
    fn cache_delta_defaults_off() {
        assert!(!Config::default().cache.delta);
    }
}
