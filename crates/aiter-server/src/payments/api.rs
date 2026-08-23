//! Payments HTTP surface (issues #19, #20, #21).

use std::sync::atomic::Ordering;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Value};

use aiter_core::store::Store;

use crate::catalog::AppState;
use crate::checkout::ApiError;

use super::client::{RazorpayClient, RazorpayError};
use super::webhook::{process_payment_webhook, WebhookError, WebhookOutcome};

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
    let client = RazorpayClient::for_state(&st)?;
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

    let client = RazorpayClient::for_state(&st).map_err(razorpay_error_response)?;
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
pub(super) fn webhook_error_response(err: WebhookError) -> (StatusCode, Json<Value>) {
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
        WebhookError::AmountMismatch { expected, got } => (
            StatusCode::CONFLICT,
            format!(
                "payment does not match the order total (amount + currency): \
                 expected {expected}, got {got}"
            ),
        ),
        WebhookError::Store(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("store error: {e:?}"),
        ),
    };
    (code, Json(json!({ "error": message })))
}
