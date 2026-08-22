//! Integration tests for the example agent client `aiter-cli` (issue #29).
//!
//! These exercise the CLI's own request-building/flow code
//! (`aiter_server::cli`) against the **real** axum router, spawned on a
//! random local port, with the merchant's well-known demo agent registered by
//! `AppState::default()`:
//!
//! * the full signed flow (via [`aiter_server::cli::run_flow`]) returns the
//!   payment link `short_url` minted by (a mock of) Razorpay,
//! * the same write without a signature is rejected `401` — proving the CLI
//!   actually signs,
//! * the CLI's request builder ([`aiter_server::cli::build_signed_request`])
//!   verifies against `require_signed` via `tower::oneshot`.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

use aiter_server::auth::{AGENT_ID_HEADER, SIGNATURE_HEADER};
use aiter_server::catalog::{AppState, DEMO_AGENT_ID};
use aiter_server::cli::{build_signed_request, run_flow, FlowResult};

/// Bind an axum app on a random local port and return its base URL.
async fn spawn_server(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// A mock Razorpay payment-links endpoint: records the request body and
/// returns a fixed `short_url` (real Razorpay would mint a `rzp.io` link).
async fn razorpay_mock() -> (String, Arc<Mutex<Value>>) {
    let seen: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let sink = seen.clone();
    let app = Router::new().route(
        "/v1/payment_links",
        post(move |req: Request<Body>| {
            let sink = sink.clone();
            async move {
                let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap();
                *sink.lock().unwrap() = serde_json::from_slice(&bytes).unwrap();
                Json(json!({
                    "id": "pl_issue29",
                    "short_url": "https://rzp.io/i/issue29-demo",
                }))
            }
        }),
    );
    let base = spawn_server(app).await;
    (base, seen)
}

/// A merchant router with the well-known demo agent pre-registered
/// (`AppState::default()`, issue #29).
fn merchant() -> (Router, AppState) {
    let state = AppState::default();
    let app = aiter_server::router(state.clone());
    (app, state)
}

/// Set env vars for the duration of `f`, restoring prior values afterwards.
/// Serializes env-mutating tests across the test threads of this binary (same
/// discipline as `payments::tests::with_env_async`).
#[allow(clippy::await_holding_lock)]
async fn with_env_async<F: std::future::Future>(vars: &[(&str, Option<&str>)], f: F) -> F::Output {
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _lock = ENV_LOCK.lock().unwrap();

    struct Restore<'a>(Vec<(&'a str, Option<String>)>);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value.as_str()),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    let before = vars
        .iter()
        .map(|(key, _)| (*key, std::env::var(key).ok()))
        .collect::<Vec<_>>();
    let _restore = Restore(before);
    for (key, value) in vars {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    f.await
}

/// Run the CLI flow (via its own request-building/flow code) against a live
/// merchant and a mock Razorpay, returning the flow result plus the Razorpay
/// request body that was seen.
async fn run_cli_flow(
    merchant_base: &str,
    product_id: Option<&str>,
    qty: u32,
) -> (FlowResult, Value) {
    let (razorpay_base, seen) = razorpay_mock().await;

    with_env_async(
        &[
            ("RAZORPAY_KEY_ID", Some("rzp_test_keyid")),
            ("RAZORPAY_KEY_SECRET", Some("rzp_test_secret")),
            ("RAZORPAY_BASE_URL", Some(&razorpay_base)),
            ("RAZORPAY_MODE", None),
        ],
        async move {
            let http = reqwest::Client::new();
            let flow = run_flow(&http, merchant_base, product_id, qty)
                .await
                .expect("the CLI flow should succeed against the running merchant");
            let body = seen.lock().unwrap().clone();
            (flow, body)
        },
    )
    .await
}

#[tokio::test]
async fn full_signed_flow_returns_a_real_payment_link() {
    let (app, state) = merchant();
    let merchant_base = spawn_server(app).await;

    // Buy the first catalog product (id-ascending => p-coldbrew, 500 minor
    // units) x2 — no product id given, so the CLI picks it itself.
    let (flow, razorpay_body) = run_cli_flow(&merchant_base, None, 2).await;
    assert_eq!(flow.product_id, "p-coldbrew");
    assert!(flow.cart_id.starts_with("cart-"));
    assert!(flow.session_id.starts_with("cs-"));
    assert!(flow.order_id.starts_with("ord-"));
    assert_eq!(
        flow.short_url, "https://rzp.io/i/issue29-demo",
        "the CLI must report the payment-link short_url"
    );
    assert_eq!(
        razorpay_body["amount"], 1000,
        "p-coldbrew x2 = 1000 minor units reaches Razorpay"
    );
    assert_eq!(
        razorpay_body["notes"]["order_id"], flow.order_id,
        "the link is tied to the completed order"
    );

    // The completed checkout was attributed to the well-known demo agent.
    let audit = state.audit_entries().await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].entry.agent_id, DEMO_AGENT_ID);

    // An explicit product id + qty flows through the same signed path.
    let (flow, razorpay_body) = run_cli_flow(&merchant_base, Some("p-latte"), 1).await;
    assert_eq!(flow.product_id, "p-latte");
    assert_eq!(razorpay_body["amount"], 450, "p-latte x1 = 450 minor units");
    assert_eq!(state.audit_entries().await.len(), 2);
}

#[tokio::test]
async fn unsigned_write_is_rejected_401() {
    let (app, _state) = merchant();
    let merchant_base = spawn_server(app).await;
    let http = reqwest::Client::new();

    // No x-agent-id / x-request-signature headers: the require_signed
    // middleware must refuse the write before any handler runs. The signed
    // flow above proves the CLI actually signs; this proves the server
    // actually checks.
    let response = http
        .post(format!("{merchant_base}/carts"))
        .header("content-type", "application/json")
        .body(r#"{"currency":"USD","items":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cli_request_builder_passes_require_signed_via_oneshot() {
    let (app, _state) = merchant();

    // The CLI's request builder signs as the well-known demo agent.
    let signed = build_signed_request("/carts", json!({ "currency": "USD", "items": [] }));
    assert_eq!(signed.agent_id_header, DEMO_AGENT_ID);

    // Feeding that exact request through the router's signed-write gate must
    // pass — the middleware verifies with the same registered identity.
    let req = Request::builder()
        .method("POST")
        .uri("/carts")
        .header("content-type", "application/json")
        .header(AGENT_ID_HEADER, &signed.agent_id_header)
        .header(SIGNATURE_HEADER, &signed.signature_header)
        .body(Body::from(signed.body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
