//! Integration tests for rate limiting + abuse protection (issue #35).
//!
//! Drives the real axum router via `tower::ServiceExt::oneshot` — which
//! never carries socket connect-info, so every read in a test shares the
//! documented `"local"` fallback key (see the `rate_limit` module docs):
//! per-test router, per-test state, no cross-test leakage.

use aiter_core::amount::{Amount, Currency};
use aiter_core::signing::{AgentIdentity, AgentKeypair};
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use aiter_server::auth::{AGENT_ID_HEADER, SIGNATURE_HEADER};
use aiter_server::catalog::{seed_catalog, AppState};

/// A freshly generated agent keypair + identity under the given id.
fn new_agent(id: &str) -> (AgentKeypair, AgentIdentity) {
    let keypair = AgentKeypair::generate();
    let identity = keypair.identity(id);
    (keypair, identity)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a signed JSON request (same shape as the trust tests).
fn signed_request(
    method: Method,
    uri: &str,
    body: Value,
    keypair: &AgentKeypair,
    agent_id: &str,
) -> Request<Body> {
    let body_str = body.to_string();
    let signature =
        keypair.sign_request(agent_id, method.as_str(), uri, body_str.as_bytes(), now());
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header(AGENT_ID_HEADER, agent_id)
        .header(SIGNATURE_HEADER, serde_json::to_string(&signature).unwrap())
        .body(Body::from(body_str))
        .unwrap()
}

/// Send a request and return (status, response headers, JSON body or null).
async fn send_headers(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, json)
}

/// Assert the documented 429 shape: status, `Retry-After: 1`, JSON error.
fn assert_rate_limited(result: (StatusCode, HeaderMap, Value)) {
    let (status, headers, body) = result;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], "rate limit exceeded");
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("1"),
        "429 must carry Retry-After: 1"
    );
}

// --- Write tier: per-verified-agent quotas ----------------------------------

#[tokio::test]
async fn write_burst_over_limit_returns_429() {
    // Tiny quota: 2 writes per agent per window.
    let state = AppState::with_rate_limits(seed_catalog(), 2, 100);
    let (keypair, identity) = new_agent("agent-burst");
    state
        .register_agent(identity.clone(), Amount::new(1_000_000, Currency::USD))
        .await;
    let app = aiter_server::router(state);

    let request = || {
        signed_request(
            Method::POST,
            "/carts",
            json!({ "currency": "USD", "items": [] }),
            &keypair,
            &identity.id,
        )
    };

    let (status, _, _) = send_headers(&app, request()).await;
    assert_eq!(status, StatusCode::OK, "first write within quota");
    let (status, _, _) = send_headers(&app, request()).await;
    assert_eq!(status, StatusCode::OK, "second write within quota");
    assert_rate_limited(send_headers(&app, request()).await);
    // Still throttled on a follow-up, not just a one-shot blip.
    assert_rate_limited(send_headers(&app, request()).await);
}

#[tokio::test]
async fn write_quotas_are_independent_per_agent() {
    let state = AppState::with_rate_limits(seed_catalog(), 2, 100);
    let (keypair_a, identity_a) = new_agent("agent-a");
    let (keypair_b, identity_b) = new_agent("agent-b");
    for identity in [&identity_a, &identity_b] {
        state
            .register_agent(identity.clone(), Amount::new(1_000_000, Currency::USD))
            .await;
    }
    let app = aiter_server::router(state);

    let write = |keypair: &AgentKeypair, id: &str| {
        signed_request(
            Method::POST,
            "/carts",
            json!({ "currency": "USD", "items": [] }),
            keypair,
            id,
        )
    };

    // agent-a exhausts its own quota; the third write is throttled.
    for _ in 0..2 {
        let (status, _, _) = send_headers(&app, write(&keypair_a, &identity_a.id)).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_rate_limited(send_headers(&app, write(&keypair_a, &identity_a.id)).await);

    // agent-b's quota is untouched: two writes still pass.
    let (status, _, _) = send_headers(&app, write(&keypair_b, &identity_b.id)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send_headers(&app, write(&keypair_b, &identity_b.id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_rate_limited(send_headers(&app, write(&keypair_b, &identity_b.id)).await);
}

// --- Read tier: per-client read quotas ---------------------------------------

#[tokio::test]
async fn public_reads_pass_below_limit_and_429_over() {
    // Tiny quota: 2 reads per client per window (oneshot tests all share the
    // documented "local" key, so this is one client's quota).
    let state = AppState::with_rate_limits(seed_catalog(), 100, 2);
    let app = aiter_server::router(state);

    let read = || {
        Request::builder()
            .uri("/catalog/products")
            .body(Body::empty())
            .unwrap()
    };

    let (status, _, _) = send_headers(&app, read()).await;
    assert_eq!(status, StatusCode::OK, "first read within quota");
    let (status, _, _) = send_headers(&app, read()).await;
    assert_eq!(status, StatusCode::OK, "second read within quota");
    assert_rate_limited(send_headers(&app, read()).await);
}

#[tokio::test]
async fn public_reads_and_writes_have_separate_quotas() {
    // Writes and reads never share a bucket: with a tight read quota, signed
    // writes still pass, and vice versa.
    let state = AppState::with_rate_limits(seed_catalog(), 2, 2);
    let (keypair, identity) = new_agent("agent-mix");
    state
        .register_agent(identity.clone(), Amount::new(1_000_000, Currency::USD))
        .await;
    let app = aiter_server::router(state);

    // Exhaust the read quota with public reads.
    for _ in 0..2 {
        let (status, _, _) = send_headers(
            &app,
            Request::builder()
                .uri("/catalog/products")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_rate_limited(
        send_headers(
            &app,
            Request::builder()
                .uri("/catalog/products")
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    );

    // The write bucket was untouched: a signed write still succeeds, and it
    // does not consume read quota (it is a write).
    let (status, _, _) = send_headers(
        &app,
        signed_request(
            Method::POST,
            "/carts",
            json!({ "currency": "USD", "items": [] }),
            &keypair,
            &identity.id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "write quota independent of reads");
}
