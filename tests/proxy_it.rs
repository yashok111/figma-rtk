//! End-to-end test of the proxy pipeline against a mock upstream — the part we
//! cannot exercise against the live Figma server without OAuth. Proves that a
//! tools/call response for a target tool is compressed, captured, and metered
//! as it flows Claude-Code -> frtk -> upstream and back.
//!
//! ENV_LOCK (a std Mutex) is intentionally held across awaits to serialize the
//! tests that mutate the process-global FRTK_LEDGER; that lint is irrelevant here.
#![allow(clippy::await_holding_lock)]

use axum::body::Body;
use axum::http::{header, Method, Request};
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
