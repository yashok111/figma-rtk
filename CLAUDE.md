# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`figma-rtk` (binary `frtk`) is a **transparent HTTP reverse proxy** that sits between
Claude Code and the **remote** Figma MCP server and compresses the heavy read-tool
responses before they reach the agent:

```
Claude Code  ──►  frtk (localhost:7337)  ──►  https://mcp.figma.com/mcp
```

The Figma MCP server is remote HTTP, **not** a stdio command — so this is a proxy,
not a CLI wrapper. `README.md` covers user-facing usage. Detailed running state and
phase history live in the maintainer's local Claude Code project memory (not part of
this repo).

## Build / test / run

**Rust is installed via rustup and is NOT on the default PATH, and the shell does
not persist between Bash calls.** Prefix every cargo command:

```bash
source "$HOME/.cargo/env"; cargo test                    # full suite
source "$HOME/.cargo/env"; cargo test --lib filter::      # one module
source "$HOME/.cargo/env"; cargo test <test_name>         # one test by name
source "$HOME/.cargo/env"; cargo clippy --all-targets     # must be clean
source "$HOME/.cargo/env"; cargo build --release          # -> ./target/release/frtk
```

`cargo` output is rewritten by the user's **RTK shell hook** into a compressed
one-liner (e.g. `cargo test: 59 passed`). To see raw failures, read the tee log
path it prints.

Hermetic test env overrides (used by tests so they don't touch real state):
`FRTK_LEDGER`, `FRTK_CONFIG`, `FRTK_TRUST`. Integration tests serialize
`FRTK_LEDGER` mutation via a module `ENV_LOCK` mutex.

## Architecture

`lib.rs` + `main.rs` (clap CLI) split. The request path:

- **`proxy.rs`** — axum catch-all reverse proxy. `build_state`/`app`/`AppState`.
  GET = stream passthrough; POST = buffer → transform → return. Loads config,
  filters, and the optional delta cache into state. Header filtering; relays the
  `Authorization: Bearer` header verbatim (never logs headers/bodies).
- **`mcp.rs`** — JSON-RPC awareness. `extract_targets` parses the *request* to map
  id → `Target{tool,key}` for the tools worth compressing (`TARGET_TOOLS`), then
  `transform_value`/`transform_sse`/`transform_msg` rewrite the matching *response*.
- **`compress.rs`** — `compress_result_with(result, tool, level, filters)` is the
  core. Standard level = near-lossless whitespace only. Aggressive/ultra also run
  filters (see below). Savings measured via `cache::content_text_len` before/after.
- **`filter.rs`** — declarative TOML filters (`drop_keys` recursive, `drop_where`
  shallow array-element match, `max_depth` truncate) with **inline tests**.
- **`trust.rs`** — trust store gating project-local filters.
- **`cache.rs`** — opt-in `DeltaCache` collapsing byte-identical re-reads.
- Support: `config.rs`, `tee.rs`, `capture.rs`, `fsutil.rs`, `stats.rs`, `tokens.rs`.
- `tests/proxy_it.rs` — end-to-end against a mock upstream via `tower::oneshot`.

### Two things that are easy to get wrong

1. **Filters have two application modes, applied together at aggressive+.**
   `FilterSet::apply_structural(tool, &mut result)` runs the filter rules over the
   *response envelope structure* — this is what drops whole `content[]` blocks (e.g.
   `get_design_context`'s boilerplate text blocks, whose text is React **code**, not
   JSON) and keys like `_meta`. Separately, `apply_text(tool, text)` parses a single
   `content[].text` field *as JSON* and strips fields inside it — that's the path
   for `get_metadata`, whose payload is a JSON string. `compress_result_with` runs
   `apply_structural` first, then `apply_text` per surviving block. A
   `get_design_context` filter cannot reach *inside* the opaque ~9KB code string —
   only the JSON structure around it.

2. **OAuth discovery must be rewritten, or auth fails.** Claude Code's SDK does a
   strict RFC 9728 origin check: it dials the proxy but Figma advertises
   `resource=https://mcp.figma.com/mcp`, which fails the check. `proxy.rs` rewrites
   the 401 `WWW-Authenticate` `resource_metadata` and the
   `GET /.well-known/oauth-protected-resource` `resource` to the **proxy** origin,
   while keeping `authorization_servers`/`authorization_uri` pointed at
   `api.figma.com` (so OAuth still runs CC↔Figma directly; the proxy never sees the
   token exchange). **CC caches discovery from first connect** — after changing auth
   behavior, the user must `/mcp` → reconnect the figma plugin server (a plain
   restart does not reliably clear it).

## Working conventions

- **Strict TDD.** Write/adjust the test, see it fail, implement, green. Every
  feature has unit tests; the proxy has the mock-upstream integration test.
- **Git repo since 2026-06-03** (branch `main`, no remote/`origin` yet). Do **not**
  commit or push without an explicit user command. Per the user's flow, do feature
  work on `feat/<name>` branches, never commit feature work straight to `main`.
  `target/` and the transient `fixtures/get_design_context-[0-9]*.json` capture dumps
  are gitignored; the `*-sample.json` fixtures are the committed TDD ground truth.
- **Tool-name matching uses a `__` boundary**, never loose `ends_with` (both
  `mcp::is_target` and `filter::matches_tool`) — so `metadata` does not match
  `set_metadata`. The write-side tools `send_code_connect_mappings` and
  `add_code_connect_map` are deliberately absent from `TARGET_TOOLS` and are
  never compressed by frtk.
- Aggressive/ultra filters can **drop data** → keep `tee` recoverable; `serve` warns
  if level is aggressive+ and tee is off. Filters never apply at Standard, and
  project-local filters never apply unless the directory is `frtk trust`ed.
- **Two figma MCP servers may be connected:** `mcp__claude_ai_Figma__*` is the
  claude.ai account connector (**unproxied** — bypasses frtk), while
  `mcp__plugin_figma_figma__*` is the plugin server that `frtk init` repoints at the
  proxy. Any capture / proxied read MUST use the **plugin** server.
- The user works in **Waves**: plan → implement (inline TDD, or parallel
  `cavecrew-builder` for independent tasks) → **two-factor review** (two parallel
  `cavecrew-reviewer` subagents, different vectors e.g. correctness + data-integrity;
  a blocker from either loops the wave) → Telegram report per wave. No commits/pushes
  without an explicit command.
