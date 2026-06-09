//! The reverse proxy itself: a catch-all axum handler that forwards every
//! request to the upstream Figma MCP server and, for POST responses carrying
//! results of the heavy read tools, compresses them on the way back.

use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::response::Builder as ResponseBuilder;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures::StreamExt;
use reqwest::Client;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

use crate::cache::DeltaCache;
use crate::config::{Level, TeeMode};
use crate::filter::FilterSet;

/// Request headers we must NOT forward upstream. `accept-encoding` is stripped
/// so the upstream answers in plain text and we can parse/transform it.
const REQ_HOP_BY_HOP: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "accept-encoding",
    "upgrade",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
];

/// Response headers we must not echo back verbatim — the body size and framing
/// can change after compression, so let the server recompute them.
const RESP_HOP_BY_HOP: &[&str] = &[
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
];

/// Max request body we will buffer (tool-call params are tiny; this is a guard).
/// Exposed `pub` so integration tests can reference it (e.g. `oversized_request_body_returns_502`).
pub const MAX_REQ_BODY: usize = 32 * 1024 * 1024;

/// Max upstream response body we will buffer. Content-Length is advisory and
/// can be absent or lie; we accumulate with a cap to prevent OOM / DoS.
/// Exposed `pub` so integration tests can reference it.
pub const MAX_RESP_BODY: usize = 64 * 1024 * 1024;

/// Default connect timeout for the upstream reqwest client.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Default total (read) timeout for the upstream reqwest client.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Idle window for the GET /mcp SSE stream: if no chunk arrives within this
/// duration, the stream is terminated rather than hanging forever.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct AppState {
    client: Client,
    upstream: String,
    /// When set, raw upstream tool-result bodies are dumped here as fixtures.
    capture_dir: Option<PathBuf>,
    /// Raw-payload recovery (tee).
    tee_mode: TeeMode,
    tee_dir: Option<PathBuf>,
    /// Compression level and the trusted filter set applied at aggressive/ultra.
    level: Level,
    filters: Arc<FilterSet>,
    /// Opt-in delta cache (None = disabled).
    delta_cache: Option<Arc<DeltaCache>>,
    /// Bare tool names whose responses are forwarded without compression.
    /// Matched using [`crate::filter::matches_tool`] so namespaced wire names
    /// (`mcp__plugin_figma_figma__get_metadata`) are correctly excluded by a
    /// bare entry (`get_metadata`).
    exclude_tools: Vec<String>,
}

impl AppState {
    /// Set the compression level and filter set (used by `serve` after loading
    /// config + trusted filters; defaults are Standard / empty).
    pub fn set_filters(&mut self, level: Level, filters: FilterSet) {
        self.level = level;
        self.filters = Arc::new(filters);
    }

    /// Enable the opt-in delta cache.
    pub fn set_delta_cache(&mut self, cache: Option<Arc<DeltaCache>>) {
        self.delta_cache = cache;
    }

    /// Set the list of bare tool names to skip compression for (see `exclude_tools`
    /// in the config). Used by `serve` and by integration tests.
    pub fn set_exclude_tools(&mut self, tools: Vec<String>) {
        self.exclude_tools = tools;
    }
}

/// Build proxy state. `capture_dir`, when set, enables fixture capture of
/// target tool responses (raw body only — never headers/token). Tee defaults
/// to off, level defaults to Standard, no filters/delta-cache/exclude-tools.
/// Use [`build_state_with`] to configure all options in one call.
pub fn build_state(upstream: &str, capture_dir: Option<PathBuf>) -> anyhow::Result<AppState> {
    build_state_with(
        upstream,
        capture_dir,
        TeeMode::Never,
        None,
        Level::Standard,
        FilterSet::default(),
        None,
        Vec::new(),
    )
}

/// Build proxy state with full configuration in a single call, eliminating the
/// implicit call-ordering contract of the former post-construction mutators
/// (`set_filters`, `set_delta_cache`, `set_exclude_tools`).
///
/// * `tee_mode` / `tee_dir` — raw-payload recovery.
/// * `level` / `filters` — compression level and trusted TOML filter set.
/// * `delta_cache` — opt-in delta cache (pass `None` to disable).
/// * `exclude_tools` — bare tool names to skip compression for.
///
/// Uses the default connect and read timeouts.
// This is a documented config constructor; the 8-arg count is intentional.
#[allow(clippy::too_many_arguments)]
pub fn build_state_with(
    upstream: &str,
    capture_dir: Option<PathBuf>,
    tee_mode: TeeMode,
    tee_dir: Option<PathBuf>,
    level: Level,
    filters: FilterSet,
    delta_cache: Option<Arc<DeltaCache>>,
    exclude_tools: Vec<String>,
) -> anyhow::Result<AppState> {
    let mut state = build_state_with_timeouts(
        upstream,
        capture_dir,
        tee_mode,
        tee_dir,
        DEFAULT_CONNECT_TIMEOUT,
        DEFAULT_READ_TIMEOUT,
    )?;
    state.level = level;
    state.filters = Arc::new(filters);
    state.delta_cache = delta_cache;
    state.exclude_tools = exclude_tools;
    Ok(state)
}

/// As [`build_state_with`], but with explicit connect and read timeouts.
/// Exposed so integration tests can lower the timeouts to verify timeout
/// behaviour without actually waiting for the production defaults.
pub fn build_state_with_timeouts(
    upstream: &str,
    capture_dir: Option<PathBuf>,
    tee_mode: TeeMode,
    tee_dir: Option<PathBuf>,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> anyhow::Result<AppState> {
    Ok(AppState {
        client: Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(read_timeout)
            .build()?,
        upstream: upstream.trim_end_matches('/').to_string(),
        capture_dir,
        tee_mode,
        tee_dir,
        level: Level::Standard,
        filters: Arc::new(FilterSet::default()),
        delta_cache: None,
        exclude_tools: Vec::new(),
    })
}

/// The proxy router. Exposed so integration tests can drive it directly.
pub fn app(state: AppState) -> Router {
    Router::new().fallback(any(handle)).with_state(state)
}

pub async fn serve(
    host: &str,
    port: u16,
    upstream: &str,
    capture_dir: Option<PathBuf>,
    level_override: Option<Level>,
) -> anyhow::Result<()> {
    let cfg = crate::config::load();
    let tee_dir = cfg.tee.dir.clone().or_else(default_tee_dir);
    if cfg.tee.mode != TeeMode::Never && tee_dir.is_none() {
        tracing::warn!("tee enabled but no tee_dir resolved; raw payloads will not be saved");
    }
    let level = level_override.unwrap_or(cfg.level);
    if !matches!(level, Level::Standard) && cfg.tee.mode == TeeMode::Never {
        tracing::warn!(
            "level={level:?} can drop data via filters but tee is off; \
             set [tee] mode=\"always\" to keep raw payloads recoverable"
        );
    }
    let filters = load_filters();
    let filter_count = filters.len();

    // Construct the delta cache (when enabled) and prime it from disk so that
    // byte-identical re-reads can collapse to a sentinel even on the first call
    // after a restart. The cache Arc is cloned into the shutdown future so we
    // can flush it on graceful shutdown.
    let delta_cache: Option<Arc<DeltaCache>> = if cfg.cache.delta {
        let cache = Arc::new(DeltaCache::new(256));
        if let Some(path) = delta_cache_path() {
            cache.prime_from(&path);
            tracing::debug!("delta-cache primed from {}", path.display());
        }
        Some(cache)
    } else {
        None
    };

    let state = build_state_with(
        upstream,
        capture_dir,
        cfg.tee.mode,
        tee_dir,
        level,
        filters,
        delta_cache.clone(),
        cfg.exclude_tools.clone(),
    )?;
    let app = app(state);

    tracing::info!(
        "compression level={level:?}, {filter_count} filter(s), delta-cache={}",
        cfg.cache.delta
    );
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("frtk listening on http://{addr}  ->  {upstream}");
    tracing::info!("run `frtk gain` to see token savings");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(delta_cache))
        .await?;
    Ok(())
}

async fn shutdown_signal(delta_cache: Option<Arc<DeltaCache>>) {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
    // Flush the delta cache to disk (best-effort — never panic or crash the
    // process on failure; a missing flush just means a cold start next time).
    if let Some(cache) = delta_cache {
        if let Some(path) = delta_cache_path() {
            cache.flush_to(&path);
            tracing::debug!("delta-cache flushed to {}", path.display());
        }
    }
}

async fn handle(State(st): State<AppState>, req: Request) -> Response {
    // The proxy's own origin (as the client dialed it). Needed to rewrite the
    // OAuth discovery documents so the SDK's protected-resource origin check
    // passes. Read before `req` is consumed by `proxy_once`.
    let self_origin = self_origin_from(req.headers());
    match proxy_once(st, req, &self_origin).await {
        Ok(mut resp) => {
            // A 401 from upstream points `resource_metadata` at Figma's own
            // well-known url; repoint it at ours so the client fetches the
            // rewritten PRM below instead.
            rewrite_www_authenticate(resp.headers_mut(), &self_origin);
            resp
        }
        Err(e) => {
            // Never log headers or bodies — they carry the Bearer token.
            let (status, kind) = classify_error(&e);
            tracing::warn!("proxy error [{kind}]: {e}");
            error_response(status, &format!("frtk upstream error [{kind}]: {e}"))
        }
    }
}

async fn proxy_once(st: AppState, req: Request, self_origin: &str) -> anyhow::Result<Response> {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let path = parts.uri.path().to_string();
    let pq = parts
        .uri
        .path_and_query()
        .map(|x| x.as_str())
        .unwrap_or("/")
        .to_string();
    let url = format!("{}{}", st.upstream, pq);

    // SEC-R1-3: handle the over-cap case locally so we can return 413 instead of
    // propagating `?` up to the generic 502 branch.  Only `LengthLimitError`
    // produces a 413; any other body error (client disconnect, HTTP framing)
    // falls through to the generic classify_error → 502 path via `?`.
    let body_bytes = match axum::body::to_bytes(body, MAX_REQ_BODY).await {
        Ok(b) => b,
        Err(e) => {
            // axum_core::Error wraps the real cause as its std::error::Error
            // source.  Check whether that source is a LengthLimitError before
            // returning 413 — any other inner error should propagate as 502.
            use std::error::Error as StdError;
            let is_limit = StdError::source(&e)
                .map(|src| src.is::<http_body_util::LengthLimitError>())
                .unwrap_or(false);
            if is_limit {
                return Ok(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "frtk: request body exceeds the 32 MiB cap",
                ));
            }
            return Err(anyhow::anyhow!(e));
        }
    };

    // Learn which JSON-RPC ids correspond to heavy read tools so we know which
    // responses to compress. Only POSTs carry tools/call requests.
    let mut target_ids = if method == Method::POST {
        crate::mcp::extract_targets(&body_bytes)
    } else {
        std::collections::HashMap::new()
    };

    // Drop any target whose tool name matches an entry in exclude_tools. The
    // comparison uses matches_tool so a bare config entry (e.g. "get_metadata")
    // correctly excludes the namespaced wire name
    // (e.g. "mcp__plugin_figma_figma__get_metadata").
    if !st.exclude_tools.is_empty() {
        target_ids.retain(|_, t| {
            !st.exclude_tools
                .iter()
                .any(|ex| crate::filter::matches_tool(&t.tool, ex))
        });
    }

    let req_headers = filter_req_headers(&parts.headers);
    // t0: start timing immediately before the upstream send.  upstream_ms
    // covers the full upstream time: request send, response headers, and
    // complete response body transfer — not only the header round-trip.
    let t0 = std::time::Instant::now();
    let upstream_resp = st
        .client
        .request(method.clone(), &url)
        .headers(req_headers)
        // PERF-1: Bytes is Clone (O(1) refcount bump); reqwest accepts it via
        // Into<Body>, so no Vec copy is needed here.
        .body(body_bytes.clone())
        .send()
        .await?;

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    // PERF-6: borrow &str from the already-owned HeaderMap; resp_headers
    // outlives all uses of `ctype` in this function.
    let ctype = resp_headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // OAuth Protected Resource Metadata (RFC 9728). Upstream advertises its own
    // url as the `resource`, which agents reject because they dialed the proxy,
    // not Figma. Rewrite `resource` to the proxy's own origin so the
    // SDK's origin check passes; `authorization_servers` is left untouched, so
    // the OAuth dance still runs directly between the agent and api.figma.com
    // and the proxy never sees the token exchange.
    if method == Method::GET && path == "/.well-known/oauth-protected-resource" {
        // SEC-1: cap the PRM body (it's tiny, but the cap applies uniformly).
        let bytes = collect_capped(upstream_resp, MAX_RESP_BODY).await?;
        let out = rewrite_prm_resource(&bytes, self_origin);
        return Ok(buffered(status, &resp_headers, out));
    }

    // GET /mcp is the long-lived server->client SSE notification channel.
    // It carries no tool results, so stream it straight through untouched —
    // but guarded by a per-chunk idle timeout so a stalled upstream doesn't
    // hang the connection forever.
    if method == Method::GET {
        let builder = apply_resp_headers(Response::builder().status(status), &resp_headers);
        let guarded = sse_idle_guarded(upstream_resp.bytes_stream(), SSE_IDLE_TIMEOUT);
        return Ok(builder.body(Body::from_stream(guarded)).unwrap());
    }

    // Everything else (the POST request/response exchange) is buffered. Tool
    // responses close the stream after the result, so buffering is safe and
    // makes transformation straightforward.
    // SEC-1: accumulate with cap; Content-Length is advisory and can lie.
    let bytes = collect_capped(upstream_resp, MAX_RESP_BODY).await?;

    // capture/tee write the WHOLE response body, saved under EACH distinct target
    // tool name. A batch must NOT hide one tool's raw under another tool's file —
    // recovery (and any lossy op like get_screenshot recompression) looks the raw
    // up BY TOOL NAME, so every target in the batch gets its own labelled copy.
    // (The common single-target response writes exactly one file, as before.)
    let mut label_tools: Vec<String> = target_ids.values().map(|t| t.tool.clone()).collect();
    label_tools.sort();
    label_tools.dedup();
    // Representative name for the one-line happy-path log (smallest, deterministic).
    let log_tool = label_tools.first().cloned();

    // Optional fixture capture: dump the raw target response body (no headers,
    // no token) so phase-2 compression can be developed against real payloads.
    // spawn_blocking so the std::fs write does not block the async response path.
    if let Some(dir) = &st.capture_dir {
        if !label_tools.is_empty() {
            let dir = dir.clone();
            let tools = label_tools.clone();
            let bytes_cap = bytes.clone();
            drop(tokio::task::spawn_blocking(move || {
                for tool in &tools {
                    match crate::capture::save_next(&dir, tool, &bytes_cap) {
                        Ok(path) => tracing::info!("captured {tool} fixture -> {}", path.display()),
                        Err(e) => tracing::warn!("capture failed: {e}"),
                    }
                }
            }));
        }
    }

    // Raw-payload recovery (tee): keep the last uncompressed body per tool so
    // aggressive compression remains recoverable.
    // spawn_blocking so the std::fs write does not block the async response path.
    if let Some(dir) = &st.tee_dir {
        if !label_tools.is_empty() {
            let dir = dir.clone();
            let tools = label_tools.clone();
            let mode = st.tee_mode;
            let bytes_tee = bytes.clone();
            drop(tokio::task::spawn_blocking(move || {
                for tool in &tools {
                    crate::tee::maybe_save(mode, &dir, tool, &bytes_tee);
                }
            }));
        }
    }

    if !target_ids.is_empty() {
        if ctype.contains("text/event-stream") {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                let (new_text, mut recs) = crate::mcp::transform_sse(
                    text,
                    &target_ids,
                    st.level,
                    &st.filters,
                    st.delta_cache.as_deref(),
                );
                let upstream_ms = t0.elapsed().as_millis() as u64;
                let level_str = format!("{:?}", st.level);
                for r in &mut recs {
                    r.upstream_ms = upstream_ms;
                    r.level = level_str.clone();
                }
                if !recs.is_empty() {
                    tracing::info!(
                        tool = ?log_tool,
                        %path,
                        status = status.as_u16(),
                        upstream_ms,
                        "proxied"
                    );
                    // spawn_blocking so the ledger write does not block the response
                    // path; guarded so an empty batch never schedules a no-op task.
                    drop(tokio::task::spawn_blocking(move || {
                        crate::stats::record_all(recs)
                    }));
                }
                return Ok(buffered(status, &resp_headers, new_text.into_bytes()));
            }
        } else if ctype.contains("application/json") {
            if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                // Invariant: transform_value emits a StatRec whenever it mutates a
                // result, so empty `recs` ⇒ `v` is unchanged ⇒ forwarding the
                // original bytes is correct.
                let mut recs = crate::mcp::transform_value(
                    &mut v,
                    &target_ids,
                    st.level,
                    &st.filters,
                    st.delta_cache.as_deref(),
                );
                if !recs.is_empty() {
                    let upstream_ms = t0.elapsed().as_millis() as u64;
                    let level_str = format!("{:?}", st.level);
                    for r in &mut recs {
                        r.upstream_ms = upstream_ms;
                        r.level = level_str.clone();
                    }
                    tracing::info!(
                        tool = ?log_tool,
                        %path,
                        status = status.as_u16(),
                        upstream_ms,
                        "proxied"
                    );
                    // spawn_blocking so the ledger write does not block the response path.
                    drop(tokio::task::spawn_blocking(move || {
                        crate::stats::record_all(recs)
                    }));
                    let out = serde_json::to_vec(&v).unwrap_or_else(|_| bytes.to_vec());
                    return Ok(buffered(status, &resp_headers, out));
                }
            }
        }
    }

    // PERF-1: forward the upstream Bytes directly — Body::from(Bytes) is a
    // zero-copy O(1) refcount bump; no Vec allocation on the pass-through path.
    Ok(buffered_bytes(status, &resp_headers, bytes))
}

fn filter_req_headers(h: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in h.iter() {
        if REQ_HOP_BY_HOP.contains(&k.as_str()) {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    out
}

fn apply_resp_headers(mut b: ResponseBuilder, h: &HeaderMap) -> ResponseBuilder {
    for (k, v) in h.iter() {
        if RESP_HOP_BY_HOP.contains(&k.as_str()) {
            continue;
        }
        b = b.header(k.as_str(), v.clone());
    }
    b
}

fn buffered(status: StatusCode, h: &HeaderMap, body: Vec<u8>) -> Response {
    apply_resp_headers(Response::builder().status(status), h)
        .body(Body::from(body))
        .unwrap()
}

/// Like [`buffered`] but accepts a `Bytes` value directly (PERF-1: O(1)
/// refcount bump instead of a Vec allocation on the pass-through path).
fn buffered_bytes(status: StatusCode, h: &HeaderMap, body: Bytes) -> Response {
    apply_resp_headers(Response::builder().status(status), h)
        .body(Body::from(body))
        .unwrap()
}

/// Accumulate a reqwest response body with a hard cap on total bytes.
///
/// SEC-1: `Content-Length` is advisory — an upstream can omit it, lie about
/// it, or send a large body without one. This function pulls chunks via
/// `chunk()` and returns an error once the accumulated size exceeds `cap`,
/// preventing OOM / DoS.
async fn collect_capped(mut resp: reqwest::Response, cap: usize) -> anyhow::Result<Bytes> {
    let hint = resp.content_length().map_or(0, |n| (n as usize).min(cap));
    let mut buf: Vec<u8> = Vec::with_capacity(hint);

    while let Some(chunk) = resp.chunk().await? {
        if buf.len() + chunk.len() > cap {
            anyhow::bail!("upstream response body exceeded the {cap}-byte cap (possible DoS)");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

/// Load the global filter set, plus the project-local set in `./.figma-rtk/`
/// when the current directory is trusted.
fn load_filters() -> FilterSet {
    let mut set = FilterSet::default();
    if let Some(cfg_dir) = dirs::config_dir() {
        let global = cfg_dir.join("figma-rtk").join("filters.toml");
        match FilterSet::load(&global) {
            Ok(f) => set.extend(f),
            Err(e) => tracing::warn!("ignoring global filters {}: {e}", global.display()),
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let project = cwd.join(".figma-rtk").join("filters.toml");
        if project.is_file() {
            if crate::trust::is_trusted(&cwd) {
                match FilterSet::load(&project) {
                    Ok(f) => set.extend(f),
                    Err(e) => tracing::warn!("ignoring project filters {}: {e}", project.display()),
                }
            } else {
                tracing::warn!(
                    "{} exists but {} is not trusted; run `frtk trust` to enable its filters",
                    project.display(),
                    cwd.display()
                );
            }
        }
    }
    set
}

/// Classify an upstream (or proxy-internal) error into an HTTP status code and
/// a short kind string for log messages. Never reads headers, bodies, or the
/// Bearer token.
///
/// * Timeout errors          → 504 Gateway Timeout
/// * Connection errors       → 502 Bad Gateway (with "connect" reason)
/// * Request body too large  → handled in proxy_once before this is called;
///   the kind "body" is reserved for future use
/// * Everything else         → 502 Bad Gateway
pub(crate) fn classify_error(e: &anyhow::Error) -> (StatusCode, &'static str) {
    // Downcast to reqwest::Error to check the specific failure kind.
    if let Some(re) = e.downcast_ref::<reqwest::Error>() {
        if re.is_timeout() {
            return (StatusCode::GATEWAY_TIMEOUT, "timeout");
        }
        if re.is_connect() {
            return (StatusCode::BAD_GATEWAY, "connect");
        }
    }
    (StatusCode::BAD_GATEWAY, "upstream")
}

/// Wrap a `reqwest` bytes stream with a per-chunk idle timeout.
///
/// If no chunk arrives within `idle` duration, the stream ends cleanly — the
/// caller sees end-of-stream, which `Body::from_stream` treats as the final
/// byte. No panic is possible; the timeout simply closes the stream.
fn sse_idle_guarded(
    stream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    idle: Duration,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    // Box::pin gives us an Unpin handle we can hold as unfold state.
    let pinned: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send>,
    > = Box::pin(stream);
    futures::stream::unfold(Some(pinned), move |state| async move {
        let mut inner = state?; // None → already terminated; yield None.
        match timeout(idle, inner.next()).await {
            Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(inner))),
            Ok(Some(Err(e))) => Some((Err(std::io::Error::other(e)), None)),
            Ok(None) => None, // upstream closed cleanly
            Err(_elapsed) => {
                tracing::debug!("SSE idle timeout; closing stream");
                None // end without an error frame
            }
        }
    })
}

fn default_tee_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("figma-rtk").join("tee"))
}

/// Resolve the delta-cache persistence file path.
/// Override with `FRTK_DELTA_CACHE`; default is
/// `$XDG_DATA_HOME/figma-rtk/delta-cache.jsonl`.
pub fn delta_cache_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FRTK_DELTA_CACHE") {
        return Some(PathBuf::from(p));
    }
    Some(
        dirs::data_dir()?
            .join("figma-rtk")
            .join("delta-cache.jsonl"),
    )
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(msg.to_string()))
        .unwrap()
}

/// The proxy's own base origin, as the client dialed it (from the Host header).
/// The proxy always serves plain HTTP on a loopback port, so the scheme is http.
/// The Host is reflected into the OAuth discovery documents (incl. the
/// `WWW-Authenticate` header), so an unsafe value is rejected — this prevents a
/// crafted Host (quote / CRLF) from injecting into that header — and we fall
/// back to the default bind address.
fn self_origin_from(h: &HeaderMap) -> String {
    let host = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_safe_host(s))
        .unwrap_or("127.0.0.1:7337");
    format!("http://{host}")
}

/// A Host value safe to reflect into a URL and an HTTP header: a non-empty
/// `host[:port]` (or bracketed IPv6) of ASCII alphanumerics and `.-:[]` only.
/// Notably excludes quotes, CR, LF, and whitespace.
fn is_safe_host(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

/// Rewrite the `resource` field of an OAuth Protected Resource Metadata document
/// to the proxy's own url, preserving every other field (notably
/// `authorization_servers`). The resource path is taken from upstream so we
/// mirror whatever MCP path Figma advertises. On any parse failure the body is
/// returned unchanged (discovery then fails loudly rather than silently).
fn rewrite_prm_resource(bytes: &[u8], self_origin: &str) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    let Some(res) = v.get("resource").and_then(|r| r.as_str()) else {
        return bytes.to_vec();
    };
    let path = reqwest::Url::parse(res)
        .ok()
        .map(|u| u.path().to_string())
        .filter(|p| !p.is_empty() && p != "/")
        .unwrap_or_else(|| "/mcp".to_string());
    v["resource"] = serde_json::Value::String(format!("{self_origin}{path}"));
    serde_json::to_vec(&v).unwrap_or_else(|_| bytes.to_vec())
}

/// Point the `resource_metadata` challenge parameter of a `WWW-Authenticate`
/// header at the proxy's own PRM endpoint, so the client fetches the rewritten
/// metadata instead of Figma's. Other parameters (scope, authorization_uri) are
/// left untouched. No-op when the header or parameter is absent.
fn rewrite_www_authenticate(headers: &mut HeaderMap, self_origin: &str) {
    let Some(val) = headers.get(header::WWW_AUTHENTICATE) else {
        return;
    };
    let Ok(val) = val.to_str() else { return };
    let new_url = format!("{self_origin}/.well-known/oauth-protected-resource");
    let Some(rewritten) = replace_quoted_param(val, "resource_metadata", &new_url) else {
        return;
    };
    if let Ok(hv) = rewritten.parse() {
        headers.insert(header::WWW_AUTHENTICATE, hv);
    }
}

/// In an HTTP auth-param string, replace the quoted value of `key="..."`. Only
/// matches at a parameter boundary (start of string, or after `,` / whitespace)
/// so the key embedded inside another param's quoted value is not mistaken for
/// the param itself. Returns None when the key is absent or its value is
/// unterminated.
///
/// Precondition: `new_val` must not contain a double-quote character.
/// The caller derives `new_val` from an `is_safe_host`-screened origin (which
/// already excludes quotes), so this is currently unreachable — the
/// `debug_assert!` and `None` return guard against future callers that relax
/// that invariant.
fn replace_quoted_param(s: &str, key: &str, new_val: &str) -> Option<String> {
    // SEC-3: Precondition — new_val must not contain a double-quote.
    // No current exploit (caller derives new_val from an is_safe_host-screened
    // origin which already excludes quotes); this guard is purely defensive
    // against future callers that relax that invariant.
    debug_assert!(
        !new_val.contains('"'),
        "new_val must not contain a double-quote"
    );
    if new_val.contains('"') {
        return None;
    }
    let needle = format!("{key}=\"");
    let mut from = 0;
    let key_start = loop {
        let abs = from + s[from..].find(&needle)?;
        let at_boundary = abs == 0
            || s[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c == ',' || c.is_whitespace());
        if at_boundary {
            break abs;
        }
        from = abs + needle.len();
    };
    let start = key_start + needle.len();
    let end = s[start..].find('"')? + start;
    let mut out = String::with_capacity(s.len().saturating_sub(end - start) + new_val.len());
    out.push_str(&s[..start]);
    out.push_str(new_val);
    out.push_str(&s[end..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prm_resource_rewritten_to_self_origin_keeping_path() {
        let upstream = br#"{"resource":"https://mcp.figma.com/mcp","authorization_servers":["https://api.figma.com"],"scopes_supported":["mcp:connect"]}"#;
        let out = rewrite_prm_resource(upstream, "http://127.0.0.1:7337");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["resource"], "http://127.0.0.1:7337/mcp");
        assert_eq!(v["authorization_servers"][0], "https://api.figma.com");
        assert_eq!(v["scopes_supported"][0], "mcp:connect");
    }

    #[test]
    fn prm_without_resource_field_unchanged() {
        let upstream = br#"{"authorization_servers":["https://api.figma.com"]}"#;
        let out = rewrite_prm_resource(upstream, "http://127.0.0.1:7337");
        assert_eq!(out, upstream);
    }

    #[test]
    fn prm_invalid_json_unchanged() {
        let out = rewrite_prm_resource(b"not json", "http://127.0.0.1:7337");
        assert_eq!(out, b"not json");
    }

    #[test]
    fn resource_path_defaults_when_upstream_resource_pathless() {
        let upstream = br#"{"resource":"https://mcp.figma.com"}"#;
        let out = rewrite_prm_resource(upstream, "http://127.0.0.1:7337");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["resource"], "http://127.0.0.1:7337/mcp");
    }

    #[test]
    fn replace_quoted_param_swaps_only_target() {
        let s = r#"Bearer resource_metadata="https://up/x",scope="mcp:connect""#;
        let out = replace_quoted_param(s, "resource_metadata", "http://proxy/x").unwrap();
        assert_eq!(
            out,
            r#"Bearer resource_metadata="http://proxy/x",scope="mcp:connect""#
        );
    }

    #[test]
    fn replace_quoted_param_absent_key_is_none() {
        assert!(replace_quoted_param(r#"scope="x""#, "resource_metadata", "y").is_none());
    }

    /// SEC-3 (release builds): a new_val containing a double-quote must be
    /// rejected (returns None) so no unescaped quote can appear in the rewritten
    /// header. Gated to release builds because in debug builds `debug_assert!`
    /// fires before the `None` path is reached — that debug path is covered by
    /// the `#[should_panic]` companion below, so both build modes are tested.
    #[test]
    #[cfg(not(debug_assertions))]
    fn replace_quoted_param_rejects_new_val_with_quote() {
        let s = r#"Bearer resource_metadata="https://up/x",scope="mcp:connect""#;
        // A new_val with an embedded quote must be rejected entirely.
        let result = replace_quoted_param(s, "resource_metadata", r#"evil" injected="bad"#);
        assert!(
            result.is_none(),
            "new_val with a quote must return None, not produce an injected header"
        );
    }

    /// SEC-3 (debug builds): the precondition `debug_assert!` fires on a new_val
    /// containing a quote, catching a future caller's programmer error loudly in
    /// dev builds. The release-build `None`-return path is covered above.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "new_val must not contain a double-quote")]
    fn replace_quoted_param_quote_panics_in_debug() {
        let s = r#"Bearer resource_metadata="https://up/x",scope="mcp:connect""#;
        let _ = replace_quoted_param(s, "resource_metadata", r#"evil" injected="bad"#);
    }

    #[test]
    fn replace_quoted_param_skips_non_boundary_match() {
        // The first occurrence is glued to another token (`foo_resource_metadata`)
        // so it is NOT a real param; the boundary-aligned one after the comma is.
        let s = r#"Bearer foo_resource_metadata="bad",resource_metadata="good""#;
        let out = replace_quoted_param(s, "resource_metadata", "NEW").unwrap();
        assert_eq!(
            out,
            r#"Bearer foo_resource_metadata="bad",resource_metadata="NEW""#
        );
    }

    #[test]
    fn self_origin_falls_back_without_host() {
        let h = HeaderMap::new();
        assert_eq!(self_origin_from(&h), "http://127.0.0.1:7337");
    }

    #[test]
    fn self_origin_rejects_unsafe_host() {
        // A Host carrying a quote (header-injection attempt) is rejected.
        let mut h = HeaderMap::new();
        h.insert(header::HOST, r#"x" injected="y"#.parse().unwrap());
        assert_eq!(self_origin_from(&h), "http://127.0.0.1:7337");
    }

    #[test]
    fn is_safe_host_accepts_normal_authority() {
        assert!(is_safe_host("127.0.0.1:7337"));
        assert!(is_safe_host("localhost:7337"));
        assert!(is_safe_host("[::1]:7337"));
        assert!(!is_safe_host("bad\r\nhost"));
        assert!(!is_safe_host("ho st"));
        assert!(!is_safe_host(""));
    }

    // -----------------------------------------------------------------------
    // SEC-R1-3: error classification
    // -----------------------------------------------------------------------

    /// A non-reqwest error (e.g. a plain anyhow string error) must fall through
    /// to the default 502 / "upstream" classification.
    #[test]
    fn classify_error_fallback_is_502_upstream() {
        let e = anyhow::anyhow!("some generic error");
        let (status, kind) = classify_error(&e);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(kind, "upstream");
    }

    // -----------------------------------------------------------------------
    // SEC-R1-3: SSE idle guard unit test
    // -----------------------------------------------------------------------

    /// `sse_idle_guarded` with a very short idle window must terminate the
    /// stream when no chunk arrives within the window, returning no items
    /// beyond those already yielded.
    #[tokio::test]
    async fn sse_idle_guard_terminates_on_idle() {
        use futures::StreamExt;
        use std::time::Duration;

        // A stream that yields one chunk immediately, then hangs forever.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, reqwest::Error>>();
        tx.send(Ok(Bytes::from_static(b"hello"))).unwrap();
        // Do NOT send a second chunk — the stream should idle-timeout.

        let base_stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });

        // Very short idle timeout for the test.
        let idle = Duration::from_millis(50);
        let mut guarded = Box::pin(sse_idle_guarded(base_stream, idle));

        // First chunk arrives immediately.
        let first = guarded.next().await;
        assert!(
            matches!(first, Some(Ok(ref b)) if b.as_ref() == b"hello"),
            "first chunk must be forwarded: {first:?}"
        );

        // Second poll should time out and end the stream cleanly.
        let second = guarded.next().await;
        assert!(
            second.is_none(),
            "stream must end after idle timeout, not hang; got: {second:?}"
        );
    }
}
