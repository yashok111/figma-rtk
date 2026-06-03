# figma-rtk — Handoff for a fresh agent

You are picking up **figma-rtk** (binary `frtk`), a Rust token-killer **reverse
proxy** for the Figma MCP server — "RTK for Figma". Read this top to bottom, then
read `README.md` and the memory files (below) before changing anything.

Working dir: `/home/yakov/projects/claude-figma`. Date context: project started
2026-06-03.

---

## ⏯ RESUME HERE (2026-06-03, 4 Waves DONE — filter live + ultra + more tools)

Build green (**66 tests**, clippy clean). The `design-context-lean` filter is **LIVE**
(proxy trusted + running `--level aggressive`, pid ~446962). Four Waves shipped this
session, each TDD'd + dual-reviewed (correctness + data-integrity) + Telegram-reported.
Full detail in memory `figma-rtk-project.md` **Phase 2d + 2e**.

- **W1:** filter live — real proxied read confirms boilerplate stripped, Font kept,
  raw recoverable. ~9% on the aggressive call.
- **W2:** ultra code-string compression — `strip_figma_node_attrs` (compress.rs) drops
  `data-node-id`/`data-name` from JSX; **ultra-only**, gated to get_design_context.
  Real fixture: **ultra = 20.3%** vs aggressive 9.5%. Enable live via serve `--level ultra`.
- **W3:** TARGET_TOOLS += `get_variable_defs` (real fixture) + `get_code_connect_map`
  (blind — needs a Figma Developer seat; live read 403'd).
- **W4:** TARGET_TOOLS += `use_figma` (JSON returns / node-tree dumps → minify+delta);
  delta sentinel reworded write-safe. `preamble`/`assertNode`/`manifest` SCOPED OUT
  (skill/agent layer, not read-proxy).

Fixtures: `get_design_context-0000.json`, `get_variable_defs-sample.json`,
`use_figma-nodetree-sample.json`. NOT git → nothing committed.

**Known nit:** capture overwrites fixtures from index 0000 on each serve restart
(per-process counter; capture.rs should find the next free index).

**Next options:** deeper get_design_context code compression (dedupe repeated sibling
divs / Tailwind soup — needs a careful text transform); flip ultra live; capture
get_code_connect_map once a Dev seat exists.

---

### (historical) RESUME — capture + first filter (Phase 2d, now part of the run above)

Real `get_design_context` captured through the plugin proxy; aggressive
`design-context-lean` filter TDD'd, verified, reviewed. OAuth unblock needed
`/mcp` → reconnect (a plain CC restart did NOT clear stale discovery). No 401 on
the data call → token audience-binding residual risk CLEARED.

---

### (historical) prior RESUME — capture flow (now complete)

The whole build is green (53 tests, clippy clean). One task is live: **capture a
real `get_design_context` payload through the proxy, then TDD an aggressive
filter against it.** Everything is wired; you are mid-flow after a CC restart.

**What just happened (do not redo):**
- `frtk init` is applied — the figma **plugin** `.mcp.json` points at the proxy.
- The proxy's **OAuth discovery rewrite is SHIPPED + verified live** (see §7). A
  transparent proxy needs it or CC's RFC 9728 check refuses to auth.
- CC was restarted to clear its **in-memory** stale discovery cache (no on-disk
  cache exists — checked). So a fresh discovery fetch will now hit the fixed
  proxy and pass the origin check.

**🚨 The single biggest trap — TWO figma MCP servers are connected:**
- `mcp__claude_ai_Figma__*` = claude.ai account connector. **UNPROXIED**, already
  authed. A read here bypasses frtk and captures NOTHING. (This already burned
  one capture attempt.)
- `mcp__plugin_figma_figma__*` = the plugin server, repointed to the proxy.
  **ALL capture reads MUST use this one.**

**Do, in order:**
1. **Confirm the proxy is up:** `ss -ltnp | grep 7337` should show `frtk`. If not,
   relaunch detached (survives CC restarts):
   `setsid ./target/release/frtk serve --capture-dir /home/yakov/projects/claude-figma/fixtures >/tmp/frtk-serve.log 2>&1 </dev/null & disown`
   Sanity: `curl -s http://127.0.0.1:7337/.well-known/oauth-protected-resource`
   must show `"resource":"http://127.0.0.1:7337/mcp"` (rewritten). If it shows
   `mcp.figma.com`, the running proxy is the OLD binary → `cargo build --release`
   (with `source "$HOME/.cargo/env"`) then kill its PID (NOT `pkill -f` — that
   killed the launching shell once) and relaunch.
2. **Authenticate the plugin server:** call `mcp__plugin_figma_figma__authenticate`
   → it returns an OAuth URL → give it to the user, they complete the browser
   flow. (Or they `/mcp` → `plugin:figma:figma` → authenticate.) If it STILL
   errors `Protected resource … does not match`, CC didn't re-fetch discovery —
   ask the user to fully restart CC again.
3. **One real read THROUGH THE PLUGIN SERVER** (not the connector):
   `mcp__plugin_figma_figma__get_design_context` with the Good Vibez landing hero —
   fileKey `4qjWA1U3dG1JvdlvCvhsVe`, nodeId `136-2`
   (`https://www.figma.com/design/4qjWA1U3dG1JvdlvCvhsVe/Good-Vibez-%E2%80%94-Landing?node-id=136-2`).
   A fixture should appear in `fixtures/` (and tee). Confirm: `ls -la fixtures/`.
4. **Watch the residual risk** (§7): the token is minted with
   `resource=http://127.0.0.1:7337/mcp`. Figma's AS advertises no
   `resource_indicators_supported`, so it *should* ignore the param — but if the
   read 401s instead of returning design data, the token audience binding is
   stricter than assumed → that's a real new blocker; report it and stop.
5. **TDD the filter:** with a real fixture in hand, write an aggressive
   `get_design_context` filter (`filter.rs` / TOML) against it — Wave flow (§6):
   implement → 2× cavecrew-reviewer (correctness + data-integrity, sonnet) →
   fix blockers → re-review → Telegram report (§8).

Detail on the OAuth fix, the residual risk, and unblocked alternatives: **§7**.

---

## 1. What it is & why

Claude Code talks to Figma via the **remote HTTP** MCP server at
`https://mcp.figma.com/mcp` (NOT stdio). figma-rtk is a transparent HTTP reverse
proxy that sits in between and compresses the heavy read-tool responses before
they reach the agent:

```
Claude Code  ──►  frtk (localhost:7337)  ──►  https://mcp.figma.com/mcp
```

**Auth model (verified):** the OAuth authorization server is a *separate* origin
(`api.figma.com`, scope `mcp:connect`). Claude Code does OAuth directly against
it; the proxy only relays the `Authorization: Bearer` header. The token audience
stays `mcp.figma.com`, so Figma accepts the relayed token. **Never log headers or
request/response bodies — they carry the Bearer token.** This invariant has held
through every review; keep it.

---

## 2. Current state (all green)

~42 tests pass, `clippy` clean. Built across these phases, each gated by a
two-vector review:

- **v1:** transparent relay + auth passthrough; near-lossless compression of
  `get_design_context` / `get_metadata` (JSON minify, XML indent strip, code
  blank-line collapse) for both `application/json` and SSE responses; `gain`
  token meter.
- **2a:** `frtk init`/`--uninstall` (atomic edit of the plugin `.mcp.json`),
  `tee` raw-recovery (0600), `config.toml`, `frtk compress` stdin harness,
  shared `fsutil`.
- **2b:** filter engine — TOML filters (`drop_keys`/`drop_where`/`max_depth`) +
  inline tests, `trust`/`untrust`, `frtk verify`, `serve --level`. Filters apply
  only at `aggressive`/`ultra`.
- **2c:** opt-in delta cache (`[cache] delta`) — collapses byte-identical
  re-reads of the same target to a sentinel.

---

## 3. Module map (`src/`)

| file | role |
|------|------|
| `main.rs` | clap CLI: `serve`, `gain`, `config`, `init`, `compress`, `verify`, `trust`, `untrust` |
| `proxy.rs` | axum catch-all reverse proxy; header filtering; GET=stream passthrough, POST=buffer+transform; loads config/filters/cache; `AppState`, `build_state`/`build_state_with`, `app()` |
| `mcp.rs` | JSON-RPC awareness: `Target{tool,key}`, `extract_targets`, `transform_value`/`transform_sse`/`transform_msg` thread (level, filters, delta cache) |
| `compress.rs` | `compress_text` (whitespace), `compress_result_with` (level+filters), `compress_payload` (CLI harness), `Savings` |
| `filter.rs` | TOML `Filter`/`FilterSet`, `apply_value` transforms, inline `run_tests` |
| `trust.rs` | trust store gating project-local filters |
| `cache.rs` | `DeltaCache` (Mutex<HashMap<key,u64 hash>>, evict-one at cap), `apply_delta` |
| `config.rs` | `Config` (level, exclude_tools, `[tee]`, `[cache]`), load/create |
| `tee.rs` | last-raw-per-tool recovery (owner-only) |
| `capture.rs` | `--capture-dir` fixture dump |
| `fsutil.rs` | `safe_name` (sanitize, never empty), `create_private` (0600) |
| `stats.rs` | JSONL savings ledger + `gain` report |
| `tokens.rs` | ~4-bytes/token estimate |
| `tests/proxy_it.rs` | mock-upstream end-to-end tests (compression, capture, meter, delta) |

---

## 4. Build / test / run

**Rust is installed via rustup and is NOT on the default PATH.** The shell does
not persist between Bash calls, so prefix every cargo command:

```bash
source "$HOME/.cargo/env"; cd /home/yakov/projects/claude-figma && cargo test
source "$HOME/.cargo/env"; cd /home/yakov/projects/claude-figma && cargo clippy --all-targets
source "$HOME/.cargo/env"; cd /home/yakov/projects/claude-figma && cargo build --release
```

**`cargo` output is rewritten by the user's RTK shell hook** — `cargo test`
prints a compressed line like `cargo test: 42 passed (4 suites)`. To see raw
output, read the tee log path it prints (`~/.local/share/rtk/tee/...`).

CLI: `frtk serve [--port 7337] [--level …] [--capture-dir …]` · `frtk init
[--uninstall]` · `frtk gain [--history]` · `frtk compress --tool X [--level …]
[--filters …]` (reads stdin) · `frtk verify [--filters …] [--require-all]` ·
`frtk trust [--list]` / `frtk untrust` · `frtk config [--create]`.

Env overrides (used by tests, hermetic): `FRTK_LEDGER`, `FRTK_CONFIG`,
`FRTK_TRUST`. Integration tests serialize `FRTK_LEDGER` mutation via a module
`ENV_LOCK` mutex.

---

## 5. Conventions & gotchas

- **Strict TDD.** Write/adjust the test, see it fail, implement, green. Every
  feature has unit tests; the proxy has a mock-upstream integration test
  (`tests/proxy_it.rs`) — copy that pattern (spawn an axum mock, drive
  `proxy::app(state)` via `tower`'s `oneshot`).
- **Never log Bearer/headers/bodies.**
- **Tool-name matching uses a `__` boundary**, not loose `ends_with` — both
  `mcp::is_target` and `filter::matches_tool`. Don't reintroduce loose suffix
  matching (it over-matches, e.g. "metadata" → "set_metadata").
- Delta cache hashes the **post-compression** serialized result (deterministic →
  correct identity). Intended; documented in `cache.rs`.
- Aggressive/ultra filters can DROP data → keep `tee` recoverable; `serve` warns
  if level is aggressive+ and tee is off.
- **Not a git repo.** No `origin`. Do **not** `git init` / commit / push without
  an explicit user command.
- The user's CC transcripts contain **zero real figma MCP calls** (so `frtk
  discover` would find nothing — it was dropped as a next step).

---

## 6. The user's working flow (follow it)

The user works in **Waves**:
1. Read the task, write acceptance criteria. Study relevant code + applicable skills.
2. Plan as Waves (a Wave = tasks parallelizable without file/module conflicts).
   Within a Wave, independent tasks → parallel `caveman:cavecrew-builder`
   subagents (the user calls these "cavecrew-implementer"; builder hard-refuses
   3+ files, so do those inline). Sequential/single tasks → inline.
3. After each Wave: **two-factor review** = 2× `caveman:cavecrew-reviewer` in
   parallel (one message, two calls), same diff, different vectors (e.g.
   correctness + security/data-integrity), `model: 'sonnet'`. A blocker from
   either → fix → re-review until both approve. Nits collected, not blocking.
4. **Telegram report per Wave** (see §8): Wave #+name, what done, ✅/❌ per
   reviewer + findings, files, next Wave.
5. Track Waves/subtasks with `TaskCreate`/`TaskUpdate`.

Rules: don't ask clarifying questions — make reasonable decisions and proceed;
**no commits/pushes without explicit command**; surface a *real* blocker
(needs user action, contradictory reqs) to Telegram and wait.

If you author a **Workflow** (multi-agent): per the user's CLAUDE.md, every
`agent()` call MUST carry an explicit `model`; default `sonnet`, escalate to
`opus` only for genuinely hard/irreversible reasoning; producers in a fan-out
are sonnet/haiku; review-gate reviewers are sonnet by default.

---

## 7. Next steps

### Wiring state (done 2026-06-03)
- `frtk init` applied: the **plugin** figma `.mcp.json`
  (`~/.claude/plugins/cache/claude-plugins-official/figma/2.2.12/.mcp.json`) is
  repointed to `http://127.0.0.1:7337/mcp`; backup `.mcp.json.frtk-backup` holds
  the original `https://mcp.figma.com/mcp`. Revert: `frtk init --uninstall`.
- Proxy runs detached: `frtk serve --capture-dir <proj>/fixtures` (own session,
  survives a CC restart). `fixtures/` is the capture dir.

### ⚠️ TWO figma MCP servers are connected — read the right one
- `mcp__claude_ai_Figma__*` = the **claude.ai account connector** (managed by
  claude.ai, talks straight to `mcp.figma.com`). It is **NOT proxied** and is
  already authed. A read here bypasses frtk and captures **nothing** — this is
  why the first capture attempt left `fixtures/` empty.
- `mcp__plugin_figma_figma__*` = the **official figma plugin** server — the one
  `frtk init` repointed to the proxy. **The fixture-capturing read MUST go
  through this server.** Pre-auth it only exposes `authenticate` /
  `complete_authentication`; the real tools appear after OAuth.

### OAuth discovery rewrite — SHIPPED (2026-06-03)
A transparent proxy breaks CC's RFC 9728 check: CC dials the proxy but Figma's
discovery advertises `resource=https://mcp.figma.com/mcp`, so the SDK refuses
("Protected resource … does not match expected http://127.0.0.1:7337/mcp"). Fix
in `proxy.rs`: `handle` rewrites the 401 `WWW-Authenticate` `resource_metadata`
→ proxy PRM url; `proxy_once` intercepts `GET /.well-known/oauth-protected-resource`
and `rewrite_prm_resource` rewrites `resource` → proxy origin. `authorization_servers`
/ `authorization_uri` stay = `api.figma.com`, so OAuth still runs directly
CC↔figma and the proxy never sees the token exchange. Host header is sanitized
(`is_safe_host`) before reflection. Verified LIVE via curl. Tests in
`proxy_it.rs::oauth_discovery_rewritten_to_proxy_origin` + unit tests in
`proxy.rs`.

### BLOCKED on the user: force a fresh discovery, then auth
CC **caches** discovery from first connect (pre-fix), so `authenticate` still
reports the old `resource` even though the live proxy now serves the rewritten
one. To clear it: **restart Claude Code** (or `/mcp` → reconnect figma) so the
SDK re-fetches discovery against the fixed proxy, then authenticate the figma
**plugin** server → complete the browser OAuth. Then do ONE
`mcp__plugin_figma_figma__get_design_context` read → fixture lands in
`fixtures/`, which you TDD a `get_design_context` filter against.

**Residual risk (verify on first data call):** the token is minted with
`resource=http://127.0.0.1:7337/mcp`. Figma's AS advertises no
`resource_indicators_supported`, so it should ignore the param and the relayed
token should be accepted at `mcp.figma.com`. If a data call 401s after auth, the
audience binding is stricter than expected → deeper fix needed.

**Unblocked options (if capture stays blocked):**
- Extend `TARGET_TOOLS` to `get_variable_defs` / `get_code_connect_map` (+ tests).
- FigmaKit write-side preamble, `assertNode` verification, `figma.manifest.json`
  / cold-start bundle (see the brainstorm in the session history / memory).

When unsure what to do next, **send the user the options on Telegram and wait**.

---

## 8. Telegram

The user drives from Telegram and expects per-Wave reports there. Reply via the
telegram `reply` MCP tool with `chat_id: "-1003914957143"` (user
`@admiralbondage`). Your terminal output does NOT reach them — anything they need
must go through the reply tool. This CC session is shim **@s9**.

## 9. Memory

Project memory lives at
`~/.claude/projects/-home-yakov-projects-claude-figma/memory/`:
`figma-rtk-project.md` (status/architecture), `rtk-patterns-to-adopt.md`
(RTK features worth porting), indexed in `MEMORY.md`. Update them as you go.
