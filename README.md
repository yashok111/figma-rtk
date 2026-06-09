# figma-rtk (`frtk`)

Token-killer **reverse proxy** for the Figma MCP server — "RTK for Figma".

The Figma MCP server is remote HTTP (`https://mcp.figma.com/mcp`), not a stdio
command. So `frtk` is not a CLI wrapper like RTK — it's a local MCP reverse
proxy:

```
Codex / Claude Code  ──►  frtk (localhost:7337)  ──►  https://mcp.figma.com/mcp
```

It relays every JSON-RPC call verbatim and, for the configured target tools,
compresses the response before it reaches the agent.

## What v1 does

- **Transparent relay + auth passthrough.** All paths/methods forwarded as-is.
  OAuth happens directly between the agent and `api.figma.com`; the proxy only
  relays the `Authorization: Bearer` token, so the token audience stays
  `mcp.figma.com`. The proxy never logs headers or bodies.
- **Read compression** of Figma target-tool results (`get_design_context`,
  `get_metadata`, and the other names in `src/mcp.rs`'s `TARGET_TOOLS`).
  At the default `standard` level this is conservative and near-lossless: JSON
  minified, XML indentation stripped, code blank-line runs collapsed. Handles
  both `application/json` and `text/event-stream` (SSE) responses.
- **Write-tool passthrough.** Write-side tools are deliberately not compressed.
  Aggressive/ultra filters may drop data, but only at those explicit levels and
  only for matching target tools.
- **`gain` meter** — RTK-style token-savings accounting.

## Usage

```bash
cargo build --release
./target/release/frtk init             # point Codex's figma server at the proxy (writes a backup)
./target/release/frtk serve            # start the proxy
./target/release/frtk gain             # show token savings (--history for recent calls)
./target/release/frtk init --uninstall # restore the original Codex config
```

By default, `frtk init` edits Codex's config (`$CODEX_HOME/config.toml`, or
`~/.codex/config.toml` when `CODEX_HOME` is unset) and writes:

```toml
[mcp_servers.figma]
url = "http://127.0.0.1:7337/mcp"
```

It creates the config file if needed and backs the original file up to
`config.toml.frtk-backup`. Restart Codex, or reconnect the Figma MCP server if
the UI exposes MCP reconnect. If you initialize a non-default path with
`frtk init --file <path>`, check the same file with `frtk status --file <path>`.

In Codex, use the MCP server tools exposed from this config (typically names like
`mcp__figma__get_design_context`) when you want reads to flow through `frtk`.
The app-backed Figma connector from Codex Apps exposes tools like
`mcp__codex_apps__figma__get_design_context`; those bypass `frtk` entirely and
will not be compressed or captured by this proxy.

In new Codex sessions, the intended setup is to disable the Figma Apps connector
and leave only the MCP server path:

```text
mcp__figma__* -> http://127.0.0.1:7337/mcp -> frtk -> Figma
```

`frtk serve` must be running for this path to work. The quick readiness check is:

```bash
frtk status --json
```

The healthy state is `proxy_up: true`, `mcp_wired: true`, and
`oauth_fresh: true`; agents should then use the `mcp__figma__*` namespace.

Claude Code remains supported with `frtk init --target claude`; that edits the
Figma plugin `.mcp.json` `url` to `http://127.0.0.1:7337/mcp` and backs the
original up to `.frtk-backup`. In Claude Code, go to `/mcp` →
`plugin:figma:figma` → **Reconnect** after changing the URL.

`frtk config` prints the equivalent snippet if you'd rather wire it by hand.

### Config (optional)

`$XDG_CONFIG/figma-rtk/config.toml` (override via `FRTK_CONFIG`); `frtk config --create`
writes a default. Absent config = sane defaults.

```toml
level = "standard"          # standard | aggressive | ultra (aggressive/ultra: phase 2)
exclude_tools = []          # tool names to pass through uncompressed

[tee]
mode = "never"              # never | always — keep last raw body per tool (owner-only 0600)
# dir = "..."               # defaults to $XDG_DATA/figma-rtk/tee

[cache]
delta = false               # collapse byte-identical re-reads of the same target to a sentinel
```

### Delta cache (`[cache] delta = true`, opt-in)

When on, a repeated read of the same target (tool + arguments) whose result is
byte-identical to the previous one is replaced with a compact sentinel instead
of resending the whole tree — the write→re-read and poll loops then cost a few
tokens. Off by default; keep `tee` on so the raw payload stays recoverable.

### Aggressive filters (`level = "aggressive"` / `"ultra"`)

At `aggressive`/`ultra`, the proxy additionally applies **TOML filters** that
strip structure from a tool's JSON result. Filters are user-authored, carry
inline tests, and — when project-local — only run if the directory is trusted.

Global filters live at `$XDG_CONFIG/figma-rtk/filters.toml` (implicitly trusted).
Project-local filters at `./.figma-rtk/filters.toml` run only after `frtk trust`.

```toml
[[filter]]
name = "design-context-lean"
tools = ["get_design_context"]            # exact wire name, or MCP-namespaced (…__get_design_context)
drop_keys = ["vectorPaths", "absoluteRenderBounds"]   # removed recursively
drop_where = [{ key = "visible", equals = false }]    # drop array elements with this top-level field
max_depth = 8                                          # root=0; depth >= max_depth becomes "…"

[[filter.tests]]
name = "drops hidden nodes and vector paths"
input = '{"children":[{"visible":false},{"name":"keep","vectorPaths":[1]}]}'
absent = ["visible", "vectorPaths"]
present = ["keep"]
```

```bash
frtk verify                            # run filters' inline tests (exit≠0 on failure; --require-all for CI)
frtk trust                             # trust ./.figma-rtk filters; frtk trust --list / frtk untrust
frtk serve --level aggressive          # apply filters (or set level in config.toml)
```

Because aggressive filters can drop data, keep `tee` on (`[tee] mode = "always"`)
so the raw pre-filter payload stays recoverable.

### Fixture / TDD harness

```bash
frtk serve --capture-dir ./fixtures    # dump raw target responses for offline work
frtk compress --tool get_design_context --level aggressive < fixtures/get_design_context-0000.json
```

## Not in v1 (later phases)

Delta cache on re-reads, `frtk discover` (analyse the savings ledger frtk
already writes to surface un-proxied or high-cost Figma calls), FigmaKit write
preamble, `assertNode` verification, `figma.manifest.json`. The proxy + config +
tee + capture + filter engine built here are the foundation those build on.

## Layout

| file          | role                                                    |
|---------------|---------------------------------------------------------|
| `proxy.rs`    | axum catch-all reverse proxy, header filtering, framing |
| `mcp.rs`      | JSON-RPC id↔tool correlation, JSON + SSE transforms     |
| `compress.rs` | near-lossless text compression strategies               |
| `stats.rs`    | savings ledger + `gain` report                          |
| `tokens.rs`   | ~4-bytes/token estimate                                 |
