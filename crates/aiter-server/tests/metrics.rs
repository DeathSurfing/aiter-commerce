//! Integration tests for the tracing/metrics surface (issue #33).
//!
//! Exercises the real axum router end-to-end via `tower::ServiceExt::oneshot`:
//! the per-request span middleware bumps `requests_total`/`requests_4xx`/
//! `requests_5xx`, the checkout completion path bumps `checkouts_completed`,
//! and `GET /metrics` serves all of them as plain text without any signature.

use std::collections::HashMap;

use aiter_core::signing::AgentKeypair;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use aiter_server::auth::{AGENT_ID_HEADER, SIGNATURE_HEADER};
use aiter_server::catalog::{demo_agent, seed_catalog, AppState};

/// A router over fresh state. `AppState::new` pre-registers the well-known
/// demo agent, so all signed requests below can use `demo_agent()`'s keypair.
fn app() -> Router {
    aiter_server::router(AppState::new(seed_catalog()))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a request signed by the given agent (same shape the existing trust
/// tests use).
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

/// Send a request through the router and return (status, body).
async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// GET a path and return (status, body).
async fn get(app: &Router, path: &str) -> (StatusCode, String) {
    send(
        app,
        Request::builder().uri(path).body(Body::empty()).unwrap(),
    )
    .await
}

/// Read `/metrics` (no signature — the endpoint is public), assert it is
/// plain text, and parse the `name value` lines into a map.
async fn counters(app: &Router) -> HashMap<String, u64> {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/metrics must be public (no signature needed)"
    );
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("text/plain"),
        "/metrics must be plain text, got {content_type:?}"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    body.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_string(), value.parse().ok()?))
        })
        .collect()
}

/// Create a cart, snapshot it into a checkout session, complete it (all
/// signed, as the trust tests do). Returns (status, order id, session id).
async fn complete_checkout_flow(
    app: &Router,
    keypair: &AgentKeypair,
    agent_id: &str,
) -> (StatusCode, String, String) {
    let (status, cart) = send(
        app,
        signed_request(
            Method::POST,
            "/carts",
            json!({ "currency": "USD", "items": [{"product_id": "p-latte", "quantity": 1}] }),
            keypair,
            agent_id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cart_id: String = serde_json::from_str::<Value>(&cart).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, session) = send(
        app,
        signed_request(
            Method::POST,
            "/checkout_sessions",
            json!({ "cart_id": cart_id }),
            keypair,
            agent_id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cs_id: String = serde_json::from_str::<Value>(&session).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, order) = send(
        app,
        signed_request(
            Method::POST,
            &format!("/checkout_sessions/{cs_id}/complete"),
            json!(null),
            keypair,
            agent_id,
        ),
    )
    .await;
    let order_id: String = serde_json::from_str::<Value>(&order)
        .ok()
        .and_then(|v| v["id"].as_str().map(str::to_string))
        .unwrap_or_default();
    (status, order_id, cs_id)
}

// --- (a) requests_total ------------------------------------------------------

#[tokio::test]
async fn catalog_hits_increment_requests_total() {
    let app = app();
    let before = counters(&app).await;

    let (status, _) = get(&app, "/catalog/products").await;
    assert_eq!(status, StatusCode::OK);

    let after = counters(&app).await;
    // +1 for the catalog hit, +1 for the /metrics read itself.
    assert_eq!(
        after["aiter_requests_total"] - before["aiter_requests_total"],
        2
    );
}

// --- (b) 4xx / 5xx -----------------------------------------------------------

#[tokio::test]
async fn client_and_server_errors_increment_4xx_and_5xx() {
    let app = app();
    let before = counters(&app).await;

    // 404: unknown product.
    let (status, _) = get(&app, "/catalog/products/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // 401: unsigned write to a protected route.
    let (status, _) = send(
        &app,
        Request::builder()
            .method(Method::POST)
            .uri("/carts")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"currency":"USD","items":[]}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 5xx: mint a payment link for a real order with no Razorpay env -> 503.
    let (keypair, identity) = demo_agent();
    let (status, order_id, _) = complete_checkout_flow(&app, &keypair, &identity.id).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &app,
        signed_request(
            Method::POST,
            &format!("/orders/{order_id}/payment_link"),
            json!({}),
            &keypair,
            &identity.id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let after = counters(&app).await;
    assert_eq!(
        after["aiter_requests_4xx"] - before["aiter_requests_4xx"],
        2
    );
    assert_eq!(
        after["aiter_requests_5xx"] - before["aiter_requests_5xx"],
        1
    );
    // Everything since `before`: two 4xx, one 5xx, plus four 2xx (cart,
    // session, complete, and the /metrics read itself).
    assert_eq!(
        after["aiter_requests_total"] - before["aiter_requests_total"],
        2 + 1 + 4
    );
}

// --- (c) checkouts_completed -------------------------------------------------

#[tokio::test]
async fn completed_checkout_increments_checkouts_completed_but_rejected_401_does_not() {
    let app = app();
    let (keypair, identity) = demo_agent();

    // A rejected (unsigned) completion never reaches the handler.
    let before = counters(&app).await;
    let (status, _) = send(
        &app,
        Request::builder()
            .method(Method::POST)
            .uri("/checkout_sessions/cs-ghost/complete")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A signed, completed checkout increments the counter exactly once.
    let (status, _, cs_id) = complete_checkout_flow(&app, &keypair, &identity.id).await;
    assert_eq!(status, StatusCode::OK);

    let after = counters(&app).await;
    assert_eq!(
        after["aiter_checkouts_completed"] - before["aiter_checkouts_completed"],
        1,
        "only the completed checkout counts, not the rejected 401"
    );

    // Idempotent re-completion returns the existing order without re-counting.
    let (status, _) = send(
        &app,
        signed_request(
            Method::POST,
            &format!("/checkout_sessions/{cs_id}/complete"),
            json!(null),
            &keypair,
            &identity.id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let after_idempotent = counters(&app).await;
    assert_eq!(
        after_idempotent["aiter_checkouts_completed"] - after["aiter_checkouts_completed"],
        0,
        "idempotent re-completion must not double-count"
    );
}

// --- (d) /metrics is public and lists counters -------------------------------

#[tokio::test]
async fn metrics_endpoint_is_public_and_lists_counters() {
    let app = app();
    let counters = counters(&app).await;
    // The in-flight /metrics request increments after the handler responds,
    // so the first read still sees 0; a second read would show 1.
    assert_eq!(counters["aiter_requests_total"], 0);
    assert_eq!(counters["aiter_checkouts_completed"], 0);
    assert_eq!(counters["aiter_webhooks_verified"], 0);
    assert_eq!(counters["aiter_reserve_debits"], 0);
}
