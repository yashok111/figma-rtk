//! Small filesystem helpers shared by `capture` and `tee`: filename
//! sanitization and owner-only file creation (raw payloads can be the user's
//! design data — keep them out of other local users' reach).

use std::path::Path;

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
}
