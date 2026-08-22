//! Shared test-support utilities (issue #58).
//!
//! One home for the signed-request builders and `tower::ServiceExt::oneshot`
//! senders that used to be copy-pasted across the integration tests
//! (`tests/*.rs`) and the in-crate `#[cfg(test)]` modules. Compiled as part
//! of the lib on purpose: integration tests link the lib **without**
//! `cfg(test)`, so shared helpers cannot live behind `#[cfg(test)]` — the
//! in-crate test modules import this module either way. `tower` and `axum`
//! are already regular dependencies of this crate, so the module adds zero
//! new dependencies.
//!
//! Two demo identities exist by design:
//!
//! * [`demo_agent`] — a freshly generated keypair under [`TEST_AGENT_ID`]
//!   (`agent-1`), registered explicitly by the in-crate test modules on their
//!   own `AppState` (their assertions pin `agent-1` as the acting agent).
//! * [`crate::catalog::demo_agent`] — the **well-known** demo agent
//!   (`agent-demo`, fixed public seed) that `AppState` pre-registers; the
//!   endpoint-level [`signed`] / [`signed_raw`] helpers and
//!   [`demo_signed_request`] sign as this agent, exactly like
//!   `aiter-cli`'s own `build_signed_request`.

use std::sync::OnceLock;

use aiter_core::amount::{Amount, Currency};
use aiter_core::signing::{AgentIdentity, AgentKeypair};
use aiter_core::util::now;
use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::Router;
use serde_json::Value;
use tower::ServiceExt;

use crate::auth::{AGENT_ID_HEADER, SIGNATURE_HEADER};
use crate::catalog::demo_agent as well_known_demo_agent;
use crate::catalog::AppState;
use crate::cli::build_signed_request;

/// Id the shared test agent signs as, and the id the in-crate test modules
/// register on their `AppState`.
pub const TEST_AGENT_ID: &str = "agent-1";
/// Spend cap registered for the shared test agent (plenty for existing flows).
pub const TEST_AGENT_CAP: i64 = 1_000_000;

/// The shared test agent every in-crate `#[cfg(test)]` module signs with:
/// one generated keypair per test process, under [`TEST_AGENT_ID`]. Distinct
/// from the well-known [`crate::catalog::demo_agent`] (see the module docs).
pub fn demo_agent() -> &'static (AgentKeypair, AgentIdentity) {
    static AGENT: OnceLock<(AgentKeypair, AgentIdentity)> = OnceLock::new();
    AGENT.get_or_init(|| {
        let keypair = AgentKeypair::generate();
        let identity = keypair.identity(TEST_AGENT_ID);
        (keypair, identity)
    })
}

/// Register the shared test agent on `st` with [`TEST_AGENT_CAP`] —
/// `auth::require_signed` 403s unregistered agents before any handler runs.
pub async fn register_test_agent(st: &AppState) {
    let (_, identity) = demo_agent();
    st.register_agent(identity.clone(), Amount::new(TEST_AGENT_CAP, Currency::USD))
        .await;
}

/// A freshly generated agent keypair + identity under the given id.
pub fn new_agent(id: &str) -> (AgentKeypair, AgentIdentity) {
    let keypair = AgentKeypair::generate();
    let identity = keypair.identity(id);
    (keypair, identity)
}

/// Build a signed request for an arbitrary keypair / agent id. The signature
/// covers method, target URI, body digest, timestamp and agent id — the same
/// components [`crate::auth::require_signed`] verifies. `uri` is the
/// origin-form path (including any query string), which is what the server
/// reconstructs as the signed target URI.
pub fn signed_request(
    method: Method,
    uri: &str,
    body: Value,
    keypair: &AgentKeypair,
    agent_id: &str,
) -> Request<Body> {
    let body_str = body.to_string();
    let signature = keypair.sign_request(
        agent_id,
        method.as_str(),
        uri,
        body_str.as_bytes(),
        now() as u64,
    );
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header(AGENT_ID_HEADER, agent_id)
        .header(SIGNATURE_HEADER, serde_json::to_string(&signature).unwrap())
        .body(Body::from(body_str))
        .unwrap()
}

/// Build a `POST` request signed by the **well-known** demo agent, delegating
/// the signing to the CLI's own builder ([`crate::cli::build_signed_request`])
/// so tests exercise the exact production request-building path `aiter-cli`
/// uses.
pub fn demo_signed_request(uri: &str, body: Value) -> Request<Body> {
    let signed = build_signed_request(uri, body);
    Request::builder()
        .method(Method::POST)
        .uri(signed.uri.as_str())
        .header("content-type", "application/json")
        .header(AGENT_ID_HEADER, &signed.agent_id_header)
        .header(SIGNATURE_HEADER, &signed.signature_header)
        .body(Body::from(signed.body))
        .unwrap()
}

/// Send a request through the router and return (status, JSON body or null).
pub async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Send a request and return (status, raw response text) — for assertions
/// that must inspect the exact bytes (panic traces, secret leaks).
pub async fn send_text(app: &Router, req: Request<Body>) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Send a request and return (status, response headers, JSON body or null).
pub async fn send_headers(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, json)
}

/// Drive a write route with a request signed by the shared test agent
/// ([`demo_agent`]), returning (status, JSON body or null). `body: None`
/// signs and sends an empty body — the no-payload-write convention the
/// checkout/reserve/payments tests use.
pub async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (keypair, identity) = demo_agent();
    let body_str = body.map(|b| b.to_string()).unwrap_or_default();
    let signature = keypair.sign_request(
        &identity.id,
        method.as_str(),
        uri,
        body_str.as_bytes(),
        now() as u64,
    );

    let mut builder = Request::builder().method(method).uri(uri);
    if !body_str.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder
        .header(AGENT_ID_HEADER, &identity.id)
        .header(SIGNATURE_HEADER, serde_json::to_string(&signature).unwrap())
        .body(Body::from(body_str))
        .unwrap();
    send(app, req).await
}

/// Drive the router with an **unsigned** request plus extra headers — for
/// public routes and provider-authenticated webhooks (e.g. Razorpay's
/// `x-razorpay-signature`).
pub async fn call_unsigned(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    send(app, req).await
}

/// Drive a write route with a request signed by the **well-known** demo agent
/// (pre-registered by `AppState::default()`), returning (status, raw response
/// text). `body: None` signs and sends an empty body.
pub async fn signed(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let (keypair, identity) = well_known_demo_agent();
    let body_str = body.map(|b| b.to_string()).unwrap_or_default();
    let signature = keypair.sign_request(
        &identity.id,
        method.as_str(),
        uri,
        body_str.as_bytes(),
        now() as u64,
    );
    let mut builder = Request::builder().method(method).uri(uri);
    if !body_str.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder
        .header(AGENT_ID_HEADER, &identity.id)
        .header(SIGNATURE_HEADER, serde_json::to_string(&signature).unwrap())
        .body(Body::from(body_str))
        .unwrap();
    send_text(app, req).await
}

/// Drive a write route with a request signed by the **well-known** demo agent
/// but carrying a RAW (possibly non-JSON) body and custom headers, returning
/// (status, raw response text).
pub async fn signed_raw(
    app: &Router,
    method: Method,
    uri: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let (keypair, identity) = well_known_demo_agent();
    let signature = keypair.sign_request(
        &identity.id,
        method.as_str(),
        uri,
        body.as_bytes(),
        now() as u64,
    );
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let req = builder
        .header(AGENT_ID_HEADER, &identity.id)
        .header(SIGNATURE_HEADER, serde_json::to_string(&signature).unwrap())
        .body(Body::from(body.to_string()))
        .unwrap();
    send_text(app, req).await
}
