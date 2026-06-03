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
    let mut state = proxy::build_state(&upstream, None).unwrap();
    state.set_delta_cache(Some(Arc::new(cache::DeltaCache::new(8))));
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
