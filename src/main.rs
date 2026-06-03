//! frtk — Figma MCP token-killer reverse proxy ("RTK for Figma").
//!
//! Sits between Claude Code and the remote Figma MCP server
//! (https://mcp.figma.com/mcp). It relays every JSON-RPC call verbatim —
//! including the OAuth Bearer token, which keeps Figma as the token audience —
//! and transparently compresses the responses of the heavy read tools
//! (get_design_context / get_metadata) before they reach the agent.

use clap::{Parser, Subcommand};
use figma_rtk::{compress, config, filter, init, proxy, stats, tokens, trust};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "frtk", version, about = "Token-killer reverse proxy for the Figma MCP server")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the reverse proxy in front of the Figma MCP server.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 7337)]
        port: u16,
        #[arg(long, default_value = "https://mcp.figma.com")]
        upstream: String,
        /// Dump raw target tool responses (body only) here as fixtures.
        #[arg(long)]
        capture_dir: Option<std::path::PathBuf>,
        /// Override the config's compression level for this run.
        #[arg(long, value_enum)]
        level: Option<config::Level>,
    },
    /// Show token savings collected so far (RTK-style `gain`).
    Gain {
        /// List the most recent compressed calls instead of just totals.
        #[arg(long)]
        history: bool,
    },
    /// Print the Claude Code MCP config snippet, or create a config file.
    Config {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 7337)]
        port: u16,
        /// Create a default config.toml instead of printing the snippet.
        #[arg(long)]
        create: bool,
    },
    /// Point Claude Code's Figma MCP server at the proxy (edits the plugin's
    /// .mcp.json, with a restorable backup). Run this yourself; restart CC after.
    Init {
        /// Path to the MCP config to edit (defaults to the figma plugin's).
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, default_value_t = 7337)]
        port: u16,
        /// Restore the original config.
        #[arg(long)]
        uninstall: bool,
    },
    /// Compress a tool-response payload read from stdin (TDD/fixture harness).
    /// Compressed output goes to stdout; savings to stderr.
    Compress {
        /// Tool name — labels the savings and selects matching filters.
        #[arg(long)]
        tool: Option<String>,
        #[arg(long, value_enum, default_value_t = config::Level::Standard)]
        level: config::Level,
        /// Filter file to apply at aggressive/ultra (default ./.figma-rtk/filters.toml).
        #[arg(long)]
        filters: Option<PathBuf>,
    },
    /// Run a filter file's inline tests. Exits nonzero on any failure.
    Verify {
        /// Filter file (defaults to ./.figma-rtk/filters.toml).
        #[arg(long)]
        filters: Option<PathBuf>,
        /// Only run tests for this filter name.
        #[arg(long)]
        filter: Option<String>,
        /// Fail if any filter has no inline tests (CI mode).
        #[arg(long)]
        require_all: bool,
    },
    /// Trust the current directory's project-local filters (.figma-rtk/).
    Trust {
        /// Directory to trust (defaults to the current directory).
        #[arg(long)]
        path: Option<PathBuf>,
        /// List trusted directories instead of adding one.
        #[arg(long)]
        list: bool,
    },
    /// Revoke trust for a project directory's filters.
    Untrust {
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { host, port, upstream, capture_dir, level } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();
            proxy::serve(&host, port, &upstream, capture_dir, level).await
        }
        Cmd::Gain { history } => stats::print_gain(history),
        Cmd::Config { host, port, create } => {
            if create {
                let path = config::create()?;
                println!("wrote default config to {}", path.display());
            } else {
                print_config(&host, port);
            }
            Ok(())
        }
        Cmd::Init { file, port, uninstall } => run_init(file, port, uninstall),
        Cmd::Compress { tool, level, filters } => run_compress(tool, level, filters),
        Cmd::Verify { filters, filter, require_all } => run_verify(filters, filter, require_all),
        Cmd::Trust { path, list } => {
            if list {
                for d in trust::list() {
                    println!("{d}");
                }
            } else {
                let dir = match path {
                    Some(p) => p,
                    None => std::env::current_dir()?,
                };
                trust::add(&dir)?;
                println!("trusted {}", dir.display());
            }
            Ok(())
        }
        Cmd::Untrust { path } => {
            let dir = match path {
                Some(p) => p,
                None => std::env::current_dir()?,
            };
            trust::remove(&dir)?;
            println!("untrusted {}", dir.display());
            Ok(())
        }
    }
}

fn run_verify(
    filters: Option<PathBuf>,
    only: Option<String>,
    require_all: bool,
) -> anyhow::Result<()> {
    let path = filters.unwrap_or_else(|| PathBuf::from(".figma-rtk/filters.toml"));
    let set = filter::FilterSet::load(&path)?;
    if set.is_empty() {
        println!("no filters in {}", path.display());
        return Ok(());
    }
    let mut filters = set.filters;
    if let Some(name) = &only {
        filters.retain(|f| &f.name == name);
        if filters.is_empty() {
            anyhow::bail!("no filter named {name:?} in {}", path.display());
        }
    }
    let missing: Vec<String> = filters
        .iter()
        .filter(|f| f.tests.is_empty())
        .map(|f| f.name.clone())
        .collect();
    let results = filter::run_tests(&filters);
    let failed = results.iter().filter(|r| !r.passed).count();
    for r in &results {
        if r.passed {
            println!("  [ok]   {} :: {}", r.filter, r.test);
        } else {
            println!("  [FAIL] {} :: {} — {}", r.filter, r.test, r.detail);
        }
    }
    println!(
        "{} test(s): {} passed, {} failed",
        results.len(),
        results.len() - failed,
        failed
    );
    if require_all && !missing.is_empty() {
        anyhow::bail!("filters with no inline tests: {}", missing.join(", "));
    }
    if failed > 0 {
        anyhow::bail!("{failed} filter test(s) failed");
    }
    Ok(())
}

fn run_init(file: Option<PathBuf>, port: u16, uninstall: bool) -> anyhow::Result<()> {
    let file = match file.or_else(init::discover_mcp_file) {
        Some(f) => f,
        None => anyhow::bail!(
            "could not find the figma plugin's .mcp.json; pass --file <path> explicitly"
        ),
    };
    if uninstall {
        init::uninstall(&file)?;
        println!("restored {}", file.display());
    } else {
        init::install(&file, port)?;
        println!(
            "pointed figma MCP server at http://127.0.0.1:{port}/mcp\n  file:   {}\n  backup: {}.frtk-backup\nRun `frtk serve` and restart Claude Code. Revert with `frtk init --uninstall`.",
            file.display(),
            file.display()
        );
    }
    Ok(())
}

fn run_compress(
    tool: Option<String>,
    level: config::Level,
    filters_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let filters = if matches!(level, config::Level::Standard) {
        filter::FilterSet::default()
    } else {
        let p = filters_path.unwrap_or_else(|| PathBuf::from(".figma-rtk/filters.toml"));
        filter::FilterSet::load(&p)?
    };
    let toolname = tool.clone().unwrap_or_default();
    let (out, sv) = compress::compress_payload(&input, &toolname, level, &filters);
    let saved = tokens::tok_saved(sv.before, sv.after);
    let pct = tokens::pct_saved(sv.before, sv.after);
    eprintln!(
        "frtk compress[{}] level={level:?}  {} -> {} bytes  (~{saved} tokens, {pct:.1}%)",
        tool.as_deref().unwrap_or("stdin"),
        sv.before,
        sv.after,
    );
    print!("{out}");
    Ok(())
}

fn print_config(host: &str, port: u16) {
    println!(
        r#"Point Claude Code's Figma MCP server at the proxy.

Project .mcp.json (or the figma plugin's server entry):

{{
  "mcpServers": {{
    "figma": {{
      "type": "http",
      "url": "http://{host}:{port}/mcp"
    }}
  }}
}}

Then run the proxy in another terminal:

  frtk serve --host {host} --port {port}

OAuth still happens directly between Claude Code and api.figma.com; the proxy
only relays the Bearer token, so the token audience stays mcp.figma.com."#
    );
}
