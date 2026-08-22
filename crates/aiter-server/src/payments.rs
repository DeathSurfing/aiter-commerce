//! Razorpay payments rail (issues #18, #23).
//!
//! Minimal Razorpay Orders API client + environment-driven config. Sandbox is
//! the default mode. Credentials come from `RAZORPAY_KEY_ID` /
//! `RAZORPAY_KEY_SECRET` and are **never** logged: every `Debug` impl and
//! error path in this module redacts the secret.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::Ordering;

use aiter_core::amount::Currency;
use aiter_core::order::OrderEvent;
use aiter_core::store::{Store, StoreError};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use hmac::{Hmac, Mac};
use reqwest::Request;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::catalog::AppState;
use crate::checkout::ApiError;

/// HMAC-SHA256, the Razorpay webhook signature algorithm (#20).
type HmacSha256 = Hmac<Sha256>;

/// Default Razorpay API base URL. Razorpay serves both sandbox (test keys,
/// `rzp_test_…`) and live (`rzp_live_…`) from the same host; the key pair
/// selects the mode. Override with `RAZORPAY_BASE_URL` for gateways/proxies.
const DEFAULT_BASE_URL: &str = "https://api.razorpay.com";

/// Payment mode: explicit sandbox vs live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RazorpayMode {
    Sandbox,
    Live,
}

impl RazorpayMode {
    fn from_env(value: &str) -> Result<Self, RazorpayError> {
        match value {
            "sandbox" => Ok(RazorpayMode::Sandbox),
            "live" => Ok(RazorpayMode::Live),
            other => Err(RazorpayError::Config(format!(
                "RAZORPAY_MODE must be 'sandbox' or 'live', got '{other}'"
            ))),
        }
    }
}

/// Typed Razorpay configuration, loaded from the environment (issue #23).
///
/// `Debug` redacts `key_secret`: the secret is never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct RazorpayConfig {
    pub key_id: String,
    pub key_secret: String,
    pub mode: RazorpayMode,
    pub base_url: String,
    /// `RAZORPAY_WEBHOOK_SECRET` — required to verify webhooks (#20). `None`
    /// means webhook processing fails closed.
    pub webhook_secret: Option<String>,
}

impl RazorpayConfig {
    /// Load from env vars. Requires `RAZORPAY_KEY_ID` + `RAZORPAY_KEY_SECRET`
    /// (clear error if missing), defaults `RAZORPAY_MODE` to `sandbox`, and
    /// honors a `RAZORPAY_BASE_URL` override. `RAZORPAY_WEBHOOK_SECRET` is
    /// optional here — webhook verification refuses to run without it.
    pub fn from_env() -> Result<Self, RazorpayError> {
        let key_id = read_env("RAZORPAY_KEY_ID")?;
        let key_secret = read_env("RAZORPAY_KEY_SECRET")?;
        let mode = match std::env::var("RAZORPAY_MODE") {
            Ok(value) => RazorpayMode::from_env(&value)?,
            Err(std::env::VarError::NotPresent) => RazorpayMode::Sandbox,
            Err(_) => {
                return Err(RazorpayError::Config(
                    "RAZORPAY_MODE must be a valid UTF-8 string".to_string(),
                ))
            }
        };
        let base_url = match std::env::var("RAZORPAY_BASE_URL") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => DEFAULT_BASE_URL.to_string(),
            Err(_) => {
                return Err(RazorpayError::Config(
                    "RAZORPAY_BASE_URL must be a valid UTF-8 string".to_string(),
                ))
            }
        };
        let webhook_secret = std::env::var("RAZORPAY_WEBHOOK_SECRET").ok();
        Ok(RazorpayConfig {
            key_id,
            key_secret,
            mode,
            base_url,
            webhook_secret,
        })
    }
}

impl fmt::Debug for RazorpayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RazorpayConfig")
            .field("key_id", &self.key_id)
            .field("key_secret", &"<redacted>")
            .field("mode", &self.mode)
            .field("base_url", &self.base_url)
            .field("webhook_secret", &"<redacted>")
            .finish()
    }
}

/// Minimal Razorpay Orders API client (issue #18).
///
/// Basic auth (`Key:Secret`), `POST /v1/orders`, sandbox base URL by default.
/// `Debug` never prints the secret.
pub struct RazorpayClient {
    config: RazorpayConfig,
    http: reqwest::Client,
}

impl RazorpayClient {
    pub fn new(config: RazorpayConfig) -> Self {
        RazorpayClient {
            config,
            http: reqwest::Client::new(),
        }
    }

    /// Build a client from the environment (see [`RazorpayConfig::from_env`]).
    pub fn from_env() -> Result<Self, RazorpayError> {
        Ok(RazorpayClient::new(RazorpayConfig::from_env()?))
    }

    /// Create an order and return its `order_id`.
    ///
    /// `amount_minor` is the amount in the currency's smallest unit (paise for
    /// INR). `receipt` is optional and omitted from the request when `None`.
    pub async fn create_order(
        &self,
        amount_minor: i64,
        currency: Currency,
        receipt: Option<&str>,
    ) -> Result<String, RazorpayError> {
        let request = self.build_order_request(amount_minor, currency, receipt)?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|err| RazorpayError::Http(err.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(512)
                .collect::<String>();
            return Err(RazorpayError::Api { status, body });
        }
        let order: OrderResponse = response
            .json()
            .await
            .map_err(|err| RazorpayError::Http(format!("failed to parse order response: {err}")))?;
        Ok(order.id)
    }

    /// Build the `POST /v1/orders` request (exposed for unit-testing the wire
    /// format without a network round-trip).
    fn build_order_request(
        &self,
        amount_minor: i64,
        currency: Currency,
        receipt: Option<&str>,
    ) -> Result<Request, RazorpayError> {
        let url = format!("{}/v1/orders", self.config.base_url.trim_end_matches('/'));
        let mut body = serde_json::Map::new();
        body.insert("amount".to_string(), json!(amount_minor));
        body.insert("currency".to_string(), json!(currency));
        if let Some(receipt) = receipt {
            body.insert("receipt".to_string(), json!(receipt));
        }
        self.http
            .post(url)
            .basic_auth(&self.config.key_id, Some(&self.config.key_secret))
            .json(&serde_json::Value::Object(body))
            .build()
            .map_err(|err| RazorpayError::Http(format!("failed to build request: {err}")))
    }

    /// Verify a Razorpay webhook signature (#20).
    ///
    /// Computes HMAC-SHA256 over the **raw** body bytes keyed with
    /// `RAZORPAY_WEBHOOK_SECRET` and compares it to the hex signature in the
    /// `x-razorpay-signature` header with a constant-time comparison. Fails
    /// closed (config error naming the env var) when no secret is configured
    /// — a webhook can never be processed without one.
    pub fn verify_webhook_signature(
        &self,
        body: &[u8],
        signature: &str,
    ) -> Result<(), RazorpayError> {
        let secret = self.config.webhook_secret.as_deref().ok_or_else(|| {
            RazorpayError::Config(
                "RAZORPAY_WEBHOOK_SECRET is not set; refusing to verify webhook".to_string(),
            )
        })?;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|err| RazorpayError::Http(format!("failed to init HMAC: {err}")))?;
        mac.update(body);
        let expected = to_hex(&mac.finalize().into_bytes());
        if constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
            Ok(())
        } else {
            Err(RazorpayError::Signature(
                "x-razorpay-signature mismatch".to_string(),
            ))
        }
    }

    /// Create a Razorpay payment link and return the `short_url` the buyer
    /// opens (issue #19).
    ///
    /// `amount_minor` is the amount in the currency's smallest unit (paise
    /// for INR). `order_id` is carried in `notes.order_id` so the `payment.paid`
    /// webhook can be reconciled back to our order (#21).
    pub async fn create_payment_link(
        &self,
        amount_minor: i64,
        currency: Currency,
        order_id: Option<&str>,
    ) -> Result<String, RazorpayError> {
        let request = self.build_payment_link_request(amount_minor, currency, order_id)?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|err| RazorpayError::Http(err.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(512)
                .collect::<String>();
            return Err(RazorpayError::Api { status, body });
        }
        let link: PaymentLinkResponse = response.json().await.map_err(|err| {
            RazorpayError::Http(format!("failed to parse payment link response: {err}"))
        })?;
        Ok(link.short_url)
    }

    /// Build the `POST /v1/payment_links` request (exposed for unit-testing
    /// the wire format without a network round-trip).
    fn build_payment_link_request(
        &self,
        amount_minor: i64,
        currency: Currency,
        order_id: Option<&str>,
    ) -> Result<Request, RazorpayError> {
        let url = format!(
            "{}/v1/payment_links",
            self.config.base_url.trim_end_matches('/')
        );
        let mut body = serde_json::Map::new();
        body.insert("amount".to_string(), json!(amount_minor));
        body.insert("currency".to_string(), json!(currency));
        body.insert("accept_partial".to_string(), json!(false));
        if let Some(order_id) = order_id {
            body.insert("notes".to_string(), json!({ "order_id": order_id }));
        }
        self.http
            .post(url)
            .basic_auth(&self.config.key_id, Some(&self.config.key_secret))
            .json(&serde_json::Value::Object(body))
            .build()
            .map_err(|err| RazorpayError::Http(format!("failed to build request: {err}")))
    }
}

impl fmt::Debug for RazorpayClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RazorpayClient")
            .field("config", &self.config)
            .finish()
    }
}

/// Razorpay order creation response — only the id is needed.
#[derive(Deserialize)]
struct OrderResponse {
    id: String,
}

/// Razorpay payment link response — only the `short_url` is needed (#19).
#[derive(Deserialize)]
struct PaymentLinkResponse {
    short_url: String,
}

/// Errors surfaced by the payments module. Never contains credentials: config
/// errors name only the offending env var, transport errors carry no auth
/// material, and API errors expose only the status + (truncated) body.
#[derive(Debug)]
pub enum RazorpayError {
    /// Environment configuration problem (missing/invalid variable).
    Config(String),
    /// HTTP/transport failure talking to Razorpay.
    Http(String),
    /// Razorpay returned a non-success status.
    Api { status: u16, body: String },
    /// Webhook signature verification failed (#20).
    Signature(String),
}

impl fmt::Display for RazorpayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RazorpayError::Config(msg) => write!(f, "razorpay config error: {msg}"),
            RazorpayError::Http(msg) => write!(f, "razorpay request failed: {msg}"),
            RazorpayError::Api { status, body } => {
                write!(f, "razorpay API error (HTTP {status}): {body}")
            }
            RazorpayError::Signature(msg) => {
                write!(f, "razorpay webhook signature verification failed: {msg}")
            }
        }
    }
}

impl std::error::Error for RazorpayError {}

fn read_env(name: &str) -> Result<String, RazorpayError> {
    std::env::var(name).map_err(|_| RazorpayError::Config(format!("{name} is required")))
}

/// Constant-time byte comparison: the loop runs over the full buffer and
/// never short-circuits on the first mismatching byte, so a timing side
/// channel cannot reveal how much of a webhook signature matched (#20).
/// Length mismatch returns `false` immediately (length is not secret for a
/// fixed-size HMAC-SHA256 hex digest).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Lowercase hex encoding (Razorpay webhook signatures are hex HMAC-SHA256).
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Webhook processing (#20 verification, #21 reconciliation)
// ---------------------------------------------------------------------------

/// Outcome of processing a verified webhook (#21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WebhookOutcome {
    /// `payment.paid` reconciled: the order transitioned to its paid state
    /// (closest legal transition — `Confirm` — there is no `Paid` order status
    /// in the core state machine) and recorded the payment id.
    Paid {
        order_id: String,
        payment_id: String,
    },
    /// `payment.paid` for an order that was already reconciled — idempotent
    /// no-op, no second status transition.
    AlreadyPaid {
        order_id: String,
        payment_id: String,
    },
    /// A signature-valid event that is not a reconciliation target (e.g.
    /// `payment.failed`, `invoice.paid`).
    Ignored,
}

/// Errors from webhook processing. Signature/config failures happen before
/// any state is touched.
#[derive(Debug)]
pub(crate) enum WebhookError {
    /// `RAZORPAY_WEBHOOK_SECRET` missing — verification fails closed.
    Config(String),
    /// `x-razorpay-signature` did not match the body.
    Signature(String),
    /// Body was not parseable as a Razorpay webhook envelope.
    InvalidJson(String),
    /// `payment.paid` without a `notes.order_id` linking it to our order.
    NoOrderReference,
    /// The order named in `notes.order_id` does not exist.
    OrderNotFound(String),
    /// The order exists but cannot transition (e.g. already cancelled).
    OrderState(String),
    /// Storage failure while persisting the transition.
    Store(StoreError),
}

/// Verify a webhook signature and reconcile it against order state (#20/#21).
///
/// This is the single entry point used by both the HTTP handler and tests:
/// on a valid signature, `payment.paid` events look up the order referenced in
/// `payment.entity.notes.order_id` and drive it to its paid state, recording
/// the payment id. Duplicate deliveries are no-ops.
pub(crate) async fn process_payment_webhook(
    st: &AppState,
    client: &RazorpayClient,
    body: &[u8],
    signature: &str,
) -> Result<WebhookOutcome, WebhookError> {
    client
        .verify_webhook_signature(body, signature)
        .map_err(|err| match err {
            RazorpayError::Config(msg) => WebhookError::Config(msg),
            RazorpayError::Signature(msg) => WebhookError::Signature(msg),
            other => WebhookError::Signature(other.to_string()),
        })?;

    let envelope: WebhookEnvelope =
        serde_json::from_slice(body).map_err(|err| WebhookError::InvalidJson(err.to_string()))?;

    if envelope.event != "payment.paid" {
        return Ok(WebhookOutcome::Ignored);
    }

    let entity = envelope.payload.payment.entity;
    let order_id = entity
        .notes
        .get("order_id")
        .ok_or(WebhookError::NoOrderReference)?;
    reconcile_payment(st, order_id, &entity.id).await
}

/// Drive the order to its paid state and record the transaction id (#21).
///
/// Idempotent: an order that already carries a `payment_reference` is returned
/// as `AlreadyPaid` without touching state again.
async fn reconcile_payment(
    st: &AppState,
    order_id: &str,
    payment_id: &str,
) -> Result<WebhookOutcome, WebhookError> {
    let mut orders = st.orders.lock().await;
    let mut order = orders
        .get(&order_id.to_string())
        .cloned()
        .ok_or_else(|| WebhookError::OrderNotFound(order_id.to_string()))?;

    if let Some(existing) = order.payment_reference.clone() {
        return Ok(WebhookOutcome::AlreadyPaid {
            order_id: order_id.to_string(),
            payment_id: existing,
        });
    }

    order
        .apply_event(OrderEvent::Confirm, now())
        .map_err(|err| WebhookError::OrderState(format!("{err:?}")))?;
    order.payment_reference = Some(payment_id.to_string());
    orders
        .update(order_id.to_string(), order)
        .map_err(WebhookError::Store)?;
    Ok(WebhookOutcome::Paid {
        order_id: order_id.to_string(),
        payment_id: payment_id.to_string(),
    })
}

/// Unix seconds — the timestamp convention used across the checkout flow.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Razorpay webhook envelope — only the fields we act on are modelled.
#[derive(Deserialize)]
struct WebhookEnvelope {
    event: String,
    payload: WebhookPayload,
}

#[derive(Deserialize)]
struct WebhookPayload {
    payment: WebhookPayment,
}

#[derive(Deserialize)]
struct WebhookPayment {
    entity: WebhookPaymentEntity,
}

#[derive(Deserialize)]
struct WebhookPaymentEntity {
    /// The payment id (`pay_…`) recorded as the order's transaction reference.
    id: String,
    /// Custom fields copied from the payment link/order; `order_id` is ours.
    #[serde(default)]
    notes: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// HTTP surface (issues #19, #20, #21)
// ---------------------------------------------------------------------------

/// `POST /orders/{id}/payment_link` — mint a Razorpay payment link for an
/// order (issue #19).
///
/// The order must exist (404 otherwise) and must not already be reconciled as
/// paid (409). The link amount is the order's final total in minor units and
/// the order id travels as `notes.order_id` so the `payment.paid` webhook can
/// reconcile the payment back to this order (#21).
pub(crate) async fn order_payment_link(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let order = st
        .orders
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or(ApiError::NotFound)?;

    // Observability (issue #33): span the payment-link mint (the Razorpay
    // call below is the slow, external part of this handler).
    let span = tracing::info_span!("payment_link_generation", order_id = %order.id);
    let _guard = span.enter();

    if order.payment_reference.is_some() {
        return Err(ApiError::Conflict(
            "order is already paid; refusing to mint a new payment link".to_string(),
        ));
    }
    let client = RazorpayClient::from_env()?;
    let short_url = client
        .create_payment_link(
            order.totals.total.units,
            order.totals.total.currency,
            Some(&order.id),
        )
        .await?;
    Ok(Json(json!({
        "order_id": order.id,
        "short_url": short_url,
    })))
}

/// `POST /webhooks/razorpay` — verify and reconcile a Razorpay webhook
/// (issues #20/#21).
///
/// The `x-razorpay-signature` header is verified against the **raw** body
/// (HMAC-SHA256, fails closed) before any state is touched. A `payment.paid`
/// event then drives the referenced order to its paid state and records the
/// payment id; duplicate deliveries are idempotent no-ops; everything else is
/// acknowledged and ignored.
pub(crate) async fn razorpay_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let signature = headers
        .get("x-razorpay-signature")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "missing x-razorpay-signature header" })),
            )
        })?;

    let client = RazorpayClient::from_env().map_err(razorpay_error_response)?;
    let outcome = process_payment_webhook(&st, &client, &body, signature)
        .await
        .map_err(webhook_error_response)?;

    let (payment_id, status) = match outcome {
        WebhookOutcome::Paid { payment_id, .. } => (Some(payment_id), "paid"),
        WebhookOutcome::AlreadyPaid { payment_id, .. } => (Some(payment_id), "already_paid"),
        WebhookOutcome::Ignored => (None, "ignored"),
    };

    // Observability (issue #33): only deliveries whose HMAC verified and
    // which were reconciled count here — signature/processing failures return
    // errors above.
    st.metrics.webhooks_verified.fetch_add(1, Ordering::Relaxed);
    let span = tracing::info_span!("webhook_verify_reconcile", outcome = %status);
    let _guard = span.enter();

    Ok(Json(json!({
        "received": true,
        "payment_id": payment_id,
        "status": status,
    })))
}

/// Map a Razorpay client failure (env config / transport / API) onto an HTTP
/// response. Configuration problems are 503s; everything else is a 502
/// Bad Gateway (never an auth failure — the secret is only redacted from
/// details).
fn razorpay_error_response(err: RazorpayError) -> (StatusCode, Json<Value>) {
    match err {
        RazorpayError::Config(msg) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": msg })),
        ),
        other => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": other.to_string() })),
        ),
    }
}

/// Map a webhook-processing failure onto an HTTP response: unverifiable
/// signatures (including a missing webhook secret — fail closed) are 401s,
/// malformed or unlinkable payloads are 400s, unreachable order states are
/// 409s, and storage failures are 500s.
fn webhook_error_response(err: WebhookError) -> (StatusCode, Json<Value>) {
    let (code, message): (StatusCode, String) = match err {
        WebhookError::Config(msg) | WebhookError::Signature(msg) => (StatusCode::UNAUTHORIZED, msg),
        WebhookError::InvalidJson(msg) => (
            StatusCode::BAD_REQUEST,
            format!("invalid webhook payload: {msg}"),
        ),
        WebhookError::NoOrderReference => (
            StatusCode::BAD_REQUEST,
            "payment.paid event missing notes.order_id".to_string(),
        ),
        WebhookError::OrderNotFound(id) => {
            (StatusCode::BAD_REQUEST, format!("unknown order: {id}"))
        }
        WebhookError::OrderState(msg) => (StatusCode::CONFLICT, msg),
        WebhookError::Store(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("store error: {e:?}"),
        ),
    };
    (code, Json(json!({ "error": message })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiter_core::amount::Currency;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex, OnceLock};

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
        let body: &[u8] = br#"{"event":"payment.failed","payload":{"payment":{"entity":{"id":"pay_x","notes":{"order_id":"ord-cs-0"}}}}}"#;
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
            br#"{"event":"payment.paid","payload":{"payment":{"entity":{"id":"pay_orphan"}}}}"#;
        let client = webhook_test_client();
        let signature = fixture_signature("whsec_test_secret", body);

        let err = process_payment_webhook(&st, &client, body, &signature)
            .await
            .unwrap_err();
        assert!(matches!(err, WebhookError::NoOrderReference));
    }

    // --- (g) HTTP surface: payment link + webhook routes -------------------

    use axum::body::to_bytes;
    use axum::http::Method;
    use tower::ServiceExt;

    /// The demo agent every signed test request is issued by (same pattern as
    /// the checkout tests): one keypair per test process, registered against
    /// a generous spend cap on the AppState before the router is built.
    fn demo_agent() -> &'static (
        aiter_core::signing::AgentKeypair,
        aiter_core::signing::AgentIdentity,
    ) {
        static AGENT: OnceLock<(
            aiter_core::signing::AgentKeypair,
            aiter_core::signing::AgentIdentity,
        )> = OnceLock::new();
        AGENT.get_or_init(|| {
            let keypair = aiter_core::signing::AgentKeypair::generate();
            let identity = keypair.identity("agent-1");
            (keypair, identity)
        })
    }

    /// Register the demo agent on `st` — require_signed 403s unregistered
    /// agents before any handler runs.
    async fn register_demo_agent(st: &AppState) {
        let (_, identity) = demo_agent();
        st.register_agent(identity.clone(), Amount::new(1_000_000, Currency::USD))
            .await;
    }

    /// Drive a write route with a request signed by the demo agent (the
    /// require_signed middleware demands it on protected routes like
    /// `/orders/{id}/payment_link`). Returns (status, JSON body or null).
    async fn signed_call(
        app: &Router,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let (keypair, identity) = demo_agent();
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
        if !body_str.is_empty() {
            builder = builder.header("content-type", "application/json");
        }
        builder = builder
            .header(crate::auth::AGENT_ID_HEADER, &identity.id)
            .header(
                crate::auth::SIGNATURE_HEADER,
                serde_json::to_string(&signature).unwrap(),
            );
        let req = builder.body(Body::from(body_str)).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// Set env vars for the duration of an async block. Same serialization
    /// discipline as `with_env`; tokio `current_thread` runtimes never wait on
    /// each other, so holding the lock across `.await` points is safe.
    #[allow(clippy::await_holding_lock)]
    async fn with_env_async<F: std::future::Future<Output = ()>>(
        vars: &[(&str, Option<&str>)],
        f: F,
    ) {
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
    async fn call(
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
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

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
                register_demo_agent(&st).await;
                let app = crate::router(st.clone());

                // 1. Checkout flow -> order in Placed status (write routes
                // require an agent signature, see trust.md / lib.rs docs).
                let (_, cart) = signed_call(
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

                let (_, session) = signed_call(
                    &app,
                    Method::POST,
                    "/checkout_sessions",
                    Some(json!({ "cart_id": cart_id })),
                )
                .await;
                let cs_id = session["id"].as_str().unwrap().to_string();

                let (status, order) = signed_call(
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
                let (status, link) = signed_call(
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

                // 3. payment.paid webhook with a fixture signature.
                let body = format!(
                    r#"{{"event":"payment.paid","payload":{{"payment":{{"entity":{{"id":"pay_int_1","notes":{{"order_id":"{order_id}"}}}}}}}}}}"#
                );
                let signature = fixture_signature("whsec_test_secret", body.as_bytes());
                let (status, receipt) = call(
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
                let (status, _) = call(
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
                assert_eq!(order.timeline.len(), 2, "duplicate webhook: no double transition");

                // 5. A bogus signature is rejected with 401.
                let (status, _) = call(
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
        register_demo_agent(&st).await;
        seed_order(&st, "ord-cs-0").await;
        let app = crate::router(st.clone());

        // Unknown order -> 404 (signed: the route sits behind require_signed).
        let (status, _) = signed_call(&app, Method::POST, "/orders/nope/payment_link", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // A reconciled (Confirmed, i.e. already paid) order -> 409, no new link.
        {
            let mut orders = st.orders.lock().await;
            let mut o = orders.get(&"ord-cs-0".to_string()).cloned().unwrap();
            o.payment_reference = Some("pay_already".to_string());
            orders.update("ord-cs-0".to_string(), o).unwrap();
        }
        let (status, _) =
            signed_call(&app, Method::POST, "/orders/ord-cs-0/payment_link", None).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
}
