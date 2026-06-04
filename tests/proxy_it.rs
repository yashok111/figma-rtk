//! End-to-end test of the proxy pipeline against a mock upstream — the part we
//! cannot exercise against the live Figma server without OAuth. Proves that a
//! tools/call response for a target tool is compressed, captured, and metered
//! as it flows Claude-Code -> frtk -> upstream and back.
//!
//! ENV_LOCK (a std Mutex) is intentionally held across awaits to serialize the
//! tests that mutate the process-global FRTK_LEDGER; that lint is irrelevant here.
#![allow(clippy::await_holding_lock)]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use figma_rtk::{cache, proxy};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

/// Serializes tests that mutate the process-global FRTK_LEDGER env var.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Minified form of the inner payload below (serde sorts object keys).
const EXPECT_MIN: &str = r#"{"children":[{"id":1},{"id":2}],"frame":"Hero"}"#;

fn inner_pretty() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "frame": "Hero",
        "children": [{"id": 1}, {"id": 2}]
    }))
    .unwrap()
}

/// A full JSON-RPC tool response whose text content is the pretty payload.
fn rpc_response_envelope() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "content": [{ "type": "text", "text": inner_pretty() }] }
    })
    .to_string()
}

fn rpc_request() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "get_design_context", "arguments": {} }
    })
    .to_string()
}

async fn spawn_mock(body: String, ctype: &'static str) -> String {
    let router = Router::new().route(
        "/mcp",
        post(move || {
            let body = body.clone();
            async move {
                axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, ctype)
                    .body(Body::from(body))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

async fn post_through_proxy(state: proxy::AppState) -> Vec<u8> {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(rpc_request()))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn pipeline_compresses_captures_and_meters() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("frtk-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let ledger = base.join("ledger.jsonl");
    let capture = base.join("fixtures");
    std::fs::create_dir_all(&base).unwrap();
    std::env::set_var("FRTK_LEDGER", &ledger);

    // --- application/json response, with capture enabled ------------------
    let upstream = spawn_mock(rpc_response_envelope(), "application/json").await;
    let state = proxy::build_state(&upstream, Some(capture.clone())).unwrap();
    let body = post_through_proxy(state).await;

    let v: Value = serde_json::from_slice(&body).unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, EXPECT_MIN, "inner text must be minified");
    assert!(text.len() < inner_pretty().len(), "must be smaller");

    // capture wrote a fixture of the raw (pre-compression) body
    let fixtures: Vec<_> = std::fs::read_dir(&capture)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(fixtures.len(), 1, "one fixture captured");
    let raw = std::fs::read_to_string(fixtures[0].path()).unwrap();
    assert!(raw.contains("\\n"), "captured fixture is the raw pretty payload");

    // --- text/event-stream (SSE) response ---------------------------------
    let sse = format!("event: message\ndata: {}\n\n", rpc_response_envelope());
    let upstream2 = spawn_mock(sse, "text/event-stream").await;
    let state2 = proxy::build_state(&upstream2, None).unwrap();
    let body2 = String::from_utf8(post_through_proxy(state2).await).unwrap();
    assert!(body2.starts_with("event: message"), "SSE framing preserved");
    // Pull the data: payload back out and verify the inner text was minified.
    let data = body2
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .expect("SSE data line present");
    let v2: Value = serde_json::from_str(data).unwrap();
    assert_eq!(
        v2["result"]["content"][0]["text"].as_str().unwrap(),
        EXPECT_MIN,
        "SSE inner payload minified"
    );

    // --- ledger recorded both calls ---------------------------------------
    let recorded = std::fs::read_to_string(&ledger).unwrap();
    let lines: Vec<&str> = recorded.lines().collect();
    assert_eq!(lines.len(), 2, "two compressed calls metered");
    assert!(recorded.contains("get_design_context"));

    std::env::remove_var("FRTK_LEDGER");
    let _ = std::fs::remove_dir_all(&base);
}

/// A mock upstream that mimics Figma's OAuth discovery: it advertises its OWN
/// url as the protected `resource` (PRM) and points `resource_metadata` at its
/// own well-known endpoint in a 401 — exactly what trips the SDK origin check
/// when a proxy sits in front.
async fn spawn_oauth_mock() -> String {
    let router = Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            axum::routing::get(|| async {
                axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"resource":"https://mcp.figma.com/mcp","authorization_servers":["https://api.figma.com"],"scopes_supported":["mcp:connect"]}"#,
                    ))
                    .unwrap()
            }),
        )
        .route(
            "/mcp",
            post(|| async {
                axum::response::Response::builder()
                    .status(401)
                    .header(
                        header::WWW_AUTHENTICATE,
                        r#"Bearer resource_metadata="https://mcp.figma.com/.well-known/oauth-protected-resource",scope="mcp:connect",authorization_uri="https://api.figma.com/.well-known/oauth-authorization-server""#,
                    )
                    .body(Body::empty())
                    .unwrap()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// The proxy must present ITSELF as the OAuth protected resource so Claude
/// Code's SDK (which dialed the proxy, not Figma) accepts the discovery
/// documents. `resource` and the `resource_metadata` pointer are rewritten to
/// the proxy origin; `authorization_servers` / `authorization_uri` / `scope`
/// stay pointed at the real Figma auth server.
#[tokio::test]
async fn oauth_discovery_rewritten_to_proxy_origin() {
    let upstream = spawn_oauth_mock().await;
    let state = proxy::build_state(&upstream, None).unwrap();

    // (1) PRM `resource` -> proxy origin; authorization_servers preserved.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/.well-known/oauth-protected-resource")
        .header(header::HOST, "127.0.0.1:7337")
        .body(Body::empty())
        .unwrap();
    let resp = proxy::app(state.clone()).oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["resource"], "http://127.0.0.1:7337/mcp", "resource rewritten");
    assert_eq!(
        v["authorization_servers"][0], "https://api.figma.com",
        "auth server preserved"
    );

    // (2) 401 `WWW-Authenticate` resource_metadata -> proxy PRM url; scope and
    //     authorization_uri untouched.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1:7337")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(rpc_request()))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 401, "401 status preserved");
    let wa = resp
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .expect("www-authenticate present")
        .to_str()
        .unwrap();
    assert!(
        wa.contains(
            r#"resource_metadata="http://127.0.0.1:7337/.well-known/oauth-protected-resource""#
        ),
        "resource_metadata repointed at proxy: {wa}"
    );
    assert!(wa.contains(r#"scope="mcp:connect""#), "scope preserved: {wa}");
    assert!(
        wa.contains(
            r#"authorization_uri="https://api.figma.com/.well-known/oauth-authorization-server""#
        ),
        "authorization_uri preserved: {wa}"
    );
}

async fn post_clone(app: &Router) -> Vec<u8> {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(rpc_request()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

// ---------------------------------------------------------------------------
// SEC-1: cap buffered upstream response bodies (MAX_RESP_BODY)
// ---------------------------------------------------------------------------

/// Build a mock upstream that returns a response body of `size` bytes.
async fn spawn_mock_sized(size: usize, ctype: &'static str) -> String {
    let body_bytes = vec![b'x'; size];
    let router = Router::new().route(
        "/mcp",
        post(move || {
            let body_bytes = body_bytes.clone();
            async move {
                axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, ctype)
                    .body(Body::from(body_bytes))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// Build a raw-TCP mock that sends `actual_size` bytes with no Content-Length
/// header. Uses `Connection: close` so reqwest reads until EOF and returns Ok
/// without a CL-mismatch error. The `_declared_size` parameter is kept for
/// call-site clarity but is not used in the response — the OOM-guard concern
/// (no pre-allocation of a huge declared CL) is a unit concern of `collect_capped`.
async fn spawn_lying_content_length(_declared_size: usize, actual_size: usize) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body_bytes = vec![b'x'; actual_size];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            // Drain the incoming request.
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await;
            // HTTP/1.1 with Connection: close and NO Content-Length.
            // reqwest reads until EOF and returns the bytes without error.
            let response =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(&body_bytes).await;
            let _ = stream.flush().await;
            // Drop stream → EOF → body ends cleanly.
        }
    });
    format!("http://{addr}")
}

/// A response body exceeding MAX_RESP_BODY must produce 502, not OOM.
#[tokio::test]
async fn oversized_response_body_returns_502() {
    // Use one byte more than MAX_RESP_BODY.
    let over_limit = proxy::MAX_RESP_BODY + 1;
    let upstream = spawn_mock_sized(over_limit, "application/json").await;
    let state = proxy::build_state(&upstream, None).unwrap();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(rpc_request()))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "oversized response body must yield 502"
    );
}

/// An upstream that omits Content-Length but sends more than MAX_RESP_BODY bytes
/// (connection-close termination) must produce a 502, not OOM.
/// This exercises the streaming-accumulate path in `collect_capped` when no CL
/// hint is present and the actual body exceeds the cap.
#[tokio::test]
async fn lying_content_length_returns_502() {
    // Send MAX_RESP_BODY + 1 bytes with no Content-Length header.
    // collect_capped must hit the cap and return an error -> 502.
    let over_limit = proxy::MAX_RESP_BODY + 1;
    let upstream = spawn_lying_content_length(0, over_limit).await;
    let state = proxy::build_state(&upstream, None).unwrap();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(rpc_request()))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "oversized body with no Content-Length must yield 502"
    );
}

/// A small upstream response with no Content-Length header (server closes
/// connection to signal end-of-body) must be forwarded successfully (200 OK).
/// This verifies the body-cap path does not 502 small within-cap bodies and
/// that the proxy handles connection-close termination cleanly.
///
/// The OOM-guard concern (proxy must not pre-allocate based on a huge declared
/// Content-Length) is covered at the unit level in `collect_capped`.
#[tokio::test]
async fn lying_content_length_small_body_forwarded() {
    // Send 100 bytes with no Content-Length header; server closes connection.
    // `collect_capped` must forward the body without 502ing.
    let actual_small: usize = 100;
    let upstream = spawn_lying_content_length(0, actual_small).await;
    let state = proxy::build_state(&upstream, None).unwrap();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(rpc_request()))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "lying CL with small actual body must be forwarded, not rejected"
    );
}

// ---------------------------------------------------------------------------
// SEC-2: connect + read timeouts on the reqwest client
// ---------------------------------------------------------------------------

/// A non-responding upstream (hangs forever on the TCP accept, never reads
/// the request) must produce a 502 from the proxy instead of hanging forever.
/// We simulate this by binding a listener but never calling accept() on it.
#[tokio::test]
async fn hung_upstream_returns_502() {
    // Bind a port and immediately drop the listener — the TCP SYN will be
    // refused (connection refused), which maps to a connect error, not a hang.
    // To simulate a truly hung upstream we bind the listener and keep it alive
    // (accepting the TCP connection) but never respond. We do this by spawning
    // a task that accepts and then sleeps forever.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Accept the connection but never write anything back.
        if let Ok((_stream, _)) = listener.accept().await {
            // Hold the stream alive (and _listener) — the peer sees an open
            // connection that never sends data.
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });

    let upstream = format!("http://{addr}");
    let state = proxy::build_state_with_timeouts(
        &upstream,
        None,
        figma_rtk::config::TeeMode::Never,
        None,
        // Very short timeouts so the test doesn't take long.
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(500),
    )
    .unwrap();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(rpc_request()))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "hung upstream must yield 502"
    );
}

// ---------------------------------------------------------------------------
// SEC-3: defensive quote-precondition on replace_quoted_param
// ---------------------------------------------------------------------------

// The unit test for this lives in proxy.rs inline tests (see SEC-3 impl).

// ---------------------------------------------------------------------------
// PERF-1: non-target pass-through returns the original body unchanged
// ---------------------------------------------------------------------------

/// A POST whose JSON-RPC method is NOT a target tool must be forwarded
/// verbatim — the body the proxy returns must equal the body the mock sent.
#[tokio::test]
async fn non_target_passthrough_body_unchanged() {
    // A tools/call for a non-target tool (e.g. use_figma — not in TARGET_TOOLS).
    let non_target_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 42,
        "result": {
            "content": [{"type": "text", "text": "some non-target response"}]
        }
    })
    .to_string();

    let upstream = spawn_mock(non_target_body.clone(), "application/json").await;
    let state = proxy::build_state(&upstream, None).unwrap();

    let non_target_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "tools/call",
        "params": { "name": "use_figma", "arguments": {} }
    })
    .to_string();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(non_target_req))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        body.as_ref(),
        non_target_body.as_bytes(),
        "non-target pass-through must return the original body unchanged"
    );
}

#[tokio::test]
async fn delta_cache_collapses_identical_reread() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("frtk-delta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    std::env::set_var("FRTK_LEDGER", base.join("ledger.jsonl"));

    let upstream = spawn_mock(rpc_response_envelope(), "application/json").await;
    let state = proxy::build_state_with(
        &upstream,
        None,
        figma_rtk::config::TeeMode::Never,
        None,
        figma_rtk::config::Level::Standard,
        figma_rtk::filter::FilterSet::default(),
        Some(Arc::new(cache::DeltaCache::new(8))),
        Vec::new(),
    )
    .unwrap();
    let app = proxy::app(state);

    // First read: full (compressed) content.
    let v1: Value = serde_json::from_slice(&post_clone(&app).await).unwrap();
    assert_eq!(v1["result"]["content"][0]["text"].as_str().unwrap(), EXPECT_MIN);

    // Identical re-read: collapsed to the delta sentinel.
    let v2: Value = serde_json::from_slice(&post_clone(&app).await).unwrap();
    let t2 = v2["result"]["content"][0]["text"].as_str().unwrap();
    assert!(t2.contains("Unchanged"), "second identical read collapsed: {t2}");
    assert!(!t2.contains(EXPECT_MIN));

    std::env::remove_var("FRTK_LEDGER");
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// COV-1: exclude_tools with namespaced wire name
// ---------------------------------------------------------------------------

/// The namespaced wire name `mcp__plugin_figma_figma__get_metadata` must be
/// excluded when `exclude_tools = ["get_metadata"]` is set. The body must pass
/// through UNCHANGED and the ledger must remain empty (no compression happened).
///
/// TDD: This test must fail BEFORE the fix (bare-name exact-match in
/// `Config::is_excluded` never matches the namespaced wire name, so the proxy
/// still compresses it). After the fix it is green.
#[tokio::test]
async fn exclude_tools_namespaced_wire_name_passes_through() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("frtk-excl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let ledger = base.join("ledger.jsonl");
    std::env::set_var("FRTK_LEDGER", &ledger);

    // Build a response envelope that uses the NAMESPACED wire name in the request,
    // and returns pretty-printed content (so compression would shrink it).
    let pretty_payload = inner_pretty();
    let upstream_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "content": [{ "type": "text", "text": pretty_payload }] }
    })
    .to_string();

    let upstream = spawn_mock(upstream_body.clone(), "application/json").await;

    // Build state with exclude_tools=["get_metadata"] so the bare name excludes
    // the namespaced wire call.
    let state = proxy::build_state_with(
        &upstream,
        None,
        figma_rtk::config::TeeMode::Never,
        None,
        figma_rtk::config::Level::Standard,
        figma_rtk::filter::FilterSet::default(),
        None,
        vec!["get_metadata".to_string()],
    )
    .unwrap();

    // Send a request using the NAMESPACED wire name.
    let namespaced_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "mcp__plugin_figma_figma__get_metadata", "arguments": {} }
    })
    .to_string();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(namespaced_req))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();

    // Body must be forwarded UNCHANGED (upstream_body is already compact JSON,
    // but the important check is that the content text is NOT minified).
    let v: Value = serde_json::from_slice(&body_bytes).unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    // The pretty payload was NOT compressed — it must still contain newlines/spaces.
    assert!(
        text.contains('\n'),
        "excluded tool body must pass through unchanged (still pretty-printed), got: {text:?}"
    );
    // And it must equal the original pretty_payload exactly.
    assert_eq!(
        text, pretty_payload,
        "excluded tool body must be byte-identical to what the upstream sent"
    );

    // Ledger must be empty — no StatRec was written.
    let ledger_content = std::fs::read_to_string(&ledger).unwrap_or_default();
    assert!(
        ledger_content.trim().is_empty(),
        "ledger must be empty for excluded tool, got: {ledger_content:?}"
    );

    std::env::remove_var("FRTK_LEDGER");
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// COV-3: GET /mcp SSE stream passes through byte-identical
// ---------------------------------------------------------------------------

/// Spawn a mock upstream that responds to GET /mcp with an SSE stream.
async fn spawn_get_mock(body: String, ctype: &'static str) -> String {
    let router = Router::new().route(
        "/mcp",
        axum::routing::get(move || {
            let body = body.clone();
            async move {
                axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, ctype)
                    .body(Body::from(body))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// A GET /mcp (the long-lived server->client SSE notification channel) must be
/// forwarded byte-for-byte — the proxy must NOT buffer or transform it.
#[tokio::test]
async fn get_mcp_stream_passes_through_verbatim() {
    let sse_body = "event: ping\ndata: {\"type\":\"ping\"}\n\nevent: message\ndata: {\"id\":1}\n\n";
    let upstream = spawn_get_mock(sse_body.to_string(), "text/event-stream").await;
    let state = proxy::build_state(&upstream, None).unwrap();

    let req = Request::builder()
        .method(Method::GET)
        .uri("/mcp")
        .body(Body::empty())
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        body.as_ref(),
        sse_body.as_bytes(),
        "GET /mcp SSE stream must be byte-identical"
    );
}

// ---------------------------------------------------------------------------
// COV-4: Aggressive level applies filters through the proxy
// ---------------------------------------------------------------------------

/// When the proxy state is configured with Aggressive level + a drop_where
/// filter, the dropped content block must be absent in the response.
#[tokio::test]
async fn aggressive_level_applies_filters_through_proxy() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("frtk-agg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    std::env::set_var("FRTK_LEDGER", base.join("ledger.jsonl"));

    // Build a response with a code block + a boilerplate block matching drop_where.
    let upstream_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "content": [
                {"type": "text", "text": "export default function Hero() { return null; }"},
                {"type": "text", "text": "BOILERPLATE: this text should be dropped by the filter"}
            ]
        }
    })
    .to_string();

    let filters = figma_rtk::filter::FilterSet::parse(
        "[[filter]]\nname=\"dc\"\ntools=[\"get_design_context\"]\n\
         drop_where=[{key=\"text\",starts_with=\"BOILERPLATE\"}]\n",
    )
    .unwrap();

    let upstream = spawn_mock(upstream_body, "application/json").await;
    let state = proxy::build_state_with(
        &upstream,
        None,
        figma_rtk::config::TeeMode::Never,
        None,
        figma_rtk::config::Level::Aggressive,
        filters,
        None,
        Vec::new(),
    )
    .unwrap();

    let body = post_through_proxy(state).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let out = serde_json::to_string(&v).unwrap();

    assert!(
        !out.contains("BOILERPLATE"),
        "dropped block must be absent at Aggressive level: {out}"
    );
    assert!(
        out.contains("export default function Hero"),
        "code block must survive: {out}"
    );

    std::env::remove_var("FRTK_LEDGER");
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// COV-5: Tee writes raw body to dir
// ---------------------------------------------------------------------------

/// When TeeMode::Always is set, the raw upstream body must be written to the
/// tee dir under `<tool>.raw`, and its content must equal the raw mock body.
#[tokio::test]
async fn tee_writes_raw_body_to_dir() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("frtk-tee-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let tee_dir = base.join("tee");

    let upstream = spawn_mock(rpc_response_envelope(), "application/json").await;
    let state = proxy::build_state_with(
        &upstream,
        None,
        figma_rtk::config::TeeMode::Always,
        Some(tee_dir.clone()),
        figma_rtk::config::Level::Standard,
        figma_rtk::filter::FilterSet::default(),
        None,
        Vec::new(),
    )
    .unwrap();

    let _ = post_through_proxy(state).await;

    // The tee dir must contain exactly one file named after the tool.
    let entries: Vec<_> = std::fs::read_dir(&tee_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1, "tee dir must contain exactly one file");

    // The file must be byte-identical to the raw upstream body (pre-compression).
    let raw = std::fs::read_to_string(entries[0].path()).unwrap();
    assert_eq!(raw, rpc_response_envelope(), "tee file must be byte-identical to the raw upstream body");

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// COV-7: Batch request compresses target, passes non-target
// ---------------------------------------------------------------------------

/// A JSON-RPC batch containing a target tool + a non-target tool: the target
/// result is compressed (and a ledger entry written), the non-target result is
/// passed through unchanged, and EXACTLY ONE ledger line is written.
#[tokio::test]
async fn batch_request_compresses_target_and_passes_nontarget() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("frtk-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let ledger = base.join("ledger.jsonl");
    std::env::set_var("FRTK_LEDGER", &ledger);

    // Batch response: id=1 is get_design_context (target, pretty inner text),
    // id=2 is whoami (non-target, compact plain text).
    let non_target_text = "non-target plain text";
    let batch_resp = serde_json::json!([
        {
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "content": [{ "type": "text", "text": inner_pretty() }] }
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "content": [{ "type": "text", "text": non_target_text }] }
        }
    ])
    .to_string();

    let upstream = spawn_mock(batch_resp, "application/json").await;
    let state = proxy::build_state(&upstream, None).unwrap();

    // Batch request: id=1 calls get_design_context (target), id=2 calls whoami (non-target).
    let batch_req = serde_json::json!([
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "get_design_context", "arguments": {} }
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "whoami", "arguments": {} }
        }
    ])
    .to_string();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(batch_req))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();

    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("response must be a batch array");
    assert_eq!(arr.len(), 2, "both responses present");

    // Find id=1 (target) and id=2 (non-target) in the response array.
    let target_resp = arr.iter().find(|m| m["id"] == 1).expect("id=1 present");
    let nontarget_resp = arr.iter().find(|m| m["id"] == 2).expect("id=2 present");

    // Target: content text must be minified.
    let target_text = target_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(
        target_text, EXPECT_MIN,
        "target result must be minified"
    );

    // Non-target: content text must be unchanged.
    let nt_text = nontarget_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(
        nt_text, non_target_text,
        "non-target result must be unchanged"
    );

    // Exactly 1 ledger line (only the target tool was compressed).
    let ledger_content = std::fs::read_to_string(&ledger).unwrap_or_default();
    let ledger_lines: Vec<&str> = ledger_content.lines().collect();
    assert_eq!(
        ledger_lines.len(),
        1,
        "EXACTLY 1 ledger line for the single target tool, got: {ledger_content:?}"
    );
    assert!(
        ledger_content.contains("get_design_context"),
        "ledger must name the target tool"
    );

    std::env::remove_var("FRTK_LEDGER");
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// COV-8: Oversized REQUEST body returns 502
// ---------------------------------------------------------------------------

/// A POST whose body exceeds MAX_REQ_BODY (32 MiB) must produce 502 — this is
/// the REQUEST-side cap, distinct from the RESPONSE-side cap tested in
/// `oversized_response_body_returns_502`.
#[tokio::test]
async fn oversized_request_body_returns_502() {
    // Any upstream will do — the proxy must reject the request before sending it.
    let upstream = spawn_mock("{}".to_string(), "application/json").await;
    let state = proxy::build_state(&upstream, None).unwrap();

    // Build a body 1 byte over MAX_REQ_BODY (32 MiB + 1).
    let over_limit = proxy::MAX_REQ_BODY + 1;
    let big_body = vec![b'x'; over_limit];

    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(big_body))
        .unwrap();
    let resp = proxy::app(state).oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "oversized request body must yield 502"
    );
}

// ---------------------------------------------------------------------------
// COV-complement: already-compact content produces no ledger entry
// ---------------------------------------------------------------------------

/// Already-compact content (no whitespace to remove, no filter applied) must
/// NOT produce a ledger entry — this is the proxy-level complement of the
/// mcp::tests::no_stat_rec_for_already_compact_json unit test (CORR-3).
#[tokio::test]
async fn already_compact_content_produces_no_ledger_entry() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("frtk-compact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let ledger = base.join("ledger.jsonl");
    std::env::set_var("FRTK_LEDGER", &ledger);

    // A target-tool response whose inner text is already compact JSON.
    let compact_text = r#"{"a":1,"b":2}"#;
    let upstream_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "content": [{ "type": "text", "text": compact_text }] }
    })
    .to_string();

    let upstream = spawn_mock(upstream_body, "application/json").await;
    let state = proxy::build_state(&upstream, None).unwrap();

    let body = post_through_proxy(state).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, compact_text, "already-compact content must pass through unchanged");

    // No ledger entry must have been written.
    let ledger_content = std::fs::read_to_string(&ledger).unwrap_or_default();
    assert!(
        ledger_content.trim().is_empty(),
        "no ledger entry for already-compact content, got: {ledger_content:?}"
    );

    std::env::remove_var("FRTK_LEDGER");
    let _ = std::fs::remove_dir_all(&base);
}
