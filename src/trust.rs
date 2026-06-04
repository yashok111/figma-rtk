//! Trust store for project-local filters. A project's `.figma-rtk/filters.toml`
//! is only applied if its directory has been explicitly trusted (`frtk trust`),
//! so dropping into a hostile repo can't silently activate a filter that strips
//! data you needed. Global filters (in the user's own config dir) are implicitly
//! trusted. Stored as a JSON set of canonical dir paths at
//! `$XDG_DATA/figma-rtk/trusted.json` (override with `FRTK_TRUST`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::fsutil;

fn store_path() -> PathBuf {
    if let Ok(p) = std::env::var("FRTK_TRUST") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("figma-rtk")
        .join("trusted.json")
}

fn canon(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn load(store: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(store)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(store: &Path, set: &BTreeSet<String>) -> anyhow::Result<()> {
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent)?;
    }
    fsutil::write_atomic(store, serde_json::to_string_pretty(set)?.as_bytes())
}

fn contains_in(store: &Path, dir: &Path) -> bool {
    load(store).contains(&canon(dir))
}

fn add_in(store: &Path, dir: &Path) -> anyhow::Result<()> {
    let mut set = load(store);
    set.insert(canon(dir));
    save(store, &set)
}

fn remove_in(store: &Path, dir: &Path) -> anyhow::Result<()> {
    let mut set = load(store);
    set.remove(&canon(dir));
    save(store, &set)
}

pub fn is_trusted(dir: &Path) -> bool {
    contains_in(&store_path(), dir)
}

pub fn add(dir: &Path) -> anyhow::Result<()> {
    add_in(&store_path(), dir)
}

pub fn remove(dir: &Path) -> anyhow::Result<()> {
    remove_in(&store_path(), dir)
}

pub fn list() -> Vec<String> {
    load(&store_path()).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("frtk-trust-{}-{name}.json", std::process::id()))
    }

    #[test]
    fn add_contains_remove_roundtrip() {
        let store = tmp_store("rt");
        let _ = std::fs::remove_file(&store);
        let dir = std::env::temp_dir();
        assert!(!contains_in(&store, &dir));
        add_in(&store, &dir).unwrap();
        assert!(contains_in(&store, &dir));
        remove_in(&store, &dir).unwrap();
        assert!(!contains_in(&store, &dir));
        let _ = std::fs::remove_file(&store);
    }

    #[test]
    fn untrusted_dir_is_not_trusted() {
        let store = tmp_store("untrusted");
        let _ = std::fs::remove_file(&store);
        add_in(&store, &std::env::temp_dir()).unwrap();
        assert!(!contains_in(&store, Path::new("/definitely/not/added")));
        let _ = std::fs::remove_file(&store);
    }

    #[test]
    fn corrupt_store_returns_empty_no_panic() {
        // load() must return an empty set (and not panic) when the store contains
        // invalid JSON (simulates a crash-corrupted file).
        let store = tmp_store("corrupt");
        std::fs::write(&store, b"not valid json {{{ ]]]").unwrap();
        let set = load(&store);
        assert!(set.is_empty(), "corrupt store must yield empty set");
        let _ = std::fs::remove_file(&store);
    }

    #[test]
    fn save_leaves_no_tmp_behind() {
        // A successful save must not leave a .frtk-tmp sibling.
        let store = tmp_store("notmp");
        let _ = std::fs::remove_file(&store);
        add_in(&store, &std::env::temp_dir()).unwrap();
        // The store itself exists.
        assert!(store.exists(), "store must exist after save");
        // No .frtk-tmp sibling must remain.
        let mut tmp_path = store.as_os_str().to_owned();
        tmp_path.push(".frtk-tmp");
        let tmp_path = std::path::PathBuf::from(tmp_path);
        assert!(!tmp_path.exists(), ".frtk-tmp must not be left behind");
        let _ = std::fs::remove_file(&store);
    }
}
