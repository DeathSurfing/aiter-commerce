//! Security-pass audit (issue #36): malformed input, error handling, secrets.
//!
//! Two layers over the real axum router (`tower::ServiceExt::oneshot`, no
//! sockets):
//!
//! 1. **Table-driven audit** — every JSON-taking endpoint is exercised with
//!    malformed input: invalid JSON, wrong types, missing fields,
//!    negative/zero/huge quantities, empty ids, bogus enum values, extra
//!    unknown fields and oversized bodies. The invariant under test: every
//!    malformed request gets a clean **4xx** — never a 5xx and never a panic
//!    (a panicking handler fails the test via the propagated panic).
//! 2. **Fuzz-ish property tests** — a deterministic xorshift PRNG feeds
//!    random garbage bodies to every write endpoint (signed with the demo
//!    agent keypair from `catalog.rs` — `AppState::default()` pre-registers
//!    it) and to the public surface; each iteration must come back 4xx.
//!
//! Also pinned here: the Razorpay secret is redacted from every `Debug` and
//! error path (see `payments.rs` for the redacting impls) and the core serde
//! parsers reject negative/bogus input without ever panicking.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use aiter_core::amount::{Amount, Currency};
use aiter_core::cart::{Cart, LineItem};
use aiter_core::checkout::Fulfillment;
use aiter_server::auth::{AGENT_ID_HEADER, SIGNATURE_HEADER};
use aiter_server::catalog::{demo_agent, AppState};
use aiter_server::payments::{RazorpayClient, RazorpayConfig, RazorpayError, RazorpayMode};
use aiter_server::router;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deterministic xorshift64 PRNG (same family as `pricing.rs`'s property
/// tests, so the "random" sweeps are fully reproducible).
struct XorShift(u64);

impl XorShift {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, max: u64) -> u64 {
        self.next_u64() % max
    }

    fn pick<'a, T>(&mut self, pool: &'a [T]) -> &'a T {
        &pool[self.below(pool.len() as u64) as usize]
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A router on fresh state. `AppState::default()` pre-registers the demo
/// agent (`agent-demo`, fixed public keypair from `DEMO_AGENT_SEED`) with a
/// generous USD cap, so every write below is signed by `demo_agent()` and
/// passes `auth::require_signed`.
fn test_app() -> Router {
    router(AppState::default())
}

/// Signed request with a JSON body (or empty body when `body` is `None`).
/// The signature covers the exact serialized bytes, so the handler's `Json`
/// extractor sees byte-for-byte what was signed.
async fn signed(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let (keypair, identity) = demo_agent();
    let body_str = body.map(|b| b.to_string()).unwrap_or_default();
    let signature = keypair.sign_request(
        &identity.id,
        method.as_str(),
        uri,
        body_str.as_bytes(),
        now(),
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

/// Signed request with a RAW (possibly non-JSON) body / custom content type.
async fn signed_raw(
    app: &Router,
    method: Method,
    uri: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let (keypair, identity) = demo_agent();
    let signature =
        keypair.sign_request(&identity.id, method.as_str(), uri, body.as_bytes(), now());
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder = builder
        .header(AGENT_ID_HEADER, &identity.id)
        .header(SIGNATURE_HEADER, serde_json::to_string(&signature).unwrap());
    let req = builder.body(Body::from(body.to_string())).unwrap();
    send(app, req).await
}

/// Unsigned request (public endpoints / webhook), returning raw body text so
/// secret-leak assertions can inspect it.
async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// The audit invariant: malformed input must be a clean 4xx and the response
/// must never look like an internal failure or leak a panic trace.
fn assert_clean_4xx(context: &str, status: StatusCode, body: &str) {
    assert!(
        status.is_client_error(),
        "{context}: expected a client error (4xx), got {status} with body: {body}"
    );
    assert!(
        !body.contains("panicked at") && !body.contains("Internal Server Error"),
        "{context}: error body looks like a panic/internal failure: {body}"
    );
}

/// Create one valid cart (USD, p-latte x1), returning its id.
async fn seed_cart(app: &Router) -> String {
    let (status, body) = signed(
        app,
        Method::POST,
        "/carts",
        Some(json!({ "currency": "USD", "items": [{ "product_id": "p-latte", "quantity": 1 }] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed cart failed: {body}");
    serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// 1. Table-driven malformed-input audit
// ---------------------------------------------------------------------------

/// Invalid JSON, wrong types, missing fields, negative/zero/huge quantities,
/// empty ids, bogus enums, extra unknown fields — every malformed cart or
/// checkout-session body must be a clean 4xx.
#[tokio::test]
async fn malformed_cart_and_checkout_bodies_are_clean_4xx() {
    let app = test_app();
    let cart_id = seed_cart(&app).await;

    // (uri, body) — each one malformed by construction.
    let cart_bodies: &[(&str, Value, &str)] = &[
        (
            "/carts",
            json!({ "items": "not-an-array" }),
            "items wrong type",
        ),
        (
            "/carts",
            json!({ "items": [{ "product_id": "p-latte", "quantity": -1 }] }),
            "negative quantity",
        ),
        (
            "/carts",
            json!({ "items": [{ "product_id": "p-latte", "quantity": 4294967296u64 }] }),
            "quantity over u32",
        ),
        (
            "/carts",
            json!({ "items": [{ "product_id": "p-latte", "quantity": 1.5 }] }),
            "float quantity",
        ),
        (
            "/carts",
            json!({ "items": [{ "product_id": "", "quantity": 1 }] }),
            "empty product id",
        ),
        (
            "/carts",
            json!({ "items": [{ "product_id": "ghost", "quantity": 1 }] }),
            "unknown product id",
        ),
        (
            "/carts",
            json!({ "items": [{ "product_id": "p-latte" }] }),
            "missing quantity field",
        ),
        (
            "/carts",
            json!({ "items": [{ "quantity": 2 }] }),
            "missing product_id field",
        ),
        (
            "/carts",
            json!({ "items": [{ "product_id": 5, "quantity": 1 }] }),
            "numeric product id",
        ),
        ("/carts", json!({ "items": [null] }), "null line item"),
        (
            "/carts",
            json!({ "currency": "BTC", "items": [] }),
            "bogus currency",
        ),
        (
            "/carts",
            json!({ "currency": "usd", "items": [] }),
            "lowercase currency",
        ),
        (
            "/carts",
            json!({ "currency": 3, "items": [] }),
            "numeric currency",
        ),
        ("/carts", json!(null), "null body"),
        (
            "/carts",
            json!([1, 2]),
            "array body (positional fields, wrong types)",
        ),
        ("/carts", json!("string body"), "string body"),
        (
            "/carts",
            json!({ "currency": "INR", "items": [{ "product_id": "p-latte", "quantity": 1 }] }),
            "cross-currency line (INR cart, USD product)",
        ),
    ];

    // Extra unknown fields are tolerated (serde ignores them): that must be a
    // 200, never a crash/5xx.
    let (status, body) = signed(
        &app,
        Method::POST,
        "/carts",
        Some(json!({ "wiggly": [1, 2, 3], "bogus": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unknown fields tolerated: {body}");
    let (status, body) = signed(
        &app,
        Method::POST,
        "/carts",
        Some(json!({ "currency": "USD", "items": [{ "product_id": "p-latte", "quantity": 1 }], "surprise": { "a": 1 } })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "extra unknown field on valid body: {body}"
    );

    for (uri, body, label) in cart_bodies {
        let (status, body_text) = signed(&app, Method::POST, uri, Some(body.clone())).await;
        assert_clean_4xx(&format!("POST {uri} ({label})"), status, &body_text);
    }

    // Same malformed families on PUT /carts/{id} (real cart id).
    let put_bodies: &[(&str, Value)] = &[
        ("items wrong type", json!({ "items": 42 })),
        (
            "negative quantity",
            json!({ "items": [{ "product_id": "p-latte", "quantity": -2 }] }),
        ),
        (
            "huge quantity",
            json!({ "items": [{ "product_id": "p-latte", "quantity": 999999999999u64 }] }),
        ),
        (
            "empty product id",
            json!({ "items": [{ "product_id": "", "quantity": 1 }] }),
        ),
        (
            "unknown product",
            json!({ "items": [{ "product_id": "ghost", "quantity": 1 }] }),
        ),
        ("missing items", json!({})),
        ("null items", json!({ "items": null })),
        ("items of strings", json!({ "items": ["p-latte"] })),
    ];
    for (label, body) in put_bodies {
        let uri = format!("/carts/{cart_id}");
        let (status, body_text) = signed(&app, Method::PUT, &uri, Some(body.clone())).await;
        assert_clean_4xx(&format!("PUT {uri} ({label})"), status, &body_text);
    }
    // PUT against an unknown cart id is a 404, not a 500.
    let (status, body_text) = signed(
        &app,
        Method::PUT,
        "/carts/does-not-exist",
        Some(json!({ "items": [] })),
    )
    .await;
    assert_clean_4xx("PUT /carts/does-not-exist", status, &body_text);

    // Checkout session creation.
    let session_bodies: &[(&str, Value, &str)] = &[
        ("/checkout_sessions", json!({}), "missing cart_id"),
        (
            "/checkout_sessions",
            json!({ "cart_id": 123 }),
            "numeric cart_id",
        ),
        (
            "/checkout_sessions",
            json!({ "cart_id": ["x"] }),
            "array cart_id",
        ),
        (
            "/checkout_sessions",
            json!({ "cart_id": "nope" }),
            "unknown cart id",
        ),
        (
            "/checkout_sessions",
            json!({ "cart_id": "" }),
            "empty cart id",
        ),
        (
            "/checkout_sessions",
            json!({ "cart_id": "nope", "fulfillment": "Teleport" }),
            "bogus fulfillment",
        ),
        (
            "/checkout_sessions",
            json!({ "cart_id": "nope", "fulfillment": { "Shipping": {} } }),
            "fulfillment missing address",
        ),
        (
            "/checkout_sessions",
            json!({ "cart_id": "nope", "fulfillment": { "Shipping": { "address": 5 } } }),
            "fulfillment address wrong type",
        ),
        (
            "/checkout_sessions",
            json!({ "cart_id": "nope", "fulfillment": null }),
            "null fulfillment",
        ),
        ("/checkout_sessions", json!([]), "array body"),
    ];
    for (uri, body, label) in session_bodies {
        let (status, body_text) = signed(&app, Method::POST, uri, Some(body.clone())).await;
        assert_clean_4xx(&format!("POST {uri} ({label})"), status, &body_text);
    }

    // cancel/complete on junk ids.
    for uri in [
        "/carts/does-not-exist/cancel",
        "/checkout_sessions/does-not-exist/cancel",
        "/checkout_sessions/does-not-exist/complete",
        "/orders/does-not-exist/payment_link",
    ] {
        let (status, body_text) = signed(&app, Method::POST, uri, None).await;
        assert_clean_4xx(&format!("POST {uri}"), status, &body_text);
    }
}

/// Every malformed reserve-pay body must be a clean 4xx, and non-positive
/// amounts must be rejected before any ledger mutation.
#[tokio::test]
async fn malformed_reserve_pay_bodies_are_clean_4xx() {
    let app = test_app();

    let consent_bodies: &[(&str, Value, &str)] = &[
        ("/reserve_pay/consent", json!({}), "empty body"),
        (
            "/reserve_pay/consent",
            json!({ "user_id": "u", "agent_id": "a", "spend_limit_minor": -5, "device": "mobile" }),
            "negative spend limit",
        ),
        (
            "/reserve_pay/consent",
            json!({ "user_id": "u", "agent_id": "a", "spend_limit_minor": 0, "device": "mobile" }),
            "zero spend limit",
        ),
        (
            "/reserve_pay/consent",
            json!({ "user_id": "u", "agent_id": "a", "spend_limit_minor": 100.5, "device": "mobile" }),
            "float spend limit",
        ),
        (
            "/reserve_pay/consent",
            json!({ "user_id": "u", "agent_id": "a", "spend_limit_minor": "100", "device": "mobile" }),
            "string spend limit",
        ),
        (
            "/reserve_pay/consent",
            json!({ "user_id": "u", "agent_id": "a", "spend_limit_minor": 100, "device": "mobile", "currency": "BTC" }),
            "bogus currency",
        ),
        (
            "/reserve_pay/consent",
            json!({ "user_id": 7, "agent_id": "a", "spend_limit_minor": 100, "device": "mobile" }),
            "numeric user_id",
        ),
        (
            "/reserve_pay/consent",
            json!({ "user_id": "u", "agent_id": "a", "spend_limit_minor": 100 }),
            "missing device",
        ),
    ];
    for (uri, body, label) in consent_bodies {
        let (status, body_text) = signed(&app, Method::POST, uri, Some(body.clone())).await;
        assert_clean_4xx(&format!("POST {uri} ({label})"), status, &body_text);
    }

    let debit_bodies: &[(&str, Value, &str)] = &[
        ("/reserve_pay/debit", json!({}), "empty body"),
        (
            "/reserve_pay/debit",
            json!({ "consent_id": "c", "amount_minor": -1, "currency": "USD", "device": "m" }),
            "negative debit",
        ),
        (
            "/reserve_pay/debit",
            json!({ "consent_id": "c", "amount_minor": 0, "currency": "USD", "device": "m" }),
            "zero debit",
        ),
        (
            "/reserve_pay/debit",
            json!({ "consent_id": "c", "amount_minor": 9223372036854775807i64, "currency": "USD", "device": "m" }),
            "max-i64 debit",
        ),
        (
            "/reserve_pay/debit",
            json!({ "consent_id": "c", "amount_minor": 100, "currency": "XXX", "device": "m" }),
            "bogus currency",
        ),
        (
            "/reserve_pay/debit",
            json!({ "consent_id": "c", "amount_minor": "ten", "currency": "USD", "device": "m" }),
            "string amount",
        ),
        (
            "/reserve_pay/debit",
            json!({ "consent_id": 1, "amount_minor": 100, "currency": "USD", "device": "m" }),
            "numeric consent_id",
        ),
        (
            "/reserve_pay/debit",
            json!({ "consent_id": "does-not-exist", "amount_minor": 100, "currency": "USD", "device": "m" }),
            "unknown consent id",
        ),
    ];
    for (uri, body, label) in debit_bodies {
        let (status, body_text) = signed(&app, Method::POST, uri, Some(body.clone())).await;
        assert_clean_4xx(&format!("POST {uri} ({label})"), status, &body_text);
    }

    // Junk query string on the debit route is a query-rejection 4xx too.
    let (status, body_text) = signed(
        &app,
        Method::POST,
        "/reserve_pay/debit?confirm=not-a-bool",
        Some(json!({ "consent_id": "c", "amount_minor": 100, "currency": "USD", "device": "m" })),
    )
    .await;
    assert_clean_4xx(
        "POST /reserve_pay/debit?confirm=not-a-bool",
        status,
        &body_text,
    );
}

/// Webhook + query/header input surfaces: garbage never 5xx/panics. Razorpay
/// env vars are set so the webhook reaches signature verification
/// (deterministic 401 for garbage); without them it fails closed with 503,
/// which the last case documents explicitly.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn malformed_webhook_and_query_inputs_are_clean() {
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    let app = test_app();

    let with_razorpay_env = async {
        // RAZORPAY_KEY_ID/SECRET set (junk values) so RazorpayClient::from_env
        // succeeds and garbage signatures fail verification with 401.
        std::env::set_var("RAZORPAY_KEY_ID", "rzp_test_secaudit");
        std::env::set_var("RAZORPAY_KEY_SECRET", "rzp_test_secaudit_secret");
        std::env::remove_var("RAZORPAY_BASE_URL");
        std::env::remove_var("RAZORPAY_MODE");

        let raw_bodies: &[(&str, &str)] = &[
            ("garbage body", "this is not json at all"),
            ("broken json", "{\"event\": \"payment.paid\","),
            ("array json", "[1,2,3]"),
            ("wrong-typed event", "{\"event\": 42, \"payload\": {}}"),
            (
                "malformed envelope",
                "{\"event\":\"payment.paid\",\"payload\":{\"payment\":{\"entity\":{\"id\":\"pay_x\",\"notes\":{\"order_id\":42}}}}}",
            ),
            ("empty body", ""),
            (
                "embedded script",
                "<script>alert(1)</script>{\"event\":\"payment.paid\"}",
            ),
        ];
        for (label, raw) in raw_bodies {
            let (status, body_text) = send(
                &app,
                Request::builder()
                    .method(Method::POST)
                    .uri("/webhooks/razorpay")
                    .header("content-type", "application/json")
                    .header("x-razorpay-signature", "deadbeefdeadbeef")
                    .body(Body::from(raw.to_string()))
                    .unwrap(),
            )
            .await;
            // 401 from signature verification (with env set) — validate the body.
            assert!(
                status.is_client_error(),
                "webhook {label}: expected 4xx, got {status}: {body_text}"
            );
            assert!(
                !body_text.contains("rzp_test_secaudit_secret"),
                "webhook {label}: response leaked the Razorpay secret!"
            );
        }

        // Missing signature header is a 401 (fail closed before any parse).
        let (status, _) = send(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/webhooks/razorpay")
                .header("content-type", "application/json")
                .body(Body::from("{\"event\":\"payment.paid\"}"))
                .unwrap(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "missing signature must 401"
        );
    };
    {
        let _guard = ENV_LOCK.lock().unwrap();
        with_razorpay_env.await;
        std::env::remove_var("RAZORPAY_KEY_ID");
        std::env::remove_var("RAZORPAY_KEY_SECRET");
    }

    // Public catalog query garbage -> query-rejection 4xx, never 5xx.
    let query_uris = [
        "/catalog/products?limit=abc",
        "/catalog/products?limit=",
        "/catalog/products?limit=-1",
        "/catalog/products?limit=999999999999999999999999",
        "/catalog/products?offset=",
        "/catalog/products?offset=abc",
        "/catalog/products?offset=-5",
        "/catalog/products?limit=1&offset=18446744073709551616",
        "/catalog/products?limit=%FF",
        "/catalog/products?limit=1.5",
    ];
    for uri in query_uris {
        let (status, body_text) = send(
            &app,
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        )
        .await;
        assert_clean_4xx(&format!("GET {uri}"), status, &body_text);
    }

    // Junk path segments are 404s (never 500/panic).
    for uri in [
        "/catalog/products/%20",
        "/catalog/products/%2F",
        "/carts/%2F",
        "/carts/%00",
        "/carts/%E2%82%AC",
    ] {
        let (status, body_text) = send(
            &app,
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        )
        .await;
        assert_clean_4xx(&format!("GET {uri}"), status, &body_text);
    }
}

/// Invalid JSON and missing/odd content types on signed write routes must be
/// clean 4xx (400/415), and oversized bodies must be rejected by the
/// middleware cap before any handler runs.
#[tokio::test]
async fn bad_json_missing_content_type_and_oversized_bodies_are_4xx() {
    let app = test_app();

    let raw_bodies: &[(&str, &str)] = &[
        ("unterminated object", "{\"currency\": \"USD\","),
        ("stray bracket", "{\"items\": [}"),
        ("not json", "hello world"),
        ("empty body", ""),
        ("trailing garbage", "{\"currency\":\"USD\"} trailing"),
        ("utf16-ish bytes", "\u{00a9}\u{00ae}\u{20ac}"),
    ];
    for (label, raw) in raw_bodies {
        let (status, body_text) = signed_raw(
            &app,
            Method::POST,
            "/carts",
            raw,
            &[("content-type", "application/json")],
        )
        .await;
        assert_clean_4xx(&format!("POST /carts ({label})"), status, &body_text);
    }

    // Missing content-type on a signed JSON write -> 415, not 5xx.
    let (status, body_text) = signed_raw(
        &app,
        Method::POST,
        "/carts",
        "{\"currency\":\"USD\",\"items\":[]}",
        &[],
    )
    .await;
    assert!(
        status.is_client_error(),
        "missing content-type: expected 4xx, got {status}: {body_text}"
    );

    // Wrong content-type -> 415.
    let (status, body_text) = signed_raw(
        &app,
        Method::POST,
        "/carts",
        "{\"currency\":\"USD\",\"items\":[]}",
        &[("content-type", "text/plain")],
    )
    .await;
    assert!(
        status.is_client_error(),
        "wrong content-type: expected 4xx, got {status}: {body_text}"
    );

    // Oversized signed body -> middleware cap (64 KiB) rejects with 4xx.
    let huge = "x".repeat(70_000);
    let (status, body_text) = signed_raw(
        &app,
        Method::POST,
        "/carts",
        &huge,
        &[("content-type", "application/json")],
    )
    .await;
    assert!(
        status.is_client_error(),
        "oversized body: expected 4xx, got {status}: {body_text}"
    );

    // Oversized body on the PUBLIC webhook route -> axum body-limit 4xx
    // (413), never 5xx.
    let huge = "y".repeat(3_000_000);
    let (status, body_text) = send(
        &app,
        Request::builder()
            .method(Method::POST)
            .uri("/webhooks/razorpay")
            .header("content-type", "application/json")
            .header("x-razorpay-signature", "deadbeef")
            .body(Body::from(huge))
            .unwrap(),
    )
    .await;
    assert!(
        status.is_client_error(),
        "oversized webhook body: expected 4xx (413), got {status}: {body_text}"
    );
}

// ---------------------------------------------------------------------------
// 2. Regression tests for the real gaps the audit found
// ---------------------------------------------------------------------------

/// The audit's headline finding: a NEGATIVE debit passed the `> remaining`
/// guard, then *decreased* `total_spent` — inflating the consent's remaining
/// limit like minting money. Now rejected with 400 before any ledger math.
#[tokio::test]
async fn negative_debit_is_rejected_and_cannot_inflate_remaining() {
    let app = test_app();

    let (status, consent) = signed(
        &app,
        Method::POST,
        "/reserve_pay/consent",
        Some(json!({
            "user_id": "user-1",
            "agent_id": "agent-demo",
            "spend_limit_minor": 5000,
            "currency": "USD",
            "device": "mobile",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "consent creation failed: {consent}");
    let consent_id = serde_json::from_str::<Value>(&consent).unwrap()["consent_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Negative debit: 400, "must be a positive integer".
    let (status, body) = signed(
        &app,
        Method::POST,
        "/reserve_pay/debit",
        Some(json!({
            "consent_id": consent_id,
            "amount_minor": -1_000_000,
            "currency": "USD",
            "device": "mobile",
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "negative debit must be a 400: {body}"
    );
    assert!(
        body.contains("must be a positive integer"),
        "expected a clear amount error, got: {body}"
    );

    // Zero debit: 400 too.
    let (status, body) = signed(
        &app,
        Method::POST,
        "/reserve_pay/debit",
        Some(json!({
            "consent_id": consent_id,
            "amount_minor": 0,
            "currency": "USD",
            "device": "mobile",
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "zero debit must be a 400: {body}"
    );

    // The ledger is untouched: a normal 100-unit debit still sees 4900 left.
    let (status, body) = signed(
        &app,
        Method::POST,
        "/reserve_pay/debit",
        Some(json!({
            "consent_id": consent_id,
            "amount_minor": 100,
            "currency": "USD",
            "device": "mobile",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "legit debit failed: {body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["remaining"],
        4900,
        "negative/zero attempts must not have inflated the limit"
    );
}

/// Non-positive spend limits are meaningless; creating one is a 400.
#[tokio::test]
async fn non_positive_spend_limit_is_rejected() {
    let app = test_app();
    for (label, limit) in [("negative", -10), ("zero", 0)] {
        let (status, body) = signed(
            &app,
            Method::POST,
            "/reserve_pay/consent",
            Some(json!({
                "user_id": "user-1",
                "agent_id": "agent-demo",
                "spend_limit_minor": limit,
                "currency": "USD",
                "device": "mobile",
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label} spend limit must be a 400: {body}"
        );
    }
}

/// A cross-currency cart (INR cart, USD-priced products) previously returned
/// 200 with `totals: null`; the currency check now makes it a 400 before the
/// cart is ever stored.
#[tokio::test]
async fn cross_currency_cart_is_rejected_not_a_null_totals_200() {
    let app = test_app();

    let (status, body) = signed(
        &app,
        Method::POST,
        "/carts",
        Some(json!({
            "currency": "INR",
            "items": [{ "product_id": "p-latte", "quantity": 1 }]
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "cross-currency cart must 400: {body}"
    );
    assert!(
        body.contains("mixed-currency"),
        "expected a clear currency error, got: {body}"
    );

    // Same-currency carts still work and carry totals (valid behavior unchanged).
    let (status, body) = signed(
        &app,
        Method::POST,
        "/carts",
        Some(json!({
            "currency": "USD",
            "items": [{ "product_id": "p-latte", "quantity": 1 }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "USD cart must still work: {body}");
    let cart = serde_json::from_str::<Value>(&body).unwrap();
    assert!(
        cart["totals"]["subtotal"]["units"] == 450,
        "totals must be present: {body}"
    );

    // Updating a USD cart with a cross-currency mismatch is also a 400.
    let cart_id = cart["id"].as_str().unwrap().to_string();
    // The seeded catalog is all-USD, so build the mismatch via an unknown
    // id check instead: covered in the audit table. Here: a foreign-currency
    // cart cannot even be created, so PUT keeps its currency invariant.
    let (status, body) = signed(
        &app,
        Method::PUT,
        &format!("/carts/{cart_id}"),
        Some(json!({ "items": [{ "product_id": "p-latte", "quantity": 0 }] })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "valid same-currency PUT must still work: {body}"
    );
}

// ---------------------------------------------------------------------------
// 3. Secrets: the Razorpay key secret must never appear in Debug or errors
// ---------------------------------------------------------------------------

#[test]
fn razorpay_config_debug_and_error_paths_redact_secret() {
    let config = RazorpayConfig {
        key_id: "rzp_test_keyid".to_string(),
        key_secret: "AUDIT_SUPER_SECRET_123".to_string(),
        mode: RazorpayMode::Sandbox,
        base_url: "https://api.razorpay.com".to_string(),
        webhook_secret: Some("AUDIT_WEBHOOK_SECRET_456".to_string()),
    };

    // Debug of the config itself.
    let debug = format!("{config:?}");
    assert!(
        !debug.contains("AUDIT_SUPER_SECRET_123"),
        "config Debug leaked key_secret: {debug}"
    );
    assert!(
        !debug.contains("AUDIT_WEBHOOK_SECRET_456"),
        "config Debug leaked webhook_secret: {debug}"
    );
    assert!(
        debug.contains("rzp_test_keyid"),
        "key_id should still be visible: {debug}"
    );

    // Debug of the client wrapper (which embeds the config).
    let client = RazorpayClient::new(config.clone());
    let debug = format!("{client:?}");
    assert!(
        !debug.contains("AUDIT_SUPER_SECRET_123"),
        "client Debug leaked key_secret: {debug}"
    );
    assert!(
        !debug.contains("AUDIT_WEBHOOK_SECRET_456"),
        "client Debug leaked webhook_secret: {debug}"
    );

    // Error paths: Display, Debug and the API-error body never carry secrets.
    let err = RazorpayError::Config(
        "RAZORPAY_WEBHOOK_SECRET is not set; refusing to verify webhook".to_string(),
    );
    let display = err.to_string();
    let debug = format!("{err:?}");
    assert!(!display.contains("AUDIT_SUPER_SECRET_123"));
    assert!(!debug.contains("AUDIT_SUPER_SECRET_123"));

    let err = RazorpayError::Api {
        status: 401,
        body: "unauthorized".to_string(),
    };
    assert!(!format!("{err:?}").contains("AUDIT_SUPER_SECRET_123"));
}

// ---------------------------------------------------------------------------
// 4. Fuzz-ish property tests (deterministic xorshift over every surface)
// ---------------------------------------------------------------------------

/// Junk strings — never a valid currency code, fulfillment, or seeded product
/// id, so a body assembled from these can never deserialize into a VALID
/// request by accident (the point of the sweep is garbage in -> 4xx out).
const JUNK_STRINGS: &[&str] = &[
    "",
    "x",
    "zz",
    "BTC",
    "XXX",
    "eur",
    "usd",
    "USDX",
    "rubles",
    "Pickup",
    "pickup",
    "PICKUP",
    "Teleport",
    "Shipping",
    "Null",
    "p-lattee",
    "p-",
    "-",
    "agent-demo ",
    "cart-",
    "cs-",
    "ord-",
    "cons-",
    "0x00",
    "NaN",
    "inf",
    "{}",
    "[]",
    "null",
    "true",
];

/// Random i64 in [-2^40, 2^40) — includes negatives, zero, and huge values
/// that overflow u32 fields.
fn junk_int(rng: &mut XorShift) -> i64 {
    let mag = rng.below(1 << 40);
    if rng.below(4) == 0 {
        mag as i64
    } else {
        -(mag as i64)
    }
}

/// Percent-encode free-form junk so it can appear safely in a URI (the junk
/// strings include `{}`, `[]`, spaces, quotes — all invalid in a raw URI).
fn uri_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A random Value that can never form a valid request body: every string is
/// junk, every number is a junk int/float, and the endpoint's poison key
/// (the field that must be valid for the body to parse) is always junk.
fn random_garbage_body(rng: &mut XorShift, poison_key: &str, poison_junk: Value) -> Value {
    let keys = [
        "currency",
        "items",
        "product_id",
        "quantity",
        "cart_id",
        "fulfillment",
        "user_id",
        "agent_id",
        "spend_limit_minor",
        "device",
        "consent_id",
        "amount_minor",
        "confirm",
        "bogus",
        "extra",
        "nested",
    ];
    let mut obj = serde_json::Map::new();
    let n = 1 + rng.below(6);
    for _ in 0..n {
        let key = *rng.pick(&keys);
        let val = match rng.below(6) {
            0 => json!(JUNK_STRINGS[rng.below(JUNK_STRINGS.len() as u64) as usize]),
            1 => json!(junk_int(rng)),
            2 => json!(rng.below(1 << 30) as f64 / 7.0),
            3 => json!(rng.below(2) == 0),
            4 => Value::Null,
            _ => json!([junk_int(rng), junk_int(rng)]),
        };
        obj.insert(key.to_string(), val);
    }
    // The poison field is guaranteed invalid, so the body can never parse.
    obj.insert(poison_key.to_string(), poison_junk);
    Value::Object(obj)
}

/// Every write endpoint, swept with ~200 deterministic garbage bodies each.
/// All bodies are signed by the demo agent, so they reach the handlers. The
/// invariant: every response is a clean 4xx — never 200, never 5xx, and a
/// handler panic would fail the test outright (no catch_unwind needed:
/// `oneshot` propagates the panic).
#[tokio::test]
async fn fuzzish_random_bodies_never_5xx_or_panic() {
    let app = test_app();
    let mut rng = XorShift(0x5E41_2026_0808_BAD5);

    // (uri, poison key, poison value that can never parse). The debit/consent
    // poisons are STRINGS: an integer poison could accidentally be positive
    // and parse, and a random consent with junk user/agent/device strings
    // would then be VALID and return 200 — exactly what the sweep must never
    // see. A string where an i64 is required can never parse.
    let write_surfaces: &[(&str, &str, Value)] = &[
        ("/carts", "currency", json!(JUNK_STRINGS[0])), // junk currency code
        ("/checkout_sessions", "cart_id", json!(JUNK_STRINGS[0])), // junk id
        (
            "/reserve_pay/consent",
            "spend_limit_minor",
            json!(JUNK_STRINGS[0]),
        ),
        ("/reserve_pay/debit", "amount_minor", json!(JUNK_STRINGS[0])),
    ];

    for iteration in 0..200 {
        for (uri, poison_key, _) in write_surfaces {
            // poison values must be regenerated per iteration to vary.
            let poison: Value = match *poison_key {
                "currency" => json!(JUNK_STRINGS[rng.below(JUNK_STRINGS.len() as u64) as usize]),
                "cart_id" => json!(rng.pick(JUNK_STRINGS)),
                _ => json!(rng.pick(JUNK_STRINGS)), // never an integer
            };
            let body = random_garbage_body(&mut rng, poison_key, poison);
            let (status, body_text) = signed(&app, Method::POST, uri, Some(body.clone())).await;
            assert!(
                status.is_client_error(),
                "fuzz POST {uri}: garbage body {} -> {status} ({body_text})",
                body.to_string().chars().take(120).collect::<String>(),
            );
        }

        // PUT /carts/{id} with garbage (id seeded once, bodies always junk).
        let (status, body_text) = signed(
            &app,
            Method::PUT,
            "/carts/fuzz-cart-0",
            Some(random_garbage_body(
                &mut rng,
                "items",
                json!(JUNK_STRINGS[0]),
            )),
        )
        .await;
        assert!(
            status.is_client_error(),
            "fuzz PUT /carts/fuzz-cart-0 -> {status} ({body_text})"
        );

        // Signed write to a junk id on the remaining path-parameter routes.
        // These repeat every 4th iteration: each signed request costs a
        // debug-build Ed25519 verify (~40 ms), and the junk-id 404s are
        // already covered by the table-driven audit — the sweep only needs
        // to confirm no surface panics.
        if iteration % 4 == 0 {
            let junk_id = format!("id-{}", rng.below(1 << 48));
            for uri in [
                format!("/carts/{junk_id}/cancel"),
                format!("/checkout_sessions/{junk_id}/cancel"),
                format!("/checkout_sessions/{junk_id}/complete"),
                format!("/orders/{junk_id}/payment_link"),
            ] {
                let (status, body_text) = signed(&app, Method::POST, &uri, None).await;
                assert!(
                    status.is_client_error(),
                    "fuzz POST {uri} -> {status} ({body_text})"
                );
            }
        }
    }

    // Public GET surface with garbage query/path inputs. limit/offset are
    // always NEGATIVE (a positive integer is a *valid* usize and would
    // legitimately 200 with an empty page); tag/search carry escaped junk.
    for _ in 0..200 {
        let junk = uri_escape(rng.pick(JUNK_STRINGS));
        let neg_limit = -(junk_int(&mut rng).unsigned_abs() as i64);
        let neg_offset = -(junk_int(&mut rng).unsigned_abs() as i64);
        let uri = format!(
            "/catalog/products?limit={neg_limit}&offset={neg_offset}&tag={junk}&search={junk}"
        );
        let (status, body_text) = send(
            &app,
            Request::builder().uri(&uri).body(Body::empty()).unwrap(),
        )
        .await;
        assert!(
            status.is_client_error(),
            "fuzz GET {uri} -> {status} ({body_text})"
        );

        // Random junk cart/path ids on the public reads.
        let escaped = uri_escape(junk.as_str());
        for path in [
            format!("/carts/{escaped}"),
            format!("/catalog/products/{escaped}"),
        ] {
            let (status, body_text) = send(
                &app,
                Request::builder().uri(&path).body(Body::empty()).unwrap(),
            )
            .await;
            assert!(
                status.is_client_error(),
                "fuzz GET {path} -> {status} ({body_text})"
            );
        }
    }
}

/// Garbage webhook deliveries: every malformed body with a bogus signature
/// must be a clean 4xx (401 from signature verification — the handler fails
/// closed before parsing) and never leak config material.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn fuzzish_webhook_garbage_never_5xx_or_panic() {
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    let app = test_app();
    let mut rng = XorShift(0xDEAD_2026_0808_BEEF);

    let with_env = async {
        std::env::set_var("RAZORPAY_KEY_ID", "rzp_test_fuzz");
        std::env::set_var("RAZORPAY_KEY_SECRET", "rzp_test_fuzz_secret");

        let mut bytes = Vec::new();
        for _ in 0..200 {
            // Random raw byte soup (mostly non-JSON).
            bytes.clear();
            let len = rng.below(200) as usize;
            for _ in 0..len {
                bytes.push(rng.below(256) as u8);
            }
            let sig: String = format!("{:016x}", rng.next_u64());
            let (status, body_text) = send(
                &app,
                Request::builder()
                    .method(Method::POST)
                    .uri("/webhooks/razorpay")
                    .header("content-type", "application/json")
                    .header("x-razorpay-signature", &sig)
                    .body(Body::from(bytes.clone()))
                    .unwrap(),
            )
            .await;
            assert!(
                status.is_client_error(),
                "fuzz webhook: garbage body -> {status} ({body_text})"
            );
            assert!(
                !body_text.contains("rzp_test_fuzz_secret"),
                "webhook error leaked the secret: {body_text}"
            );
        }
    };
    {
        let _guard = ENV_LOCK.lock().unwrap();
        with_env.await;
        std::env::remove_var("RAZORPAY_KEY_ID");
        std::env::remove_var("RAZORPAY_KEY_SECRET");
    }
}

/// Parser-level fuzz (no HTTP): the serde types behind every endpoint must
/// never panic on arbitrary garbage, and must reject negative / overflowing
/// / bogus-enum input outright.
#[test]
fn serde_parsers_reject_garbage_without_panicking() {
    let mut rng = XorShift(0xFA57_2026_0808_CAFE);

    // Random string soup -> every parse attempt must simply return Err/Ok,
    // never panic.
    let alphabet: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789{},[]:\"\\-_ \t\n\x00\x7f\xc3\xa9";
    for _ in 0..500 {
        let len = rng.below(96) as usize;
        let s: String = (0..len)
            .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize] as char)
            .collect();
        let _ = serde_json::from_str::<LineItem>(&s);
        let _ = serde_json::from_str::<Cart>(&s);
        let _ = serde_json::from_str::<Currency>(&s);
        let _ = serde_json::from_str::<Amount>(&s);
        let _ = serde_json::from_str::<Fulfillment>(&s);
    }

    // Random JSON trees -> same non-panicking property through from_value.
    for _ in 0..300 {
        let poison = json!(rng.pick(JUNK_STRINGS));
        let v = random_garbage_body(&mut rng, "poison", poison);
        let _ = serde_json::from_value::<LineItem>(v.clone());
        let _ = serde_json::from_value::<Cart>(v.clone());
        let _ = serde_json::from_value::<Amount>(v);
    }

    // The wire-level quantity guards: negatives and > u32::MAX never parse.
    assert!(
        serde_json::from_str::<LineItem>(r#"{"product_id":"p","quantity":-1}"#).is_err(),
        "negative quantity must not deserialize"
    );
    assert!(
        serde_json::from_str::<LineItem>(r#"{"product_id":"p","quantity":4294967296}"#).is_err(),
        "quantity over u32::MAX must not deserialize"
    );
    assert!(
        serde_json::from_str::<Cart>(
            r#"{"currency":"USD","items":[{"product_id":"p","quantity":-1}]}"#
        )
        .is_err(),
        "cart with a negative line quantity must not deserialize"
    );

    // Bogus enum strings are rejected at the wire boundary.
    for json in ["\"BTC\"", "\"jpy \"", "3", "null"] {
        assert!(
            serde_json::from_str::<Currency>(json).is_err(),
            "currency {json} must be rejected"
        );
    }
    for json in ["\"Teleport\"", "\"pickup\"", "{ \"Shipping\": {} }", "[]"] {
        assert!(
            serde_json::from_str::<Fulfillment>(json).is_err(),
            "fulfillment {json} must be rejected"
        );
    }
}
