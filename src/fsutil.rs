//! Small filesystem helpers shared by `capture` and `tee`: filename
//! sanitization and owner-only file creation (raw payloads can be the user's
//! design data — keep them out of other local users' reach).

use std::path::{Path, PathBuf};

/// Write `content` to `file` atomically: write a `.frtk-tmp` sibling, then
/// rename it into place. Rename is atomic on POSIX (and near-atomic on Windows)
/// and replaces the destination name itself, so a crash never leaves a
/// half-written file. The tmp is a sibling so it shares the same filesystem as
/// the destination, making rename cheap and truly atomic (no cross-device copy).
pub(crate) fn write_atomic(file: &Path, content: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write;

    let mut tmp_os = file.as_os_str().to_owned();
    tmp_os.push(".frtk-tmp");
    let tmp = PathBuf::from(tmp_os);
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating temp {}", tmp.display()))?;
        f.write_all(content)?;
    }
    std::fs::rename(&tmp, file).with_context(|| format!("finalizing {}", file.display()))?;
    Ok(())
}

/// Sanitize a tool name into a safe filename component. Never empty and cannot
/// contain path separators, so it can't escape the target directory.
pub fn safe_name(tool: &str) -> String {
    let s: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "unknown".to_string()
    } else {
        s
    }
}

/// Create (truncating) a file readable/writable only by the owner (0600 on Unix).
pub fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Like [`create_private`] but exclusive: fails with `AlreadyExists` if the file
/// exists (O_EXCL). Lets a caller claim the next free filename without ever
/// clobbering an existing one, even under concurrent writers.
pub fn create_private_new(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_never_empty_and_no_separators() {
        assert_eq!(safe_name(""), "unknown");
        assert_eq!(safe_name("../evil/name"), "___evil_name");
        assert_eq!(safe_name("get_metadata"), "get_metadata");
    }

    #[cfg(unix)]
    #[test]
    fn create_private_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir().join(format!("frtk-priv-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        {
            let _f = create_private(&p).unwrap();
        }
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn create_private_new_refuses_existing() {
        let p = std::env::temp_dir().join(format!("frtk-excl-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        {
            let _f = create_private_new(&p).unwrap(); // first create succeeds
        }
        let err = create_private_new(&p).unwrap_err(); // second must fail, not clobber
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_file(&p);
    }

    #[cfg(unix)]
    #[test]
    fn create_private_new_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir().join(format!("frtk-exclmode-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        {
            let _f = create_private_new(&p).unwrap();
        }
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(&p);
    }
}
