//! Cart + checkout session REST surface (issues #13, #14).
//!
//! Provides the merchant-facing checkout flow on top of the core models:
//!
//! * `POST /carts` — create a cart (optional initial items).
//! * `GET /carts/{id}` — read a cart with live re-derived totals.
//! * `PUT /carts/{id}` — replace a cart's items (quantity `0` removes a line).
//! * `POST /carts/{id}/cancel` — cancel a cart (idempotent).
//! * `POST /checkout_sessions` — snapshot a cart into a checkout session.
//! * `POST /checkout_sessions/{id}/complete` — drive the state machine
//!   `Pending -> Ready -> Paid` and produce an [`Order`] (idempotent).
//! * `POST /checkout_sessions/{id}/cancel` — cancel a session (idempotent).
//!
//! All state lives in the shared [`AppState`] defined in [`crate::catalog`]:
//! the in-memory cart / session / order stores; unit prices resolve from the
//! served catalog (`AppState::price_of`), never a separate price book.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use aiter_core::amount::Currency;
use aiter_core::cart::{Cart, LineItem};
use aiter_core::checkout::{
    CheckoutError, CheckoutEvent, CheckoutSession, CheckoutStatus, Fulfillment,
};
use aiter_core::order::Order;
use aiter_core::pricing::{compute_totals, NoTax, PricingError, Totals};
use aiter_core::receipt::Receipt;
use aiter_core::store::{Store, StoreError};

use crate::auth::VerifiedAgent;
use crate::catalog::AppState;
use crate::payments::RazorpayError;

// ---------------------------------------------------------------------------
// Cart API
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub(crate) struct CreateCartRequest {
    #[serde(default)]
    currency: Option<Currency>,
    #[serde(default)]
    items: Vec<LineItem>,
}

/// A cart as seen by clients: the core [`Cart`] plus its id, cancelled flag,
/// and live re-derived totals (totals are not stored on the cart; they are
/// always computed at pricing time, see [`aiter_core::cart`]).
#[derive(Serialize)]
pub(crate) struct CartResponse {
    id: String,
    cancelled: bool,
    cart: Cart,
    totals: Option<Totals>,
}

/// Build a [`CartResponse`] with totals re-derived from the catalog.
fn cart_response(st: &AppState, id: &str, cart: &Cart, cancelled: bool) -> CartResponse {
    let totals = compute_totals(cart, |p| st.price_of(p), &NoTax).ok();
    CartResponse {
        id: id.to_string(),
        cancelled,
        cart: cart.clone(),
        totals,
    }
}

/// Reject any line item whose product id is not in the served catalog, or
/// whose price currency differs from the cart's currency (#36).
///
/// Both cart mutators call this up front so an unknown id or a cross-currency
/// line is a 400 with a clear error, never a 200 cart with `totals: null`
/// (a mixed-currency cart cannot be priced — subtotal math refuses to combine
/// currencies, which would otherwise surface only later as a null-totals
/// cart or a session-time 400).
fn validate_catalog_items(
    st: &AppState,
    currency: Currency,
    items: &[LineItem],
) -> Result<(), ApiError> {
    for item in items {
        match st.price_of(&item.product_id) {
            None => return Err(ApiError::UnknownProduct(item.product_id.clone())),
            Some(price) if price.currency() != currency => {
                return Err(ApiError::CurrencyMismatch {
                    product_id: item.product_id.clone(),
                    expected: currency,
                    got: price.currency(),
                });
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// `POST /carts` — create a cart, returning its id.
pub(crate) async fn create_cart(
    State(st): State<AppState>,
    Json(body): Json<CreateCartRequest>,
) -> Result<Json<CartResponse>, ApiError> {
    let currency = body.currency.unwrap_or(Currency::USD);
    validate_catalog_items(&st, currency, &body.items)?;
    let mut cart = Cart::new(currency);
    for item in body.items {
        cart.update(&item.product_id, item.quantity);
    }
    let id = st.gen_id("cart");
    st.carts
        .lock()
        .await
        .create(id.clone(), cart.clone())
        .map_err(ApiError::Store)?;
    Ok(Json(cart_response(&st, &id, &cart, false)))
}

/// `GET /carts/{id}` — read a cart.
pub(crate) async fn get_cart(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CartResponse>, ApiError> {
    let cart = st
        .carts
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or(ApiError::NotFound)?;
    let cancelled = st.cancelled_carts.lock().await.contains(&id);
    Ok(Json(cart_response(&st, &id, &cart, cancelled)))
}

#[derive(Deserialize)]
pub(crate) struct UpdateCartRequest {
    /// Full replacement body. Quantity 0 removes a line.
    items: Vec<LineItem>,
}

/// `PUT /carts/{id}` — replace the cart's items and re-derive totals.
pub(crate) async fn update_cart(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCartRequest>,
) -> Result<Json<CartResponse>, ApiError> {
    let mut stores = st.carts.lock().await;
    let cart = stores.get(&id).cloned().ok_or(ApiError::NotFound)?;
    // Reject unknown product ids / cross-currency lines up front (400),
    // never a null-totals cart.
    validate_catalog_items(&st, cart.currency, &body.items)?;
    let mut updated = Cart::new(cart.currency);
    for item in body.items {
        updated.update(&item.product_id, item.quantity);
    }
    stores
        .update(id.clone(), updated.clone())
        .map_err(ApiError::Store)?;
    let cancelled = st.cancelled_carts.lock().await.contains(&id);
    Ok(Json(cart_response(&st, &id, &updated, cancelled)))
}

/// `POST /carts/{id}/cancel` — cancel a cart. Idempotent: cancelling an
/// already-cancelled (or missing-but-registered) cart is a no-op.
pub(crate) async fn cancel_cart(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CartResponse>, ApiError> {
    let cart = st
        .carts
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or(ApiError::NotFound)?;
    st.cancelled_carts.lock().await.insert(id.clone());
    Ok(Json(cart_response(&st, &id, &cart, true)))
}

// ---------------------------------------------------------------------------
// Checkout session API
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct CreateSessionRequest {
    cart_id: String,
    #[serde(default)]
    fulfillment: Option<Fulfillment>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `POST /checkout_sessions` — snapshot a cart into a new checkout session.
pub(crate) async fn create_checkout_session(
    State(st): State<AppState>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<Json<CheckoutSession>, ApiError> {
    let cart = st
        .carts
        .lock()
        .await
        .get(&body.cart_id)
        .cloned()
        .ok_or(ApiError::NotFound)?;
    let totals = compute_totals(&cart, |p| st.price_of(p), &NoTax).map_err(ApiError::Pricing)?;
    let id = st.gen_id("cs");
    let session = CheckoutSession::new(
        id,
        cart.clone(),
        body.fulfillment.unwrap_or(Fulfillment::Pickup),
        now() + 3600,
        totals,
    );
    st.sessions
        .lock()
        .await
        .create(session.id.clone(), session.clone())
        .map_err(ApiError::Store)?;
    Ok(Json(session))
}

/// `POST /checkout_sessions/{id}/complete` — finalize totals and produce an
/// [Order]. Drives the checkout state machine Pending -> Ready -> Paid, then
/// creates the order in `Placed` status. Idempotent: completing an
/// already-completed session is a no-op that returns the existing order (no
/// double order is ever created).
///
/// Trust enforcement (issues #25–#27): over HTTP the request must carry a
/// valid agent signature (checked by [`crate::auth::require_signed`] before
/// this handler runs), the *same* verified agent is charged against its
/// registered spend cap (a `403` over-limit checkout leaves the session
/// untouched), and on success a [`Receipt`] is issued and appended to the
/// audit log exactly once. The MCP stdio binding (#28) calls this handler
/// directly without an agent context, so the agent argument is optional:
/// spend-limit charging and receipts apply to agent-signed completions only.
pub(crate) async fn complete_checkout(
    State(st): State<AppState>,
    agent: Option<Extension<VerifiedAgent>>,
    Path(id): Path<String>,
) -> Result<Json<Order>, ApiError> {
    let mut orders = st.orders.lock().await;
    let mut sessions = st.sessions.lock().await;

    // The order id is deterministically derived from the session id, so an
    // idempotent re-complete always resolves to the same order. Returning the
    // existing order here also guarantees the agent is charged and the audit
    // entry appended exactly once per completed checkout.
    let order_key = format!("ord-{id}");
    if let Some(order) = orders.get(&order_key) {
        return Ok(Json(order.clone())); // already completed -> no-op
    }

    let mut session = sessions.get(&id).cloned().ok_or(ApiError::NotFound)?;
    let order_total = session.totals.total;

    if session.status == CheckoutStatus::Paid {
        // Session already paid but no order registered (edge case): nothing to
        // reject — fall through and create it.
    } else if session.status.is_terminal() {
        return Err(ApiError::Conflict("session is not completable".into()));
    }

    // Spend-cap enforcement BEFORE any state change: an over-limit checkout is
    // rejected and the session is left exactly as it was (#26). Over HTTP the
    // agent is always present (require_signed inserts it); the MCP stdio
    // binding calls this handler directly with no agent context, so it skips
    // charging — spend limits/receipts are attributes of agent-signed
    // completions.
    if let Some(agent) = agent.as_ref() {
        let agent_id = &agent.0 .0; // Extension<VerifiedAgent> -> agent id
        st.charge_agent(agent_id, &order_total)
            .await
            .map_err(ApiError::SpendLimit)?;
    }

    // Legal path: Pending -> Ready -> Paid (ready skipped when already Paid).
    if session.status == CheckoutStatus::Pending {
        session
            .apply_event(CheckoutEvent::MarkReady)
            .map_err(ApiError::Checkout)?;
    }
    if session.status != CheckoutStatus::Paid {
        session
            .apply_event(CheckoutEvent::MarkPaid)
            .map_err(ApiError::Checkout)?;
    }

    let order = Order::new(order_key, id.clone(), session.totals, now());
    orders
        .create(order.id.clone(), order.clone())
        .map_err(ApiError::Store)?;
    sessions
        .update(id.clone(), session)
        .map_err(ApiError::Store)?;

    // Receipt + append-only audit entry (#27): who = verified agent, what =
    // order id, when = now, amount = order total. Issued exactly once per
    // completed agent-signed checkout; the MCP path has no agent identity to
    // attribute, so nothing is recorded there.
    if let Some(agent) = agent.as_ref() {
        let receipt = Receipt::new(
            format!("rcpt-{}", order.id),
            agent.0 .0.clone(),
            order.id.clone(),
            order_total,
            now(),
        );
        st.audit.lock().await.push(receipt);
    }

    Ok(Json(order))
}

/// `POST /checkout_sessions/{id}/cancel` — cancel a session. Idempotent:
/// cancelling an already-cancelled session is a no-op.
pub(crate) async fn cancel_checkout(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CheckoutSession>, ApiError> {
    let mut sessions = st.sessions.lock().await;
    let mut session = sessions.get(&id).cloned().ok_or(ApiError::NotFound)?;
    // Idempotent: already cancelled -> no-op (MarkCancelled on Cancelled is legal).
    if session.status != CheckoutStatus::Cancelled {
        session
            .apply_event(CheckoutEvent::MarkCancelled)
            .map_err(ApiError::Checkout)?;
        sessions
            .update(id.clone(), session.clone())
            .map_err(ApiError::Store)?;
    }
    Ok(Json(session))
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum ApiError {
    NotFound,
    Conflict(String),
    Store(StoreError),
    Checkout(CheckoutError),
    Pricing(PricingError),
    Razorpay(RazorpayError),
    /// A cart line references a product id that is not in the served catalog.
    UnknownProduct(String),
    /// A cart line's price currency differs from the cart's currency (#36).
    CurrencyMismatch {
        product_id: String,
        expected: Currency,
        got: Currency,
    },
    /// Spend-cap enforcement at checkout completion (issue #26).
    SpendLimit(String),
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError::Store(e)
    }
}

impl From<CheckoutError> for ApiError {
    fn from(e: CheckoutError) -> Self {
        ApiError::Checkout(e)
    }
}

impl From<PricingError> for ApiError {
    fn from(e: PricingError) -> Self {
        ApiError::Pricing(e)
    }
}

impl From<RazorpayError> for ApiError {
    fn from(e: RazorpayError) -> Self {
        ApiError::Razorpay(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, message): (StatusCode, String) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::Store(StoreError::NotFound) => {
                (StatusCode::NOT_FOUND, "not found".to_string())
            }
            ApiError::Store(StoreError::AlreadyExists) => {
                (StatusCode::CONFLICT, "already exists".to_string())
            }
            ApiError::Checkout(e) => (
                StatusCode::CONFLICT,
                format!("illegal checkout transition: {e:?}"),
            ),
            ApiError::Pricing(e) => (StatusCode::BAD_REQUEST, format!("unpriced item: {e:?}")),
            ApiError::Razorpay(e) => match &e {
                RazorpayError::Config(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg.clone()),
                RazorpayError::Signature(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
                _ => (StatusCode::BAD_GATEWAY, e.to_string()),
            },
            ApiError::UnknownProduct(id) => {
                (StatusCode::BAD_REQUEST, format!("unknown product: {id}"))
            }
            ApiError::CurrencyMismatch {
                product_id,
                expected,
                got,
            } => (
                StatusCode::BAD_REQUEST,
                format!(
                    "product {product_id} is priced in {} but the cart is in {}; \
                     refusing to price a mixed-currency cart",
                    got.code(),
                    expected.code()
                ),
            ),
            ApiError::SpendLimit(m) => (StatusCode::FORBIDDEN, m),
        };
        (code, Json(json!({ "error": message }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::seed_catalog;
    use aiter_core::amount::Amount;
    use aiter_core::order::OrderStatus;
    use aiter_core::signing::{AgentIdentity, AgentKeypair};
    use axum::body::{to_bytes, Body};
    use axum::http::{Method, Request};
    use axum::Router;
    use serde_json::Value;
    use std::sync::OnceLock;
    use tower::ServiceExt;

    /// The demo agent every test request is signed by. One keypair per test
    /// process; registered (with a generous spend cap) by each `app()`.
    fn demo_agent() -> &'static (AgentKeypair, AgentIdentity) {
        static AGENT: OnceLock<(AgentKeypair, AgentIdentity)> = OnceLock::new();
        AGENT.get_or_init(|| {
            let keypair = AgentKeypair::generate();
            let identity = keypair.identity("agent-1");
            (keypair, identity)
        })
    }

    /// Demo spend cap for the shared test agent (plenty for the existing flows).
    const DEMO_CAP: i64 = 1_000_000;

    /// Send a request signed by the demo agent (issue #25 middleware requires
    /// it on every write route). `uri` is the origin-form path, which is what
    /// the server reconstructs as the signed target URI.
    async fn call(
        app: &Router,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let (keypair, identity) = demo_agent();
        let has_body = body.is_some();
        let body_str = body.map(|b| b.to_string()).unwrap_or_default();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let signature = keypair.sign_request(
            &identity.id,
            method.as_str(),
            uri,
            body_str.as_bytes(),
            timestamp,
        );

        let mut builder = Request::builder().method(method).uri(uri);
        if has_body {
            builder = builder.header("content-type", "application/json");
        }
        builder = builder
            .header(super::super::auth::AGENT_ID_HEADER, &identity.id)
            .header(
                super::super::auth::SIGNATURE_HEADER,
                serde_json::to_string(&signature).unwrap(),
            );
        let req = builder.body(Body::from(body_str)).unwrap();

        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    async fn app() -> Router {
        let st = AppState::new(seed_catalog());
        let (_, identity) = demo_agent();
        st.register_agent(identity.clone(), Amount::new(DEMO_CAP, Currency::USD))
            .await;
        crate::router(st)
    }

    #[tokio::test]
    async fn cart_crud_and_idempotent_cancel() {
        let app = app().await;

        // Create a cart.
        let (status, created) = call(
            &app,
            Method::POST,
            "/carts",
            Some(json!({"currency": "USD", "items": [{"product_id": "p-latte", "quantity": 2}]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = created["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("cart-"));
        assert_eq!(created["totals"]["subtotal"]["units"], 900); // p-latte x2 = 900 units

        // Read it back.
        let (status, got) = call(&app, Method::GET, &format!("/carts/{id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got["cart"]["items"][0]["quantity"], 2);
        assert_eq!(got["cancelled"], false);

        // Update it; totals are re-derived.
        let (status, updated) = call(
            &app,
            Method::PUT,
            &format!("/carts/{id}"),
            Some(json!({"items": [{"product_id": "p-espresso", "quantity": 2}]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["totals"]["subtotal"]["units"], 600); // 2 * 300

        // Cancel (idempotent).
        let (status, cancelled) =
            call(&app, Method::POST, &format!("/carts/{id}/cancel"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cancelled["cancelled"], true);

        let (status, cancelled_again) =
            call(&app, Method::POST, &format!("/carts/{id}/cancel"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cancelled_again["cancelled"], true);
    }

    #[tokio::test]
    async fn missing_cart_is_404() {
        let app = app().await;
        let (status, _) = call(&app, Method::GET, "/carts/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_product_in_cart_is_rejected_with_400_and_totals_never_null() {
        let app = app().await;

        // Unknown product ids are rejected up front with a clear JSON error.
        let (status, body) = call(
            &app,
            Method::POST,
            "/carts",
            Some(json!({
                "currency": "USD",
                "items": [{"product_id": "ghost", "quantity": 1}]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("ghost"),
            "error should name the unknown product"
        );

        // Known ids always carry non-null totals.
        let (status, created) = call(
            &app,
            Method::POST,
            "/carts",
            Some(json!({"currency": "USD", "items": [{"product_id": "p-latte", "quantity": 2}]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(created["totals"]["subtotal"]["units"], 900);
        let id = created["id"].as_str().unwrap().to_string();

        // Replacing items with an unknown id is also a 400, not a null-totals 200.
        let (status, body) = call(
            &app,
            Method::PUT,
            &format!("/carts/{id}"),
            Some(json!({"items": [{"product_id": "ghost", "quantity": 1}]})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("ghost"));
    }

    #[tokio::test]
    async fn checkout_complete_produces_order_with_correct_totals() {
        let st = AppState::new(seed_catalog());
        let (_, identity) = demo_agent();
        st.register_agent(identity.clone(), Amount::new(DEMO_CAP, Currency::USD))
            .await;
        let app = crate::router(st.clone());

        // Cart: p-latte x 2 ($4.50 each) + p-coldbrew x 1 ($5.00) => subtotal 1400.
        let (_, cart) = call(
            &app,
            Method::POST,
            "/carts",
            Some(json!({
                "currency": "USD",
                "items": [
                    {"product_id": "p-latte", "quantity": 2},
                    {"product_id": "p-coldbrew", "quantity": 1}
                ]
            })),
        )
        .await;
        let cart_id = cart["id"].as_str().unwrap().to_string();

        // Create a checkout session — snapshots the cart with totals.
        let (status, session) = call(
            &app,
            Method::POST,
            "/checkout_sessions",
            Some(json!({"cart_id": cart_id})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let cs_id = session["id"].as_str().unwrap().to_string();
        assert_eq!(session["status"], "Pending");
        assert_eq!(session["totals"]["subtotal"]["units"], 1400);
        assert_eq!(session["totals"]["total"]["units"], 1400);
        // Session pinned the cart snapshot.
        assert_eq!(session["cart"]["items"].as_array().unwrap().len(), 2);

        // Complete -> Order in Placed status with correct totals.
        let (status, order) = call(
            &app,
            Method::POST,
            &format!("/checkout_sessions/{cs_id}/complete"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(order["status"], "Placed");
        assert_eq!(order["checkout_session_id"], cs_id);
        assert_eq!(order["totals"]["subtotal"]["units"], 1400);
        assert_eq!(order["totals"]["total"]["units"], 1400);

        // Completing again is idempotent: same order, no second order created.
        let (status, order2) = call(
            &app,
            Method::POST,
            &format!("/checkout_sessions/{cs_id}/complete"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(order2["id"], order["id"]);
        assert_eq!(st.orders.lock().await.len(), 1, "no double order");

        // Trust (issues #26/#27): the verified agent was charged exactly once
        // and exactly one receipt landed in the append-only audit log.
        assert_eq!(
            st.agents.lock().await.get("agent-1").unwrap().spent.units(),
            1400,
            "agent charged the order total once"
        );
        assert_eq!(
            st.audit.lock().await.len(),
            1,
            "one audit entry per checkout"
        );
        let audit = st.audit.lock().await;
        let entry = &audit.entries()[0];
        assert_eq!(entry.entry.agent_id, "agent-1");
        assert_eq!(entry.entry.order_id, order["id"].as_str().unwrap());
        assert_eq!(entry.entry.amount.units(), 1400);
    }

    #[tokio::test]
    async fn checkout_cancel_is_idempotent() {
        let app = app().await;
        let (_, cart) = call(
            &app,
            Method::POST,
            "/carts",
            Some(json!({"currency": "USD", "items": [{"product_id": "p-latte", "quantity": 1}]})),
        )
        .await;
        let cart_id = cart["id"].as_str().unwrap().to_string();

        let (_, session) = call(
            &app,
            Method::POST,
            "/checkout_sessions",
            Some(json!({"cart_id": cart_id})),
        )
        .await;
        let cs_id = session["id"].as_str().unwrap().to_string();

        let (status, cancelled) = call(
            &app,
            Method::POST,
            &format!("/checkout_sessions/{cs_id}/cancel"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cancelled["status"], "Cancelled");

        // Cancelling again is a no-op, still Cancelled.
        let (status, cancelled_again) = call(
            &app,
            Method::POST,
            &format!("/checkout_sessions/{cs_id}/cancel"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cancelled_again["status"], "Cancelled");
    }

    #[tokio::test]
    async fn completing_a_cancelled_session_is_rejected() {
        let app = app().await;
        let (_, cart) = call(
            &app,
            Method::POST,
            "/carts",
            Some(json!({"currency": "USD", "items": [{"product_id": "p-latte", "quantity": 1}]})),
        )
        .await;
        let cart_id = cart["id"].as_str().unwrap().to_string();
        let (_, session) = call(
            &app,
            Method::POST,
            "/checkout_sessions",
            Some(json!({"cart_id": cart_id})),
        )
        .await;
        let cs_id = session["id"].as_str().unwrap().to_string();

        call(
            &app,
            Method::POST,
            &format!("/checkout_sessions/{cs_id}/cancel"),
            None,
        )
        .await;
        let (status, _) = call(
            &app,
            Method::POST,
            &format!("/checkout_sessions/{cs_id}/complete"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[test]
    fn order_status_is_serde_friendly() {
        // Sanity: our returned Order enum serializes as a plain string.
        let json = serde_json::to_string(&OrderStatus::Placed).unwrap();
        assert_eq!(json, "\"Placed\"");
    }
}
