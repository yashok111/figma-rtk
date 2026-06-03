#!/usr/bin/env bash
# install-skills.sh — install Claude Code skills for figma-rtk (frtk)
#
# Stack: Rust (rustup/cargo), axum reverse proxy, tokio async, hyper/reqwest,
#        serde/serde_json JSON-RPC transforms, TOML declarative filters,
#        SSE streaming, OAuth RFC 9728 discovery rewriting, strict TDD Waves.
#
# Usage:
#   ./scripts/install-skills.sh               # install all skill sets
#   ./scripts/install-skills.sh core          # install core skills only
#   ./scripts/install-skills.sh useful        # install useful skills only
#   ./scripts/install-skills.sh maybe         # install maybe skills only
#   ./scripts/install-skills.sh rust axum tdd # install named groups
#   ./scripts/install-skills.sh --dry-run ...  # print commands without running
#   ./scripts/install-skills.sh -n ...         # same as --dry-run
#   ./scripts/install-skills.sh -h             # show help
#
# Note: skills install with '-a universal' so they land in <repo>/.agents/skills/
# (same convention as the telega repo's install-skills.sh). For a Claude-Code-only
# project you could switch to '-a claude' to install into .claude/skills/ instead.
# Always run with --dry-run first to eyeball the npx commands before installing.

set -euo pipefail

# ── repo roots ─────────────────────────────────────────────────────────────────
REPO_APOLLO="https://github.com/apollographql/skills"
REPO_LEONARDOMSO="https://github.com/leonardomso/rust-skills"
REPO_ZHANG="https://github.com/zhanghandong/rust-skills"          # redirects → actionbook/rust-skills
REPO_MELONASK="https://github.com/melonask/axum-skills"
REPO_IMPERTIO="https://github.com/Impertio-Studio/Axum-Claude-Skill-Package"
REPO_OBRA="https://github.com/obra/superpowers"
REPO_TRAILOFBITS="https://github.com/trailofbits/skills"
REPO_RUSTFS="https://github.com/rustfs/rustfs"
REPO_UDAPY="https://github.com/udapy/rust-agentic-skills"
REPO_CHRISHUFFMAN="https://github.com/chrishuffman5/domain-expert"
REPO_LAURIGATES="https://github.com/laurigates/claude-plugins"
REPO_MATTPOCOCK="https://github.com/mattpocock/skills"
REPO_NGVOICU="https://github.com/ngvoicu/specmint-tdd"
REPO_QIAO="https://github.com/qiao/rust-api-guidelines-skill"
REPO_NANLONG="https://github.com/nanlong/rust-architect"
REPO_NETRESEARCH="https://github.com/netresearch/git-workflow-skill"
REPO_AWESOME_SKILLS="https://github.com/awesome-skills/code-review-skill"
REPO_TERRYLICA="https://github.com/terrylica/cc-skills"
REPO_GETSENTRY="https://github.com/getsentry/skills"
REPO_VERCEL_LABS="https://github.com/vercel-labs/skills"
REPO_ANTHROPICS="https://github.com/anthropics/skills"
REPO_MURATCANKOYLAN="https://github.com/muratcankoylan/Agent-Skills-for-Context-Engineering"
REPO_KOCHETKOV="https://github.com/kochetkov-ma/claude-brewcode"

# ── CORE skills ────────────────────────────────────────────────────────────────

# Idiomatic Rust: clippy config, error handling (thiserror/anyhow), no-clone,
# testing patterns, generics, Send+Sync — baseline hygiene for every frtk module.
APOLLO_SKILLS=(
  rust-best-practices
)

# 179 rules across async, error handling, API design, performance, testing —
# broad Rust coverage for the axum/tokio reverse-proxy codebase.
LEONARDOMSO_SKILLS=(
  rust-skills
)

# Result vs panic, error hierarchies, caller-visible error design — directly
# applicable to proxy.rs / mcp.rs / compress.rs / filter.rs Result paths.
# Also: axum State extractors, async Send+Sync, tracing+tower layers (domain-web).
ZHANG_CORE_SKILLS=(
  m06-error-handling
  domain-web
)

# axum routing, extractors, middleware, AppState, SSE, tower-http, tower::ServiceExt
# testing — the exact surface used in proxy.rs and the proxy integration tests.
MELONASK_SKILLS=(
  axum
)

# tower Service/Layer composition, ServiceBuilder, axum::serve — the middleware
# stack that frtk's catch-all reverse proxy is built on.
IMPERTIO_CORE_SKILLS=(
  axum-impl-tower-stack
)

# Red-green-refactor enforcement, verification gate, structured debugging, code
# review dispatch, plan writing and execution — matches frtk's strict TDD + Wave
# workflow documented in CLAUDE.md.
OBRA_CORE_SKILLS=(
  test-driven-development
  verification-before-completion
  systematic-debugging
  requesting-code-review
  writing-plans
  executing-plans
)

# Risk-first diff review (auth/crypto/external-interaction vectors) — directly
# relevant to OAuth discovery rewriting, Bearer token relay, and trust-gated
# project-local filters in frtk.
TRAILOFBITS_CORE_SKILLS=(
  differential-review
)

# ── USEFUL skills ──────────────────────────────────────────────────────────────

# code-change-verification: diff/PR review gate matching the two-factor Wave review.
RUSTFS_SKILLS=(
  code-change-verification
)

# Concurrency primitive selection (CPU-bound vs I/O-bound), DeltaCache shared
# state reasoning, tokio task model — secondary but real for the async proxy.
# coding-guidelines: 50 idiomatic Rust naming/iterator/newtype rules.
# rust-skill-creator: on-demand tokio/serde/axum crate doc skills.
# domain-cli: clap derive macros, CLI>env>config precedence, exit codes for frtk's thin CLI layer.
ZHANG_USEFUL_SKILLS=(
  m07-concurrency
  coding-guidelines
  rust-skill-creator
  domain-cli
)

# Idiomatic Rust, Clippy-clean output, type safety — enforces the frtk CLAUDE.md
# requirement of clean clippy and strict TDD.
# Install slug is the display name (spaces+caps), not the folder name.
UDAPY_SKILLS=(
  'Rust Core Specialist'
)

# axum SSE (Sse, Event, KeepAlive) — relevant to the transform_sse path in
# mcp.rs/proxy.rs.  axum-core-async-performance: tokio::spawn_blocking, avoiding
# runtime blocking, connection pool sizing for a concurrent reverse-proxy service.
# axum-core-architecture: tokio+hyper+tower composition, debug_handler,
# server-exits-immediately debugging — all applicable to proxy.rs.
IMPERTIO_USEFUL_SKILLS=(
  axum-impl-sse
  axum-core-async-performance
  axum-core-architecture
)

# api-realtime SSE sub-skill: text/event-stream wire format, proxy buffering
# pitfalls, MCP transport — the transform_sse streaming proxy path.
# backend: rust-web sub-skill covering axum, Tower Service/Layer, extractors,
# Tokio, Serde, thiserror/anyhow/IntoResponse.
CHRISHUFFMAN_SKILLS=(
  api-realtime
  backend
)

# cargo-nextest: process-per-test isolation and filter expressions (--lib filter::
# pattern already used in CLAUDE.md). cargo-llvm-cov: HTML/lcov coverage reports
# and CI threshold enforcement for the strict-TDD suite.
LAURIGATES_SKILLS=(
  cargo-nextest
  cargo-llvm-cov
)

# Behavior-first TDD, vertical slicing, red-green-refactor — language-agnostic
# enforcement layer complementing the project's TDD mandate.
MATTPOCOCK_SKILLS=(
  tdd
)

# Structured TDD enforcement (alternating TEST-IMPL pairs, .specs/ audit trail)
# aligned with frtk's strict inline TDD convention.
NGVOICU_SKILLS=(
  specmint-tdd
)

# Rust public API naming, trait impls, type safety, and interoperability
# guidelines — applicable during code review and API design work.
QIAO_SKILLS=(
  rust-api-guidelines
)

# Stack-agnostic workflow skills for the Wave methodology: parallel subagent
# fan-out (dispatching-parallel-agents), git worktree isolation (using-git-worktrees),
# review feedback handling (receiving-code-review), branch finishing and design
# sign-off before coding (brainstorming, finishing-a-development-branch).
# subagent-driven-development: parallel cavecrew-reviewer subagents.
OBRA_USEFUL_SKILLS=(
  subagent-driven-development
  dispatching-parallel-agents
  using-git-worktrees
  receiving-code-review
  finishing-a-development-branch
  brainstorming
)

# Checks trust store (trust.rs), TOML config loading, env-var overrides
# (FRTK_LEDGER/CONFIG/TRUST), and fail-secure patterns for the proxy.
TRAILOFBITS_USEFUL_SKILLS=(
  insecure-defaults
)

# feat/<name> branching convention, PR workflows, merge practices — matches
# the CLAUDE.md no-commit-without-command and branch hygiene rules.
NETRESEARCH_SKILLS=(
  git-workflow
)

# Diff/PR review covering Rust ownership/borrowing/async — relevant to frtk's
# axum/async codebase and the two-factor review gate convention.
AWESOME_SKILLS=(
  code-review-excellence
)

# OAuth proxy patterns: WWW-Authenticate rewriting, /.well-known/oauth-protected-resource,
# auth failure diagnosis — aligns with CLAUDE.md's RFC 9728 origin check concern.
TERRYLICA_SKILLS=(
  claude-code-proxy-patterns
)

# Scans SKILL.md files for prompt injection, malicious code, excessive permissions,
# and supply-chain risks before adoption — relevant whenever new skills are added.
GETSENTRY_SKILLS=(
  skill-scanner
)

# ── MAYBE skills ───────────────────────────────────────────────────────────────

# unsafe-checker: relevant to Rust but frtk has no unsafe blocks currently.
# rust-architect: useful only when planning a new phase from scratch (ADR / Director
#   handoff), not for routine feature work.
# m11-ecosystem: generic crate-selection guidance; project crate choices already settled.
# networking: IT-ops networking (routing, VPN) — tangential to HTTP proxy concerns.
# cargo-fuzz: useful for adversarial JSON parsing testing but not a workflow staple.
# find-skills: meta skill-discovery; no Rust/proxy/TDD value for this project.
# skill-creator: for creating new skills — not core dev workflow.
# context-compression: AI inference context-management — no Rust/proxy connection.
# text-optimizer: prompt/.md token reduction — project already has RTK/frtk for that.
ZHANG_MAYBE_SKILLS=(
  unsafe-checker
  m11-ecosystem
)

NANLONG_SKILLS=(
  rust-architect
)

# Empty: m11-ecosystem was moved to ZHANG_MAYBE_SKILLS (zhanghandong/rust-skills).
RUSTFS_MAYBE_SKILLS=()

CHRISHUFFMAN_MAYBE_SKILLS=(
  networking
)

TRAILOFBITS_MAYBE_SKILLS=(
  cargo-fuzz
)

VERCEL_LABS_SKILLS=(
  find-skills
)

ANTHROPICS_SKILLS=(
  skill-creator
)

MURATCANKOYLAN_SKILLS=(
  context-compression
)

KOCHETKOV_SKILLS=(
  text-optimizer
)

# ── argument parsing ───────────────────────────────────────────────────────────
DRY_RUN=0
TARGETS=()

usage() {
  cat <<'HELP'
Usage: ./scripts/install-skills.sh [OPTIONS] [TARGETS...]

TARGETS (default: all):
  core      apollographql rust-best-practices, leonardomso rust-skills,
            zhang m06-error-handling+domain-web, melonask axum,
            impertio axum-impl-tower-stack, obra core 6, trailofbits differential-review
  useful    all useful-tier groups
  maybe     all maybe-tier groups (install at your own risk)
  rust      apollographql + leonardomso + zhang-core + udapy + rustfs + qiao
  axum      melonask + impertio (all)
  tdd       obra-core + mattpocock + ngvoicu + laurigates
  workflow  obra-useful + netresearch + awesome-skills
  security  trailofbits (all) + getsentry + terrylica

OPTIONS:
  --dry-run, -n   Print install commands without executing them
  -h, --help      Show this help
HELP
  exit 0
}

for arg in "$@"; do
  case "$arg" in
    --dry-run|-n) DRY_RUN=1 ;;
    -h|--help)    usage ;;
    *)            TARGETS+=("$arg") ;;
  esac
done

[[ ${#TARGETS[@]} -eq 0 ]] && TARGETS=("all")

# ── helpers ────────────────────────────────────────────────────────────────────
run() {
  echo "+ $*"
  if [[ $DRY_RUN -eq 0 ]]; then
    "$@"
  fi
}

# install_set <repo_url> <skill> [skill ...]
# Installs one or more skills from the given repo via the skills CLI.
install_set() {
  local repo="$1"; shift
  run npx -y -p skills skills add "$repo" -y -a universal --skill "$@"
}

# ── cd to repo root ────────────────────────────────────────────────────────────
cd "$(dirname "$0")/.."

# ── SELECTED set assembly ──────────────────────────────────────────────────────
SELECTED=()
for t in "${TARGETS[@]}"; do
  case "$t" in
    all)
      # Expands directly to individual group names so the dispatch loop below
      # can match them. (Symbolic names like 'core'/'useful' have no dispatch
      # case and would silently skip all installs — do NOT use them here.)
      SELECTED+=(
        apollo-core
        leonardomso-core
        zhang-core
        melonask-core
        impertio-core
        obra-core
        trailofbits-core
        rustfs-useful
        zhang-useful
        udapy-useful
        impertio-useful
        chrishuffman-useful
        laurigates-useful
        mattpocock-useful
        ngvoicu-useful
        qiao-useful
        obra-useful
        trailofbits-useful
        netresearch-useful
        awesome-useful
        terrylica-useful
        getsentry-useful
      )
      ;;
    core)
      SELECTED+=(
        apollo-core
        leonardomso-core
        zhang-core
        melonask-core
        impertio-core
        obra-core
        trailofbits-core
      )
      ;;
    useful)
      SELECTED+=(
        rustfs-useful
        zhang-useful
        udapy-useful
        impertio-useful
        chrishuffman-useful
        laurigates-useful
        mattpocock-useful
        ngvoicu-useful
        qiao-useful
        obra-useful
        trailofbits-useful
        netresearch-useful
        awesome-useful
        terrylica-useful
        getsentry-useful
      )
      ;;
    maybe)
      SELECTED+=(
        zhang-maybe
        nanlong-maybe
        rustfs-maybe
        chrishuffman-maybe
        trailofbits-maybe
        vercel-labs-maybe
        anthropics-maybe
        muratcankoylan-maybe
        kochetkov-maybe
      )
      ;;
    rust)
      SELECTED+=(apollo-core leonardomso-core zhang-core zhang-useful udapy-useful rustfs-useful qiao-useful)
      ;;
    axum)
      SELECTED+=(melonask-core impertio-core impertio-useful)
      ;;
    tdd)
      SELECTED+=(obra-core mattpocock-useful ngvoicu-useful laurigates-useful)
      ;;
    workflow)
      SELECTED+=(obra-useful netresearch-useful awesome-useful)
      ;;
    security)
      SELECTED+=(trailofbits-core trailofbits-useful getsentry-useful terrylica-useful)
      ;;
    *)
      echo "Unknown target: $t" >&2; exit 1 ;;
  esac
done

# dedup while preserving order
DEDUPED=()
declare -A _SEEN
for s in "${SELECTED[@]}"; do
  if [[ -z "${_SEEN[$s]+x}" ]]; then
    DEDUPED+=("$s")
    _SEEN[$s]=1
  fi
done

# ── install ────────────────────────────────────────────────────────────────────
for group in "${DEDUPED[@]}"; do
  case "$group" in

    # CORE ─────────────────────────────────────────────────────────────────────
    apollo-core)
      install_set "$REPO_APOLLO"       "${APOLLO_SKILLS[@]}" ;;
    leonardomso-core)
      install_set "$REPO_LEONARDOMSO"  "${LEONARDOMSO_SKILLS[@]}" ;;
    zhang-core)
      install_set "$REPO_ZHANG"        "${ZHANG_CORE_SKILLS[@]}" ;;
    melonask-core)
      install_set "$REPO_MELONASK"     "${MELONASK_SKILLS[@]}" ;;
    impertio-core)
      install_set "$REPO_IMPERTIO"     "${IMPERTIO_CORE_SKILLS[@]}" ;;
    obra-core)
      install_set "$REPO_OBRA"         "${OBRA_CORE_SKILLS[@]}" ;;
    trailofbits-core)
      install_set "$REPO_TRAILOFBITS"  "${TRAILOFBITS_CORE_SKILLS[@]}" ;;

    # USEFUL ───────────────────────────────────────────────────────────────────
    rustfs-useful)
      install_set "$REPO_RUSTFS"       "${RUSTFS_SKILLS[@]}" ;;
    zhang-useful)
      install_set "$REPO_ZHANG"        "${ZHANG_USEFUL_SKILLS[@]}" ;;
    udapy-useful)
      install_set "$REPO_UDAPY"        "${UDAPY_SKILLS[@]}" ;;
    impertio-useful)
      install_set "$REPO_IMPERTIO"     "${IMPERTIO_USEFUL_SKILLS[@]}" ;;
    chrishuffman-useful)
      install_set "$REPO_CHRISHUFFMAN" "${CHRISHUFFMAN_SKILLS[@]}" ;;
    laurigates-useful)
      install_set "$REPO_LAURIGATES"   "${LAURIGATES_SKILLS[@]}" ;;
    mattpocock-useful)
      install_set "$REPO_MATTPOCOCK"   "${MATTPOCOCK_SKILLS[@]}" ;;
    ngvoicu-useful)
      install_set "$REPO_NGVOICU"      "${NGVOICU_SKILLS[@]}" ;;
    qiao-useful)
      install_set "$REPO_QIAO"         "${QIAO_SKILLS[@]}" ;;
    obra-useful)
      install_set "$REPO_OBRA"         "${OBRA_USEFUL_SKILLS[@]}" ;;
    trailofbits-useful)
      install_set "$REPO_TRAILOFBITS"  "${TRAILOFBITS_USEFUL_SKILLS[@]}" ;;
    netresearch-useful)
      install_set "$REPO_NETRESEARCH"  "${NETRESEARCH_SKILLS[@]}" ;;
    awesome-useful)
      install_set "$REPO_AWESOME_SKILLS" "${AWESOME_SKILLS[@]}" ;;
    terrylica-useful)
      install_set "$REPO_TERRYLICA"    "${TERRYLICA_SKILLS[@]}" ;;
    getsentry-useful)
      install_set "$REPO_GETSENTRY"    "${GETSENTRY_SKILLS[@]}" ;;

    # MAYBE ────────────────────────────────────────────────────────────────────
    zhang-maybe)
      install_set "$REPO_ZHANG"        "${ZHANG_MAYBE_SKILLS[@]}" ;;
    nanlong-maybe)
      install_set "$REPO_NANLONG"      "${NANLONG_SKILLS[@]}" ;;
    rustfs-maybe)
      # RUSTFS_MAYBE_SKILLS is currently empty (m11-ecosystem moved to zhang-maybe).
      [[ ${#RUSTFS_MAYBE_SKILLS[@]} -gt 0 ]] && install_set "$REPO_RUSTFS" "${RUSTFS_MAYBE_SKILLS[@]}" ;;
    chrishuffman-maybe)
      install_set "$REPO_CHRISHUFFMAN" "${CHRISHUFFMAN_MAYBE_SKILLS[@]}" ;;
    trailofbits-maybe)
      install_set "$REPO_TRAILOFBITS"  "${TRAILOFBITS_MAYBE_SKILLS[@]}" ;;
    vercel-labs-maybe)
      install_set "$REPO_VERCEL_LABS"  "${VERCEL_LABS_SKILLS[@]}" ;;
    anthropics-maybe)
      install_set "$REPO_ANTHROPICS"   "${ANTHROPICS_SKILLS[@]}" ;;
    muratcankoylan-maybe)
      install_set "$REPO_MURATCANKOYLAN" "${MURATCANKOYLAN_SKILLS[@]}" ;;
    kochetkov-maybe)
      install_set "$REPO_KOCHETKOV"    "${KOCHETKOV_SKILLS[@]}" ;;

  esac
done

echo ""
echo "Done. Re-start Claude Code (or /mcp refresh) to pick up new skills."
