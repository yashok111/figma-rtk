//! `frtk status` — three-check health probe.
//!
//! Pure helper functions are separated from the I/O side-effects so they can be
//! unit-tested without a live socket or file system.

use serde_json::Value;
use std::path::PathBuf;

use crate::init::Target;

// ─── pure helpers (unit-testable) ────────────────────────────────────────────

/// Return `true` iff the `.mcp.json` `mcpServers.figma.url` equals
/// `expected_url` exactly.
pub fn mcp_url_matches(mcp_json: &Value, expected_url: &str) -> bool {
    mcp_json
        .get("mcpServers")
        .and_then(|m| m.get("figma"))
        .and_then(|f| f.get("url"))
        .and_then(|u| u.as_str())
        .map(|u| u == expected_url)
        .unwrap_or(false)
}

/// Return `true` iff Codex's `config.toml` has `mcp_servers.figma.url` equal to
/// `expected_url` exactly.
pub fn codex_mcp_url_matches(config: &toml::Value, expected_url: &str) -> bool {
    config
        .get("mcp_servers")
        .and_then(|m| m.get("figma"))
        .and_then(|f| f.get("url"))
        .and_then(|u| u.as_str())
        .map(|u| u == expected_url)
        .unwrap_or(false)
}

/// Return `true` iff the `resource` field of an OAuth Protected Resource
/// Metadata JSON document has its **origin** (scheme + host + port) equal to
/// `expected_origin` (e.g. `"http://127.0.0.1:7337"`).
///
/// Concretely the proxy rewrites `resource` to `{proxy_origin}/mcp`, so the
/// full value must be `http://127.0.0.1:{port}/mcp`.  We accept any path on
/// the same origin so we remain robust to future path changes.
pub fn prm_origin_matches(prm_json: &Value, expected_origin: &str) -> bool {
    let Some(resource) = prm_json.get("resource").and_then(|r| r.as_str()) else {
        return false;
    };
    // Parse with reqwest::Url (already a dependency) to extract the origin.
    let Ok(url) = reqwest::Url::parse(resource) else {
        return false;
    };
    let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""),);
    let origin = if let Some(port) = url.port() {
        format!("{origin}:{port}")
    } else {
        origin
    };
    origin == expected_origin
}

// ─── I/O side-effects (manual-test only) ─────────────────────────────────────

/// Result of a single status check.
#[derive(Debug)]
pub struct CheckResult {
    pub ok: bool,
    pub label: &'static str,
    pub detail: String,
}

/// Human-facing note about which tool namespace actually flows through frtk.
pub fn target_usage_note(target: Target) -> &'static str {
    match target {
        Target::Codex => {
            "Use the Codex MCP-server tools (for example `mcp__figma__get_design_context`) \
for proxied reads. Codex Apps Figma tools (`mcp__codex_apps__figma__*`) bypass frtk."
        }
        Target::Claude => {
            "Use the plugin Figma MCP tools (for example \
`mcp__plugin_figma_figma__get_design_context`) for proxied reads; account connector \
tools bypass frtk."
        }
    }
}

pub fn target_usage_json(target: Target) -> Value {
    match target {
        Target::Codex => serde_json::json!({
            "proxied_tool_namespace": "mcp__figma__*",
            "bypasses_proxy_namespace": "mcp__codex_apps__figma__*",
            "note": target_usage_note(target),
        }),
        Target::Claude => serde_json::json!({
            "proxied_tool_namespace": "mcp__plugin_figma_figma__*",
            "bypasses_proxy_namespace": "account connector tools",
            "note": target_usage_note(target),
        }),
    }
}

/// Run all three status checks and return them in order.
///
/// This function performs live I/O (TCP connect, HTTP GET, file parse) and must
/// NOT be called from unit tests.
pub async fn run_checks(target: Target, file: Option<PathBuf>, port: u16) -> Vec<CheckResult> {
    let mut results = Vec::with_capacity(3);

    // Check 1 — proxy reachable
    let proxy_up = check_proxy_up(port).await;
    results.push(proxy_up);

    // Check 2 — agent MCP config wired to the proxy
    results.push(check_mcp_wired(target, file, port).await);

    // Check 3 — OAuth discovery fresh (only when proxy is up)
    if results[0].ok {
        results.push(check_oauth_fresh(port).await);
    } else {
        results.push(CheckResult {
            ok: false,
            label: "oauth_fresh",
            detail: "skipped (proxy not reachable)".into(),
        });
    }

    results
}

async fn check_proxy_up(port: u16) -> CheckResult {
    use std::time::Duration;
    use tokio::time::timeout;

    let addr = format!("127.0.0.1:{port}");
    match timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(addr.as_str()),
    )
    .await
    {
        Ok(Ok(_)) => CheckResult {
            ok: true,
            label: "proxy_up",
            detail: format!("connected to {addr}"),
        },
        Ok(Err(e)) => CheckResult {
            ok: false,
            label: "proxy_up",
            detail: format!("connection refused: {e}"),
        },
        Err(_) => CheckResult {
            ok: false,
            label: "proxy_up",
            detail: format!("timeout connecting to {addr}"),
        },
    }
}

async fn check_mcp_wired(target: Target, file: Option<PathBuf>, port: u16) -> CheckResult {
    match target {
        Target::Codex => check_codex_mcp_wired(file, port).await,
        Target::Claude => check_claude_mcp_wired(file, port).await,
    }
}

async fn check_claude_mcp_wired(file: Option<PathBuf>, port: u16) -> CheckResult {
    let expected = format!("http://127.0.0.1:{port}/mcp");
    let Some(path) = file.or_else(crate::init::discover_mcp_file) else {
        return CheckResult {
            ok: false,
            label: "mcp_wired",
            detail: "could not find Claude Code's figma plugin .mcp.json".into(),
        };
    };
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                ok: false,
                label: "mcp_wired",
                detail: format!("could not read {}: {e}", path.display()),
            };
        }
    };
    let v: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            return CheckResult {
                ok: false,
                label: "mcp_wired",
                detail: format!("JSON parse error in {}: {e}", path.display()),
            };
        }
    };
    if mcp_url_matches(&v, &expected) {
        CheckResult {
            ok: true,
            label: "mcp_wired",
            detail: format!("{} -> {expected}", path.display()),
        }
    } else {
        let actual = v
            .get("mcpServers")
            .and_then(|m| m.get("figma"))
            .and_then(|f| f.get("url"))
            .and_then(|u| u.as_str())
            .unwrap_or("<missing>")
            .to_string();
        CheckResult {
            ok: false,
            label: "mcp_wired",
            detail: format!("url is {actual:?}, expected {expected:?}"),
        }
    }
}

async fn check_codex_mcp_wired(file: Option<PathBuf>, port: u16) -> CheckResult {
    let expected = format!("http://127.0.0.1:{port}/mcp");
    let Some(path) = file.or_else(crate::init::discover_codex_config) else {
        return CheckResult {
            ok: false,
            label: "mcp_wired",
            detail: "could not find Codex config.toml".into(),
        };
    };
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                ok: false,
                label: "mcp_wired",
                detail: format!("could not read {}: {e}", path.display()),
            };
        }
    };
    let v: toml::Value = match toml::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            return CheckResult {
                ok: false,
                label: "mcp_wired",
                detail: format!("TOML parse error in {}: {e}", path.display()),
            };
        }
    };
    if codex_mcp_url_matches(&v, &expected) {
        CheckResult {
            ok: true,
            label: "mcp_wired",
            detail: format!("{} -> {expected}", path.display()),
        }
    } else {
        let actual = v
            .get("mcp_servers")
            .and_then(|m| m.get("figma"))
            .and_then(|f| f.get("url"))
            .and_then(|u| u.as_str())
            .unwrap_or("<missing>")
            .to_string();
        CheckResult {
            ok: false,
            label: "mcp_wired",
            detail: format!("url is {actual:?}, expected {expected:?}"),
        }
    }
}

async fn check_oauth_fresh(port: u16) -> CheckResult {
    let url = format!("http://127.0.0.1:{port}/.well-known/oauth-protected-resource");
    let expected_origin = format!("http://127.0.0.1:{port}");

    // Build a one-shot client; no auth, no redirect follow needed.
    let client = match reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(false)
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                ok: false,
                label: "oauth_fresh",
                detail: format!("could not build HTTP client: {e}"),
            };
        }
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return CheckResult {
                ok: false,
                label: "oauth_fresh",
                detail: format!("GET {url} failed: {e}"),
            };
        }
    };

    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return CheckResult {
                ok: false,
                label: "oauth_fresh",
                detail: format!("reading response body failed: {e}"),
            };
        }
    };

    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return CheckResult {
                ok: false,
                label: "oauth_fresh",
                detail: format!("JSON parse of PRM doc failed: {e}"),
            };
        }
    };

    if prm_origin_matches(&v, &expected_origin) {
        CheckResult {
            ok: true,
            label: "oauth_fresh",
            detail: format!("resource origin == {expected_origin}"),
        }
    } else {
        let actual = v
            .get("resource")
            .and_then(|r| r.as_str())
            .unwrap_or("<missing>")
            .to_string();
        CheckResult {
            ok: false,
            label: "oauth_fresh",
            detail: format!(
                "resource={actual:?} origin != {expected_origin}; \
                 reconnect the Figma MCP server in your agent \
                 to clear the cached discovery"
            ),
        }
    }
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── mcp_url_matches ──────────────────────────────────────────────────────

    #[test]
    fn mcp_url_matches_correct_url() {
        let v = json!({"mcpServers":{"figma":{"type":"http","url":"http://127.0.0.1:7337/mcp"}}});
        assert!(mcp_url_matches(&v, "http://127.0.0.1:7337/mcp"));
    }

    #[test]
    fn mcp_url_matches_rejects_upstream_url() {
        let v = json!({"mcpServers":{"figma":{"type":"http","url":"https://mcp.figma.com/mcp"}}});
        assert!(!mcp_url_matches(&v, "http://127.0.0.1:7337/mcp"));
    }

    #[test]
    fn mcp_url_matches_different_port() {
        let v = json!({"mcpServers":{"figma":{"type":"http","url":"http://127.0.0.1:9000/mcp"}}});
        assert!(!mcp_url_matches(&v, "http://127.0.0.1:7337/mcp"));
        assert!(mcp_url_matches(&v, "http://127.0.0.1:9000/mcp"));
    }

    #[test]
    fn mcp_url_matches_missing_servers_key() {
        let v = json!({});
        assert!(!mcp_url_matches(&v, "http://127.0.0.1:7337/mcp"));
    }

    #[test]
    fn mcp_url_matches_missing_figma_key() {
        let v = json!({"mcpServers":{}});
        assert!(!mcp_url_matches(&v, "http://127.0.0.1:7337/mcp"));
    }

    #[test]
    fn codex_mcp_url_matches_correct_url() {
        let v: toml::Value = toml::from_str(
            r#"
[mcp_servers.figma]
url = "http://127.0.0.1:7337/mcp"
"#,
        )
        .unwrap();
        assert!(codex_mcp_url_matches(&v, "http://127.0.0.1:7337/mcp"));
    }

    #[test]
    fn codex_mcp_url_matches_rejects_upstream_url() {
        let v: toml::Value = toml::from_str(
            r#"
[mcp_servers.figma]
url = "https://mcp.figma.com/mcp"
"#,
        )
        .unwrap();
        assert!(!codex_mcp_url_matches(&v, "http://127.0.0.1:7337/mcp"));
    }

    #[test]
    fn codex_mcp_url_matches_missing_figma_key() {
        let v: toml::Value = toml::from_str(
            r#"
[mcp_servers.node_repl]
command = "node"
"#,
        )
        .unwrap();
        assert!(!codex_mcp_url_matches(&v, "http://127.0.0.1:7337/mcp"));
    }

    #[test]
    fn target_usage_note_warns_codex_apps_bypass_proxy() {
        let note = target_usage_note(Target::Codex);
        assert!(note.contains("mcp__figma__"));
        assert!(note.contains("mcp__codex_apps__figma__"));
        assert!(note.contains("bypass frtk"));
    }

    #[test]
    fn target_usage_json_includes_machine_readable_codex_namespaces() {
        let value = target_usage_json(Target::Codex);
        assert_eq!(value["proxied_tool_namespace"], "mcp__figma__*");
        assert_eq!(
            value["bypasses_proxy_namespace"],
            "mcp__codex_apps__figma__*"
        );
    }

    #[tokio::test]
    async fn check_mcp_wired_uses_explicit_codex_config_file() {
        let dir = std::env::temp_dir().join(format!(
            "frtk-status-{}-explicit-codex-file",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("custom-config.toml");
        std::fs::write(
            &file,
            r#"
[mcp_servers.figma]
url = "http://127.0.0.1:8123/mcp"
"#,
        )
        .unwrap();

        let check = check_mcp_wired(Target::Codex, Some(file.clone()), 8123).await;

        assert!(check.ok, "{check:?}");
        assert!(check.detail.contains(&file.display().to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── prm_origin_matches ───────────────────────────────────────────────────

    #[test]
    fn prm_origin_matches_rewritten_resource() {
        // What the proxy actually returns after rewriting.
        let v = json!({"resource":"http://127.0.0.1:7337/mcp","authorization_servers":["https://api.figma.com"]});
        assert!(prm_origin_matches(&v, "http://127.0.0.1:7337"));
    }

    #[test]
    fn prm_origin_matches_rejects_upstream_origin() {
        // Stale cache — still points at Figma's origin.
        let v = json!({"resource":"https://mcp.figma.com/mcp","authorization_servers":["https://api.figma.com"]});
        assert!(!prm_origin_matches(&v, "http://127.0.0.1:7337"));
    }

    #[test]
    fn prm_origin_matches_different_port() {
        let v = json!({"resource":"http://127.0.0.1:9000/mcp"});
        assert!(!prm_origin_matches(&v, "http://127.0.0.1:7337"));
        assert!(prm_origin_matches(&v, "http://127.0.0.1:9000"));
    }

    #[test]
    fn prm_origin_matches_missing_resource_field() {
        let v = json!({"authorization_servers":["https://api.figma.com"]});
        assert!(!prm_origin_matches(&v, "http://127.0.0.1:7337"));
    }

    #[test]
    fn prm_origin_matches_invalid_url_in_resource() {
        let v = json!({"resource":"not a url"});
        assert!(!prm_origin_matches(&v, "http://127.0.0.1:7337"));
    }
}
