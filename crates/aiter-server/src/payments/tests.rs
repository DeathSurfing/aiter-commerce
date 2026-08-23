use super::api::webhook_error_response;
use super::client::constant_time_eq;
use super::webhook::{process_payment_webhook, WebhookError, WebhookOutcome};
use super::*;
use crate::catalog::AppState;
use crate::test_util;
use aiter_core::amount::Currency;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

// --- test helpers ------------------------------------------------------

/// Set/clear the given env vars for the duration of `f`, restoring prior
/// values afterwards. Serializes env-mutating tests (Rust test threads run
/// in parallel, and `std::env` is process-global).
fn with_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
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
    f();
}

/// Expected `Authorization` header for basic auth with the given key pair.
fn basic_auth_value(key_id: &str, key_secret: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    format!(
        "Basic {}",
        STANDARD.encode(format!("{key_id}:{key_secret}"))
    )
}

fn test_client() -> RazorpayClient {
    RazorpayClient::new(RazorpayConfig {
        key_id: "rzp_test_keyid".to_string(),
        key_secret: "rzp_test_secret".to_string(),
        webhook_secret: None,
        mode: RazorpayMode::Sandbox,
        base_url: "https://api.razorpay.com".to_string(),
    })
}

/// A client configured with a webhook secret (for verification tests).
fn webhook_test_client() -> RazorpayClient {
    RazorpayClient::new(RazorpayConfig {
        key_id: "rzp_test_keyid".to_string(),
        key_secret: "rzp_test_secret".to_string(),
        webhook_secret: Some("whsec_test_secret".to_string()),
        mode: RazorpayMode::Sandbox,
        base_url: "https://api.razorpay.com".to_string(),
    })
}

/// Compute the Razorpay webhook signature for a body: hex-encoded
/// HMAC-SHA256 over the raw body keyed with the webhook secret — the exact
/// algorithm the server verifies with (#20).
fn fixture_signature(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Bind a throwaway mock Razorpay server on a random local port.
async fn mock_server(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// --- (a) request construction -----------------------------------------

#[test]
fn order_request_sends_post_to_v1_orders_with_basic_auth_and_body() {
    let req = test_client()
        .build_order_request(499, Currency::INR, Some("receipt_1"))
        .unwrap();
    assert_eq!(req.method().as_str(), "POST");
    assert_eq!(req.url().as_str(), "https://api.razorpay.com/v1/orders");
    let auth = req
        .headers()
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(auth, basic_auth_value("rzp_test_keyid", "rzp_test_secret"));
    let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(body["amount"], 499);
    assert_eq!(body["currency"], "INR");
    assert_eq!(body["receipt"], "receipt_1");
}

#[test]
fn order_request_omits_receipt_when_none() {
    let req = test_client()
        .build_order_request(100, Currency::USD, None)
        .unwrap();
    let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(body["amount"], 100);
    assert_eq!(body["currency"], "USD");
    assert!(body.get("receipt").is_none());
}

#[test]
fn client_and_config_debug_redact_secret() {
    let client = webhook_test_client();
    let cfg_debug = format!("{:?}", client.config);
    let client_debug = format!("{:?}", client);
    assert!(!cfg_debug.contains("rzp_test_secret"));
    assert!(!cfg_debug.contains("whsec_test_secret"));
    assert!(!client_debug.contains("rzp_test_secret"));
    assert!(!client_debug.contains("whsec_test_secret"));
    assert!(cfg_debug.contains("rzp_test_keyid"));
}

// --- (b) integration-style against a local mock server ----------------

#[tokio::test]
async fn create_order_returns_order_id_from_mock_server() {
    let seen_auth: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let seen_body: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let auth = seen_auth.clone();
    let body = seen_body.clone();
    let app = Router::new().route(
        "/v1/orders",
        post(move |req: Request<Body>| {
            let auth = auth.clone();
            let body = body.clone();
            async move {
                let header = req
                    .headers()
                    .get("authorization")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap();
                *auth.lock().unwrap() = header;
                *body.lock().unwrap() = serde_json::from_slice(&bytes).unwrap();
                Json(json!({ "id": "order_mock_123" }))
            }
        }),
    );
    let base_url = mock_server(app).await;
    let client = RazorpayClient::new(RazorpayConfig {
        key_id: "rzp_test_keyid".to_string(),
        key_secret: "rzp_test_secret".to_string(),
        webhook_secret: None,
        mode: RazorpayMode::Sandbox,
        base_url,
    });
    let order_id = client
        .create_order(499, Currency::INR, Some("receipt_1"))
        .await
        .unwrap();
    assert_eq!(order_id, "order_mock_123");
    assert_eq!(
        *seen_auth.lock().unwrap(),
        basic_auth_value("rzp_test_keyid", "rzp_test_secret")
    );
    let body = seen_body.lock().unwrap().clone();
    assert_eq!(body["amount"], 499);
    assert_eq!(body["currency"], "INR");
    assert_eq!(body["receipt"], "receipt_1");
}

#[tokio::test]
async fn api_error_reports_status_and_never_leaks_secret() {
    let app = Router::new().route(
        "/v1/orders",
        post(|| async { (StatusCode::UNAUTHORIZED, "bad or missing keys") }),
    );
    let base_url = mock_server(app).await;
    let client = RazorpayClient::new(RazorpayConfig {
        key_id: "rzp_test_keyid".to_string(),
        key_secret: "rzp_test_secret".to_string(),
        webhook_secret: None,
        mode: RazorpayMode::Sandbox,
        base_url,
    });
    let err = client
        .create_order(100, Currency::INR, None)
        .await
        .unwrap_err();
    assert!(matches!(&err, RazorpayError::Api { status: 401, .. }));
    let debug = format!("{err:?}");
    assert!(debug.contains("401"));
    assert!(!debug.contains("rzp_test_secret"));
}

#[tokio::test]
async fn transport_error_never_leaks_secret() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // nothing is listening anymore -> connection refused
    let client = RazorpayClient::new(RazorpayConfig {
        key_id: "rzp_test_keyid".to_string(),
        key_secret: "rzp_test_secret".to_string(),
        webhook_secret: None,
        mode: RazorpayMode::Sandbox,
        base_url: format!("http://{addr}"),
    });
    let err = client
        .create_order(100, Currency::INR, None)
        .await
        .unwrap_err();
    assert!(matches!(&err, RazorpayError::Http(_)));
    assert!(!format!("{err:?}").contains("rzp_test_secret"));
}

// --- (c) config --------------------------------------------------------

const KEY_ID: (&str, Option<&str>) = ("RAZORPAY_KEY_ID", Some("rzp_test_keyid"));
const KEY_SECRET: (&str, Option<&str>) = ("RAZORPAY_KEY_SECRET", Some("rzp_test_secret"));
const MODE: (&str, Option<&str>) = ("RAZORPAY_MODE", None);
const BASE_URL: (&str, Option<&str>) = ("RAZORPAY_BASE_URL", None);

#[test]
fn config_defaults_to_sandbox_mode_and_api_base() {
    with_env(&[KEY_ID, KEY_SECRET, MODE, BASE_URL], || {
        let cfg = RazorpayConfig::from_env().unwrap();
        assert_eq!(cfg.key_id, "rzp_test_keyid");
        assert_eq!(cfg.key_secret, "rzp_test_secret");
        assert_eq!(cfg.mode, RazorpayMode::Sandbox);
        assert_eq!(cfg.base_url, "https://api.razorpay.com");
    });
}

#[test]
fn config_switches_mode_via_env() {
    with_env(
        &[
            KEY_ID,
            KEY_SECRET,
            ("RAZORPAY_MODE", Some("live")),
            BASE_URL,
        ],
        || {
            assert_eq!(RazorpayConfig::from_env().unwrap().mode, RazorpayMode::Live);
        },
    );
    with_env(
        &[
            KEY_ID,
            KEY_SECRET,
            ("RAZORPAY_MODE", Some("sandbox")),
            BASE_URL,
        ],
        || {
            assert_eq!(
                RazorpayConfig::from_env().unwrap().mode,
                RazorpayMode::Sandbox
            );
        },
    );
}

#[test]
fn config_rejects_unknown_mode() {
    with_env(
        &[
            KEY_ID,
            KEY_SECRET,
            ("RAZORPAY_MODE", Some("prod")),
            BASE_URL,
        ],
        || {
            let err = RazorpayConfig::from_env().unwrap_err();
            assert!(matches!(&err, RazorpayError::Config(_)));
            let msg = err.to_string();
            assert!(msg.contains("RAZORPAY_MODE"));
            assert!(msg.contains("sandbox"));
            assert!(msg.contains("live"));
        },
    );
}

#[test]
fn config_missing_key_id_is_clear_error() {
    with_env(
        &[("RAZORPAY_KEY_ID", None), KEY_SECRET, MODE, BASE_URL],
        || {
            let err = RazorpayConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("RAZORPAY_KEY_ID"));
        },
    );
}

#[test]
fn config_missing_key_secret_is_clear_error() {
    with_env(
        &[KEY_ID, ("RAZORPAY_KEY_SECRET", None), MODE, BASE_URL],
        || {
            let err = RazorpayConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("RAZORPAY_KEY_SECRET"));
        },
    );
}

#[test]
fn config_base_url_override_wins() {
    with_env(
        &[
            KEY_ID,
            KEY_SECRET,
            MODE,
            ("RAZORPAY_BASE_URL", Some("http://localhost:1234")),
        ],
        || {
            let cfg = RazorpayConfig::from_env().unwrap();
            assert_eq!(cfg.base_url, "http://localhost:1234");
            assert_eq!(cfg.mode, RazorpayMode::Sandbox);
        },
    );
}

#[test]
fn config_reads_optional_webhook_secret() {
    with_env(
        &[
            KEY_ID,
            KEY_SECRET,
            MODE,
            BASE_URL,
            ("RAZORPAY_WEBHOOK_SECRET", Some("whsec_env")),
        ],
        || {
            let cfg = RazorpayConfig::from_env().unwrap();
            assert_eq!(cfg.webhook_secret.as_deref(), Some("whsec_env"));
        },
    );
    with_env(
        &[
            KEY_ID,
            KEY_SECRET,
            MODE,
            BASE_URL,
            ("RAZORPAY_WEBHOOK_SECRET", None),
        ],
        || {
            let cfg = RazorpayConfig::from_env().unwrap();
            assert_eq!(cfg.webhook_secret, None);
        },
    );
}

// --- (d) webhook signature verification (#20) --------------------------

/// A minimal `payment.paid` webhook body carrying our order reference in
/// `notes` and the Razorpay payment id in `payment.entity.id`.
const WEBHOOK_BODY: &[u8] = br#"{"account_id":"acc_mock","event":"payment.paid","contains":["payment"],"payload":{"payment":{"entity":{"id":"pay_fixture","order_id":"order_mock_123","notes":{"order_id":"ord-cs-0"},"amount":499,"currency":"USD"}}}}"#;

#[test]
fn verify_accepts_valid_signature() {
    let client = webhook_test_client();
    let signature = fixture_signature("whsec_test_secret", WEBHOOK_BODY);
    assert!(client
        .verify_webhook_signature(WEBHOOK_BODY, &signature)
        .is_ok());
}

#[test]
fn verify_rejects_tampered_body() {
    let client = webhook_test_client();
    let signature = fixture_signature("whsec_test_secret", WEBHOOK_BODY);
    let tampered: &[u8] = br#"{"account_id":"acc_mock","event":"payment.paid","contains":["payment"],"payload":{"payment":{"entity":{"id":"pay_tampered","notes":{"order_id":"ord-cs-0"},"amount":999,"currency":"USD"}}}}"#;
    let err = client
        .verify_webhook_signature(tampered, &signature)
        .unwrap_err();
    assert!(matches!(&err, RazorpayError::Signature(_)));
}

#[test]
fn verify_rejects_wrong_signature() {
    let client = webhook_test_client();
    let err = client
        .verify_webhook_signature(WEBHOOK_BODY, "deadbeefdeadbeef")
        .unwrap_err();
    assert!(matches!(&err, RazorpayError::Signature(_)));
}

#[test]
fn verify_without_secret_fails_closed() {
    let client = test_client(); // no RAZORPAY_WEBHOOK_SECRET configured
    let err = client
        .verify_webhook_signature(WEBHOOK_BODY, "deadbeef")
        .unwrap_err();
    assert!(matches!(&err, RazorpayError::Config(_)));
    assert!(
        err.to_string().contains("RAZORPAY_WEBHOOK_SECRET"),
        "error should name the missing var: {err}"
    );
}

#[test]
fn constant_time_eq_compares_without_early_exit() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"abcd")); // length mismatch
    assert!(!constant_time_eq(b"", b"x"));
}

// --- (e) payment links (#19) -------------------------------------------

#[test]
fn payment_link_request_posts_to_v1_payment_links_with_auth_amount_currency_and_order_note() {
    let req = test_client()
        .build_payment_link_request(499, Currency::INR, Some("ord-cs-0"))
        .unwrap();
    assert_eq!(req.method().as_str(), "POST");
    assert_eq!(
        req.url().as_str(),
        "https://api.razorpay.com/v1/payment_links"
    );
    let auth = req
        .headers()
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(auth, basic_auth_value("rzp_test_keyid", "rzp_test_secret"));
    let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(body["amount"], 499);
    assert_eq!(body["currency"], "INR");
    assert_eq!(body["notes"]["order_id"], "ord-cs-0");
}

#[test]
fn payment_link_request_omits_notes_when_no_order_id() {
    let req = test_client()
        .build_payment_link_request(100, Currency::USD, None)
        .unwrap();
    let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(body["amount"], 100);
    assert_eq!(body["currency"], "USD");
    assert!(body.get("notes").is_none());
}

#[tokio::test]
async fn create_payment_link_returns_short_url_from_mock_server() {
    let seen_auth: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let seen_body: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let auth = seen_auth.clone();
    let body = seen_body.clone();
    let app = Router::new().route(
        "/v1/payment_links",
        post(move |req: Request<Body>| {
            let auth = auth.clone();
            let body = body.clone();
            async move {
                let header = req
                    .headers()
                    .get("authorization")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap();
                *auth.lock().unwrap() = header;
                *body.lock().unwrap() = serde_json::from_slice(&bytes).unwrap();
                Json(json!({
                    "id": "plink_mock_123",
                    "short_url": "https://rzp.io/i/mock-link"
                }))
            }
        }),
    );
    let base_url = mock_server(app).await;
    let client = RazorpayClient::new(RazorpayConfig {
        key_id: "rzp_test_keyid".to_string(),
        key_secret: "rzp_test_secret".to_string(),
        webhook_secret: None,
        mode: RazorpayMode::Sandbox,
        base_url,
    });
    let short_url = client
        .create_payment_link(499, Currency::INR, Some("ord-cs-0"))
        .await
        .unwrap();
    assert_eq!(short_url, "https://rzp.io/i/mock-link");
    assert_eq!(
        *seen_auth.lock().unwrap(),
        basic_auth_value("rzp_test_keyid", "rzp_test_secret")
    );
    let body = seen_body.lock().unwrap().clone();
    assert_eq!(body["amount"], 499);
    assert_eq!(body["currency"], "INR");
    assert_eq!(body["notes"]["order_id"], "ord-cs-0");
}

// --- (f) order-paid reconciliation (#21) -------------------------------

use crate::catalog::seed_catalog;
use aiter_core::amount::Amount;
use aiter_core::order::{Order, OrderStatus};
use aiter_core::pricing::Totals;
use aiter_core::store::Store;

/// Seed an order in `Placed` status (as produced by checkout completion).
async fn seed_order(st: &AppState, id: &str) {
    let order = Order::new(
        id.to_string(),
        "cs-0".to_string(),
        Totals {
            subtotal: Amount::new(499, Currency::USD),
            tax: Amount::new(0, Currency::USD),
            total: Amount::new(499, Currency::USD),
        },
        1_000,
    );
    st.orders
        .lock()
        .await
        .create(id.to_string(), order)
        .unwrap();
}

#[tokio::test]
async fn payment_paid_webhook_reconciles_order_to_confirmed_with_receipt() {
    let st = AppState::new(seed_catalog());
    seed_order(&st, "ord-cs-0").await;
    let client = webhook_test_client();
    let signature = fixture_signature("whsec_test_secret", WEBHOOK_BODY);

    let outcome = process_payment_webhook(&st, &client, WEBHOOK_BODY, &signature)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        WebhookOutcome::Paid { ref order_id, ref payment_id }
            if order_id == "ord-cs-0" && payment_id == "pay_fixture"
    ));

    let order = st
        .orders
        .lock()
        .await
        .get(&"ord-cs-0".to_string())
        .cloned()
        .unwrap();
    // No Paid variant in the core state machine — Confirm is the closest
    // legal transition; the transaction id is recorded on the order.
    assert_eq!(order.status, OrderStatus::Confirmed);
    assert_eq!(order.payment_reference.as_deref(), Some("pay_fixture"));
    assert_eq!(order.timeline.len(), 2, "Placed + one Confirm entry");
}

#[tokio::test]
async fn duplicate_payment_paid_webhook_is_idempotent_noop() {
    let st = AppState::new(seed_catalog());
    seed_order(&st, "ord-cs-0").await;
    let client = webhook_test_client();
    let signature = fixture_signature("whsec_test_secret", WEBHOOK_BODY);

    process_payment_webhook(&st, &client, WEBHOOK_BODY, &signature)
        .await
        .unwrap();

    let outcome = process_payment_webhook(&st, &client, WEBHOOK_BODY, &signature)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        WebhookOutcome::AlreadyPaid { ref order_id, ref payment_id }
            if order_id == "ord-cs-0" && payment_id == "pay_fixture"
    ));

    let order = st
        .orders
        .lock()
        .await
        .get(&"ord-cs-0".to_string())
        .cloned()
        .unwrap();
    assert_eq!(order.status, OrderStatus::Confirmed);
    assert_eq!(order.payment_reference.as_deref(), Some("pay_fixture"));
    assert_eq!(
        order.timeline.len(),
        2,
        "duplicate webhook must not add a transition"
    );
}

#[tokio::test]
async fn invalid_signature_is_rejected_before_reconcile() {
    let st = AppState::new(seed_catalog());
    seed_order(&st, "ord-cs-0").await;
    let client = webhook_test_client();

    let err = process_payment_webhook(&st, &client, WEBHOOK_BODY, "deadbeef")
        .await
        .unwrap_err();
    assert!(matches!(err, WebhookError::Signature(_)));

    let order = st
        .orders
        .lock()
        .await
        .get(&"ord-cs-0".to_string())
        .cloned()
        .unwrap();
    assert_eq!(
        order.status,
        OrderStatus::Placed,
        "order untouched by bad signature"
    );
    assert_eq!(order.payment_reference, None);
}

#[tokio::test]
async fn non_paid_event_is_ignored() {
    let st = AppState::new(seed_catalog());
    seed_order(&st, "ord-cs-0").await;
    let body: &[u8] = br#"{"event":"payment.failed","payload":{"payment":{"entity":{"id":"pay_x","notes":{"order_id":"ord-cs-0"},"amount":499,"currency":"USD"}}}}"#;
    let client = webhook_test_client();
    let signature = fixture_signature("whsec_test_secret", body);

    let outcome = process_payment_webhook(&st, &client, body, &signature)
        .await
        .unwrap();
    assert!(matches!(outcome, WebhookOutcome::Ignored));

    let order = st
        .orders
        .lock()
        .await
        .get(&"ord-cs-0".to_string())
        .cloned()
        .unwrap();
    assert_eq!(order.status, OrderStatus::Placed);
}

#[tokio::test]
async fn payment_paid_without_order_note_reference_is_an_error() {
    let st = AppState::new(seed_catalog());
    let body: &[u8] =
        br#"{"event":"payment.paid","payload":{"payment":{"entity":{"id":"pay_orphan","amount":100,"currency":"USD"}}}}"#;
    let client = webhook_test_client();
    let signature = fixture_signature("whsec_test_secret", body);

    let err = process_payment_webhook(&st, &client, body, &signature)
        .await
        .unwrap_err();
    assert!(matches!(err, WebhookError::NoOrderReference));
}

// --- (f2) amount/currency binding (#69) --------------------------------

/// A `payment.paid` body with an explicit amount/currency addressed to
/// `ord-cs-0` (seeded at 499 USD by [`seed_order`]).
fn paid_body(amount: i64, currency: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "event": "payment.paid",
        "payload": {
            "payment": {
                "entity": {
                    "id": "pay_bound",
                    "notes": { "order_id": "ord-cs-0" },
                    "amount": amount,
                    "currency": currency
                }
            }
        }
    }))
    .unwrap()
}

async fn process_paid(st: &AppState, body: &[u8]) -> Result<WebhookOutcome, WebhookError> {
    let client = webhook_test_client();
    let signature = fixture_signature("whsec_test_secret", body);
    process_payment_webhook(st, &client, body, &signature).await
}

async fn seeded_order_state(st: &AppState) -> (OrderStatus, Option<String>) {
    let order = st
        .orders
        .lock()
        .await
        .get(&"ord-cs-0".to_string())
        .cloned()
        .unwrap();
    (order.status, order.payment_reference)
}

#[tokio::test]
async fn underpaid_payment_webhook_conflicts_and_leaves_order_payable() {
    let st = AppState::new(seed_catalog());
    seed_order(&st, "ord-cs-0").await;
    let body = paid_body(498, "USD");

    let err = process_paid(&st, &body).await.unwrap_err();
    assert!(matches!(
        err,
        WebhookError::AmountMismatch {
            expected: 499,
            got: 498
        }
    ));

    let (status, reference) = seeded_order_state(&st).await;
    assert_eq!(status, OrderStatus::Placed, "underpay must not transition");
    assert_eq!(reference, None);

    // The order stays payable by the correct webhook afterwards.
    let outcome = process_paid(&st, WEBHOOK_BODY).await.unwrap();
    assert!(matches!(outcome, WebhookOutcome::Paid { .. }));
}

#[tokio::test]
async fn overpaid_payment_webhook_conflicts_exact_match_only() {
    let st = AppState::new(seed_catalog());
    seed_order(&st, "ord-cs-0").await;
    let body = paid_body(500, "USD");

    let err = process_paid(&st, &body).await.unwrap_err();
    assert!(matches!(
        err,
        WebhookError::AmountMismatch {
            expected: 499,
            got: 500
        }
    ));

    let (status, reference) = seeded_order_state(&st).await;
    assert_eq!(status, OrderStatus::Placed, "overpay must not transition");
    assert_eq!(reference, None);
}

#[tokio::test]
async fn wrong_currency_payment_webhook_conflicts() {
    let st = AppState::new(seed_catalog());
    seed_order(&st, "ord-cs-0").await;
    let body = paid_body(499, "INR"); // right amount, wrong currency

    let err = process_paid(&st, &body).await.unwrap_err();
    assert!(matches!(
        err,
        WebhookError::AmountMismatch {
            expected: 499,
            got: 499
        }
    ));

    let (status, reference) = seeded_order_state(&st).await;
    assert_eq!(
        status,
        OrderStatus::Placed,
        "currency mismatch must not transition"
    );
    assert_eq!(reference, None);
}

#[tokio::test]
async fn exact_amount_with_case_insensitive_currency_reconciles() {
    let st = AppState::new(seed_catalog());
    seed_order(&st, "ord-cs-0").await;
    let body = paid_body(499, "usd"); // ISO codes compare case-insensitively

    let outcome = process_paid(&st, &body).await.unwrap();
    assert!(matches!(
        outcome,
        WebhookOutcome::Paid { ref order_id, ref payment_id }
            if order_id == "ord-cs-0" && payment_id == "pay_bound"
    ));
    let (status, reference) = seeded_order_state(&st).await;
    assert_eq!(status, OrderStatus::Confirmed);
    assert_eq!(reference.as_deref(), Some("pay_bound"));
}

#[tokio::test]
async fn duplicate_delivery_returns_already_paid_before_any_comparison() {
    let st = AppState::new(seed_catalog());
    seed_order(&st, "ord-cs-0").await;

    // First delivery reconciles; the second one would fail the binding if
    // it were compared — idempotency must win first.
    let first = process_paid(&st, WEBHOOK_BODY).await.unwrap();
    assert!(matches!(first, WebhookOutcome::Paid { .. }));
    let outcome = process_paid(&st, &paid_body(100, "EUR")).await.unwrap();

    assert!(matches!(
        outcome,
        WebhookOutcome::AlreadyPaid { ref payment_id, .. } if payment_id == "pay_fixture"
    ));
    let (status, reference) = seeded_order_state(&st).await;
    assert_eq!(status, OrderStatus::Confirmed);
    assert_eq!(reference.as_deref(), Some("pay_fixture"));
}

#[test]
fn amount_mismatch_maps_to_409_with_error_json_shape() {
    let (code, body) = webhook_error_response(WebhookError::AmountMismatch {
        expected: 499,
        got: 498,
    });
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(
        body.0["error"],
        json!("payment does not match the order total (amount + currency): expected 499, got 498")
    );
}

// --- (g) HTTP surface: payment link + webhook routes -------------------

use axum::http::Method;

/// Set env vars for the duration of an async block. Same serialization
/// discipline as `with_env`; tokio `current_thread` runtimes never wait on
/// each other, so holding the lock across `.await` points is safe.
#[allow(clippy::await_holding_lock)]
async fn with_env_async<F: std::future::Future<Output = ()>>(vars: &[(&str, Option<&str>)], f: F) {
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
    f.await;
}

/// Drive the full router with `tower::ServiceExt::oneshot`.

#[tokio::test]
async fn integration_checkout_then_payment_link_then_webhook_reconciles_order() {
    let seen_body: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let body_sink = seen_body.clone();
    let razorpay_mock = Router::new().route(
        "/v1/payment_links",
        post(move |req: Request<Body>| {
            let body_sink = body_sink.clone();
            async move {
                let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap();
                *body_sink.lock().unwrap() = serde_json::from_slice(&bytes).unwrap();
                Json(json!({
                    "id": "plink_int_1",
                    "short_url": "https://rzp.io/i/int-flow"
                }))
            }
        }),
    );
    let base_url = mock_server(razorpay_mock).await;

    with_env_async(
        &[
            ("RAZORPAY_KEY_ID", Some("rzp_test_keyid")),
            ("RAZORPAY_KEY_SECRET", Some("rzp_test_secret")),
            ("RAZORPAY_WEBHOOK_SECRET", Some("whsec_test_secret")),
            ("RAZORPAY_BASE_URL", Some(base_url.as_str())),
            ("RAZORPAY_MODE", None),
        ],
        async move {
            let st = AppState::new(seed_catalog());
            test_util::register_test_agent(&st).await;
            let app = crate::router(st.clone());

            // 1. Checkout flow -> order in Placed status (write routes
            // require an agent signature, see trust.md / lib.rs docs).
            let (_, cart) = test_util::call(
                &app,
                Method::POST,
                "/carts",
                Some(json!({
                    "currency": "USD",
                    "items": [{"product_id": "p-espresso", "quantity": 2}]
                })),
            )
            .await;
            let cart_id = cart["id"].as_str().unwrap().to_string();

            let (_, session) = test_util::call(
                &app,
                Method::POST,
                "/checkout_sessions",
                Some(json!({ "cart_id": cart_id })),
            )
            .await;
            let cs_id = session["id"].as_str().unwrap().to_string();

            let (status, order) = test_util::call(
                &app,
                Method::POST,
                &format!("/checkout_sessions/{cs_id}/complete"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let order_id = order["id"].as_str().unwrap().to_string();
            assert_eq!(order["status"], "Placed");

            // 2. Generate a payment link for the order (via mock Razorpay);
            // the route is agent-protected, so the request is signed.
            let (status, link) = test_util::call(
                &app,
                Method::POST,
                &format!("/orders/{order_id}/payment_link"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(link["order_id"], order_id);
            assert_eq!(link["short_url"], "https://rzp.io/i/int-flow");
            // The mock saw the correct amount/currency and our order note.
            let sent = seen_body.lock().unwrap().clone();
            assert_eq!(sent["amount"], 600, "2 x $3.00 (p-espresso) in minor units");
            assert_eq!(sent["currency"], "USD");
            assert_eq!(sent["notes"]["order_id"], order_id);

            // 3. payment.paid webhook with a fixture signature; carries the
            // exact order total (600 minor units) + currency (#69 binding).
            // Serialize once and sign those exact bytes: `call_unsigned`
            // re-serializes the value (sorted keys), so the signature must
            // cover the canonical form.
            let body = json!({
                "event": "payment.paid",
                "payload": {
                    "payment": {
                        "entity": {
                            "id": "pay_int_1",
                            "notes": { "order_id": order_id },
                            "amount": 600,
                            "currency": "USD"
                        }
                    }
                }
            })
            .to_string();
            let signature = fixture_signature("whsec_test_secret", body.as_bytes());
            let (status, receipt) = test_util::call_unsigned(
                &app,
                Method::POST,
                "/webhooks/razorpay",
                Some(serde_json::from_str(&body).unwrap()),
                &[("x-razorpay-signature", &signature)],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(receipt["received"], true);
            assert_eq!(receipt["payment_id"], "pay_int_1");

            let order = st.orders.lock().await.get(&order_id).cloned().unwrap();
            assert_eq!(order.status, OrderStatus::Confirmed);
            assert_eq!(order.payment_reference.as_deref(), Some("pay_int_1"));

            // 4. Duplicate webhook is a no-op (still one Confirm entry).
            let (status, _) = test_util::call_unsigned(
                &app,
                Method::POST,
                "/webhooks/razorpay",
                Some(serde_json::from_str(&body).unwrap()),
                &[("x-razorpay-signature", &signature)],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let order = st.orders.lock().await.get(&order_id).cloned().unwrap();
            assert_eq!(order.status, OrderStatus::Confirmed);
            assert_eq!(
                order.timeline.len(),
                2,
                "duplicate webhook: no double transition"
            );

            // 5. A bogus signature is rejected with 401.
            let (status, _) = test_util::call_unsigned(
                &app,
                Method::POST,
                "/webhooks/razorpay",
                Some(serde_json::from_str(&body).unwrap()),
                &[("x-razorpay-signature", "deadbeef")],
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        },
    )
    .await;
}

#[tokio::test]
async fn payment_link_endpoint_rejects_unknown_and_paid_orders() {
    let st = AppState::new(seed_catalog());
    test_util::register_test_agent(&st).await;
    seed_order(&st, "ord-cs-0").await;
    let app = crate::router(st.clone());

    // Unknown order -> 404 (signed: the route sits behind require_signed).
    let (status, _) = test_util::call(&app, Method::POST, "/orders/nope/payment_link", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A reconciled (Confirmed, i.e. already paid) order -> 409, no new link.
    {
        let mut orders = st.orders.lock().await;
        let mut o = orders.get(&"ord-cs-0".to_string()).cloned().unwrap();
        o.payment_reference = Some("pay_already".to_string());
        orders.update("ord-cs-0".to_string(), o).unwrap();
    }
    let (status, _) =
        test_util::call(&app, Method::POST, "/orders/ord-cs-0/payment_link", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}
