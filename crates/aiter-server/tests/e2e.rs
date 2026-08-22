//! End-to-end integration test (issue #30): an agent buys and the order is
//! reconciled as paid via a VERIFIED Razorpay webhook.
//!
//! Drives the real axum router over `tower::ServiceExt::oneshot` — no real
//! Razorpay is ever contacted. The payment-link mint runs against a throwaway
//! mock Razorpay server on a random local port (pointed at via
//! `RAZORPAY_BASE_URL`), and the `payment.paid` webhook is a fixture signed
//! with the same `RAZORPAY_WEBHOOK_SECRET` the server verifies against.
//!
//! The whole journey lives in ONE test function on purpose: env vars are
//! process-global, and this binary runs its tests in parallel threads — a
//! single test fn cannot race itself (other test binaries are separate
//! processes and cannot see these vars).

use std::sync::{Arc, Mutex};

use aiter_core::amount::{Amount, Currency};
use aiter_core::order::OrderStatus;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use aiter_core::signing::AgentKeypair;

use aiter_server::catalog::{seed_catalog, AppState};
use aiter_server::test_util;

/// Webhook secret the fixture signature is computed with — must match the
/// `RAZORPAY_WEBHOOK_SECRET` the server is configured with.
const WEBHOOK_SECRET: &str = "whsec_e2e_test";
/// Fixed short_url the mock Razorpay returns for `POST /v1/payment_links`.
const MOCK_SHORT_URL: &str = "https://rzp.io/e2e/aiter-checkout";

type HmacSha256 = Hmac<Sha256>;

/// Hex HMAC-SHA256 over the raw webhook body — the exact algorithm the server
/// verifies (`payments::verify_webhook_signature`).
fn fixture_signature(body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A Razorpay `payment.paid` webhook delivery, signed with [`WEBHOOK_SECRET`]
/// and addressed to the given order.
fn signed_webhook_request(order_id: &str) -> Request<Body> {
    let raw = json!({
        "event": "payment.paid",
        "payload": {
            "payment": {
                "entity": {
                    "id": "pay_e2e_123",
                    "notes": { "order_id": order_id },
                    "amount": 900,
                    "currency": "USD",
                }
            }
        }
    })
    .to_string();
    let signature = fixture_signature(raw.as_bytes());
    Request::builder()
        .method(Method::POST)
        .uri("/webhooks/razorpay")
        .header("content-type", "application/json")
        .header("x-razorpay-signature", signature)
        .body(Body::from(raw))
        .unwrap()
}

#[tokio::test]
async fn agent_lists_catalog_buys_and_order_is_paid_via_verified_webhook() {
    // --- Mock Razorpay: fixed short_url, capturing request bodies ----------
    let seen_body: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let sink = seen_body.clone();
    let razorpay_mock = Router::new().route(
        "/v1/payment_links",
        post(move |req: Request<Body>| {
            let sink = sink.clone();
            async move {
                let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                *sink.lock().unwrap() = serde_json::from_slice(&bytes).unwrap();
                Json(json!({ "id": "plink_e2e_1", "short_url": MOCK_SHORT_URL }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, razorpay_mock).await.unwrap();
    });

    // --- Point the payment rail at the mock (handlers read these env vars) --
    std::env::set_var("RAZORPAY_KEY_ID", "rzp_test_e2e");
    std::env::set_var("RAZORPAY_KEY_SECRET", "rzp_test_secret");
    std::env::set_var("RAZORPAY_MODE", "sandbox");
    std::env::set_var("RAZORPAY_BASE_URL", &base_url);
    std::env::set_var("RAZORPAY_WEBHOOK_SECRET", WEBHOOK_SECRET);

    // --- AppState + demo agent (AppState::default() registers no agents) ----
    let state = AppState::new(seed_catalog());
    let keypair = AgentKeypair::generate();
    let identity = keypair.identity("e2e-agent");
    state
        .register_agent(identity.clone(), Amount::new(1_000_000, Currency::USD))
        .await;
    let app = aiter_server::router(state.clone());

    // --- 1. Browse the (public) catalog; pick p-latte @ $4.50 --------------
    let (status, catalog) = test_util::send(
        &app,
        Request::builder()
            .uri("/catalog/products")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let latte = catalog["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "p-latte")
        .expect("catalog lists p-latte");
    assert_eq!(latte["price"]["units"], 450, "p-latte unit price");

    // --- 2. Add 2x p-latte to a cart -> subtotal 900 -----------------------
    let (status, cart) = test_util::send(
        &app,
        test_util::signed_request(
            Method::POST,
            "/carts",
            json!({
                "currency": "USD",
                "items": [{ "product_id": "p-latte", "quantity": 2 }]
            }),
            &keypair,
            &identity.id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cart_id = cart["id"].as_str().unwrap().to_string();
    assert_eq!(cart["totals"]["subtotal"]["units"], 900);
    assert_eq!(cart["totals"]["total"]["units"], 900);

    // --- 3. Snapshot the cart into a checkout session ----------------------
    let (status, session) = test_util::send(
        &app,
        test_util::signed_request(
            Method::POST,
            "/checkout_sessions",
            json!({ "cart_id": cart_id }),
            &keypair,
            &identity.id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cs_id = session["id"].as_str().unwrap().to_string();
    assert_eq!(session["totals"]["total"]["units"], 900);

    // --- 4. Complete checkout -> order Placed, totals 900 ------------------
    let (status, order) = test_util::send(
        &app,
        test_util::signed_request(
            Method::POST,
            &format!("/checkout_sessions/{cs_id}/complete"),
            json!(null),
            &keypair,
            &identity.id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let order_id = order["id"].as_str().unwrap().to_string();
    assert_eq!(order["status"], "Placed");
    assert_eq!(order["totals"]["total"]["units"], 900);

    // --- 5. Mint a payment link against the MOCK -> fixed short_url --------
    let (status, link) = test_util::send(
        &app,
        test_util::signed_request(
            Method::POST,
            &format!("/orders/{order_id}/payment_link"),
            json!(null),
            &keypair,
            &identity.id,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(link["order_id"], order_id);
    assert_eq!(link["short_url"], MOCK_SHORT_URL);
    // The mock saw the order total + currency and our order id as the note
    // the webhook will be reconciled against.
    let sent = seen_body.lock().unwrap().clone();
    assert_eq!(sent["amount"], 900, "payment link amount = order total");
    assert_eq!(sent["currency"], "USD");
    assert_eq!(sent["notes"]["order_id"], order_id);

    // --- 6. Verified payment.paid webhook -> order Confirmed + paid --------
    let (status, receipt) = test_util::send(&app, signed_webhook_request(&order_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["received"], true);
    assert_eq!(receipt["payment_id"], "pay_e2e_123");
    assert_eq!(receipt["status"], "paid");

    let paid = state.order(&order_id).await.expect("order exists");
    assert_eq!(paid.status, OrderStatus::Confirmed);
    assert_eq!(paid.payment_reference.as_deref(), Some("pay_e2e_123"));
    assert_eq!(paid.timeline.len(), 2, "Placed + one Confirm transition");

    // --- 7. Idempotency: re-delivery changes nothing -----------------------
    let (status, receipt) = test_util::send(&app, signed_webhook_request(&order_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["status"], "already_paid");

    let paid_again = state.order(&order_id).await.expect("order exists");
    assert_eq!(paid_again.status, OrderStatus::Confirmed);
    assert_eq!(paid_again.payment_reference.as_deref(), Some("pay_e2e_123"));
    assert_eq!(
        paid_again.timeline.len(),
        2,
        "duplicate webhook must not add a transition"
    );

    // --- 8. A tampered signature is rejected without touching the order ----
    let mut tampered = signed_webhook_request(&order_id);
    tampered
        .headers_mut()
        .insert("x-razorpay-signature", "deadbeef".parse().unwrap());
    let (status, _) = test_util::send(&app, tampered).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let untouched = state.order(&order_id).await.unwrap();
    assert_eq!(untouched.status, OrderStatus::Confirmed);
    assert_eq!(untouched.payment_reference.as_deref(), Some("pay_e2e_123"));
}
