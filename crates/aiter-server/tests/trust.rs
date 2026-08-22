//! Integration tests for the trust surface (issues #25, #26, #27).
//!
//! Exercises the real axum router end-to-end via `tower::ServiceExt::oneshot`:
//!
//! * #25 — request verification middleware: signed writes pass, unsigned and
//!   tampered requests are rejected, public discovery/catalog endpoints stay
//!   reachable without a signature.
//! * #26 — per-agent spend limits: under-limit checkouts complete, over-limit
//!   checkouts are rejected with 403, caps are configurable per agent.
//! * #27 — receipts + append-only audit log: a completed checkout emits exactly
//!   one receipt/audit entry with correct who/what/when/amount; idempotent
//!   re-completion does not double-record.

use aiter_core::amount::{Amount, Currency};
use aiter_core::signing::{AgentIdentity, AgentKeypair};
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
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

/// Build a signed JSON request. The signature covers method, target URI,
/// body digest, timestamp and agent id (the origin-form URI here matches what
/// the server reconstructs for a request without scheme/authority).
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

/// Send a request through the router and return (status, JSON body or null).
async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// A router with `agent_id` registered against a USD spend cap of `cap` minor
/// units, plus the keypair/identity needed to sign for it.
async fn app_with_agent(id: &str, cap: i64) -> (Router, AgentKeypair, AgentIdentity) {
    let state = AppState::new(seed_catalog());
    let (keypair, identity) = new_agent(id);
    state
        .register_agent(identity.clone(), Amount::new(cap, Currency::USD))
        .await;
    (aiter_server::router(state), keypair, identity)
}

/// Drive cart -> checkout session -> complete, all signed, returning
/// (complete_status, order_json, checkout_session_id).
async fn checkout_flow(
    app: &Router,
    keypair: &AgentKeypair,
    agent_id: &str,
    items: Value,
) -> (StatusCode, Value, String) {
    let (status, cart) = send(
        app,
        signed_request(
            Method::POST,
            "/carts",
            json!({ "currency": "USD", "items": items }),
            keypair,
            agent_id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cart creation should pass");
    let cart_id = cart["id"].as_str().unwrap().to_string();

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
    assert_eq!(status, StatusCode::OK, "session creation should pass");
    let cs_id = session["id"].as_str().unwrap().to_string();

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
    (status, order, cs_id)
}

// --- Issue #25: request verification middleware ---------------------------------

#[tokio::test]
async fn unsigned_write_is_rejected_401() {
    let (app, _, _) = app_with_agent("agent-1", 1_000_000).await;

    let (status, body) = send(
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
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn signed_write_passes() {
    let (app, keypair, identity) = app_with_agent("agent-1", 1_000_000).await;

    let (status, cart) = send(
        &app,
        signed_request(
            Method::POST,
            "/carts",
            json!({ "currency": "USD", "items": [{"product_id": "p-latte", "quantity": 1}] }),
            &keypair,
            &identity.id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(cart["id"].as_str().unwrap().starts_with("cart-"));
}

#[tokio::test]
async fn unknown_agent_is_403() {
    // The signing key is never registered against any agent in the store.
    let (keypair, identity) = new_agent("ghost");
    let state = AppState::new(seed_catalog());
    let app = aiter_server::router(state);

    let (status, _) = send(
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
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn tampered_body_is_rejected_401() {
    let (app, keypair, identity) = app_with_agent("agent-1", 1_000_000).await;

    // Sign over the honest body, then ship a different one.
    let honest = json!({ "currency": "USD", "items": [{"product_id": "p-latte", "quantity": 1}] });
    let mut honest_req = signed_request(Method::POST, "/carts", honest, &keypair, &identity.id);
    *honest_req.body_mut() = Body::from(
        json!({ "currency": "USD", "items": [{"product_id": "p-latte", "quantity": 99}] })
            .to_string(),
    );

    let (status, _) = send(&app, honest_req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn public_endpoints_are_reachable_without_signature() {
    let (app, keypair, identity) = app_with_agent("agent-1", 1_000_000).await;

    for uri in [
        "/",
        "/agentic/health",
        "/catalog/products",
        "/catalog/products/p-latte",
        "/.well-known/agent-card.json",
        "/llms.txt",
        "/seed/catalog",
    ] {
        let (status, _) = send(
            &app,
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{uri} should stay public");
    }

    // GET cart reads are public too (documented read side of the split): an
    // unsigned read of a cart created by a signed write must succeed.
    let (status, cart) = send(
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
    assert_eq!(status, StatusCode::OK);
    let cart_id = cart["id"].as_str().unwrap().to_string();

    let (status, _) = send(
        &app,
        Request::builder()
            .uri(format!("/carts/{cart_id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unsigned GET /carts/{{id}} stays public"
    );
}

// --- Issue #26: per-agent spend limits -------------------------------------------

#[tokio::test]
async fn under_limit_checkout_completes() {
    let (app, keypair, identity) = app_with_agent("agent-1", 1_000).await;

    let (status, order, _) = checkout_flow(
        &app,
        &keypair,
        &identity.id,
        json!([{ "product_id": "p-latte", "quantity": 2 }]), // 900 minor units
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(order["status"], "Placed");
    assert_eq!(order["totals"]["total"]["units"], 900);
}

#[tokio::test]
async fn over_limit_checkout_is_rejected_403() {
    let (app, keypair, identity) = app_with_agent("agent-1", 150).await;

    let (status, body, _) = checkout_flow(
        &app,
        &keypair,
        &identity.id,
        json!([{ "product_id": "p-latte", "quantity": 2 }]), // 900 > cap 150
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("spend limit exceeded"),
        "expected a clear spend-limit error, got: {error}"
    );
}

#[tokio::test]
async fn spend_caps_are_configurable_per_agent() {
    // Two agents, two different caps: agent-a tolerates a second order that
    // agent-b's tighter cap rejects.
    let (app_a, keypair_a, identity_a) = app_with_agent("agent-a", 1_000).await;
    let (app_b, keypair_b, identity_b) = app_with_agent("agent-b", 300).await;

    let items = json!([{ "product_id": "p-espresso", "quantity": 1 }]); // 300 each

    let (s1, _, _) = checkout_flow(&app_a, &keypair_a, &identity_a.id, items.clone()).await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, _, _) = checkout_flow(&app_a, &keypair_a, &identity_a.id, items.clone()).await;
    assert_eq!(s2, StatusCode::OK, "agent-a: 600 <= 1000 cap");

    let (s3, _, _) = checkout_flow(&app_b, &keypair_b, &identity_b.id, items.clone()).await;
    assert_eq!(s3, StatusCode::OK);
    let (s4, _, _) = checkout_flow(&app_b, &keypair_b, &identity_b.id, items.clone()).await;
    assert_eq!(s4, StatusCode::FORBIDDEN, "agent-b: 600 > 300 cap");
}

// --- Issue #27: receipts + append-only audit log ---------------------------------

#[tokio::test]
async fn completed_checkout_emits_exactly_one_receipt_to_the_audit_log() {
    let state = AppState::new(seed_catalog());
    let (keypair, identity) = new_agent("agent-1");
    state
        .register_agent(identity.clone(), Amount::new(1_000_000, Currency::USD))
        .await;
    let app = aiter_server::router(state.clone());

    let (status, order, cs_id) = checkout_flow(
        &app,
        &keypair,
        &identity.id,
        json!([{ "product_id": "p-latte", "quantity": 2 }]), // total 900
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let order_id = order["id"].as_str().unwrap().to_string();

    // Receipt fields: who / what / when / amount.
    let audit = state.audit_entries().await;
    assert_eq!(
        audit.len(),
        1,
        "one completed checkout -> exactly one entry"
    );
    let entry = &audit[0];
    assert_eq!(entry.seq, 0, "first audit entry has sequence 0");
    assert_eq!(entry.entry.agent_id, identity.id, "who = verified agent");
    assert_eq!(entry.entry.order_id, order_id, "what = order id");
    assert_eq!(entry.entry.amount.units(), 900, "amount = order total");
    assert_eq!(entry.entry.amount.currency(), Currency::USD);
    assert!(entry.entry.issued_at > 0, "when = timestamp");
    assert_eq!(entry.entry.id, format!("rcpt-{order_id}"));

    // Idempotent re-completion of the SAME session: same order, no second
    // audit entry and no double charge.
    let (status, order2) = send(
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
    assert_eq!(order2["id"], order["id"]);
    assert_eq!(
        state.audit_entries().await.len(),
        1,
        "re-complete must not double-record"
    );
}

#[tokio::test]
async fn over_limit_checkout_emits_no_receipt() {
    let state = AppState::new(seed_catalog());
    let (keypair, identity) = new_agent("agent-1");
    state
        .register_agent(identity.clone(), Amount::new(150, Currency::USD))
        .await;
    let app = aiter_server::router(state.clone());

    let (status, _, _) = checkout_flow(
        &app,
        &keypair,
        &identity.id,
        json!([{ "product_id": "p-latte", "quantity": 2 }]), // 900 > cap 150
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        state.audit_entries().await.is_empty(),
        "rejected checkout must not append an audit entry"
    );
}
