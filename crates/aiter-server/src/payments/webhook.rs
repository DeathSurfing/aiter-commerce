//! Razorpay webhook processing (#20 verification, #21 reconciliation).

use std::collections::HashMap;

use aiter_core::amount::Currency;
use aiter_core::order::OrderEvent;
use aiter_core::store::{Store, StoreError};
use aiter_core::util::now;
use serde::Deserialize;

use crate::catalog::AppState;

use super::client::{RazorpayClient, RazorpayError};

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
    /// The delivered payment does not match the order total exactly (#69).
    AmountMismatch { expected: i64, got: i64 },
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
    reconcile_payment(st, order_id, &entity.id, &entity).await
}

/// Drive the order to its paid state and record the transaction id (#21),
/// binding the delivered payment to the order's exact total (#69).
///
/// Idempotent: an order that already carries a `payment_reference` is returned
/// as `AlreadyPaid` without touching state again.
async fn reconcile_payment(
    st: &AppState,
    order_id: &str,
    payment_id: &str,
    payment: &WebhookPaymentEntity,
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

    // ponytail: exact-match binding; partial-capture flows need a real
    // order-payment ledger.
    if payment.amount != order.totals.total.units()
        || !currency_matches(payment.currency.as_deref(), order.totals.total.currency)
    {
        return Err(WebhookError::AmountMismatch {
            expected: order.totals.total.units(),
            got: payment.amount,
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

/// Case-insensitive ISO-code comparison against the order currency. A missing
/// webhook currency (`#[serde(default)]`) is accepted: Razorpay always sends
/// it on real deliveries and the amount comparison alone still binds those.
fn currency_matches(reported: Option<&str>, expected: Currency) -> bool {
    reported.is_none_or(|code| code.eq_ignore_ascii_case(expected.code()))
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
    /// Amount actually paid, in minor units — bound against the order total (#69).
    amount: i64,
    /// ISO 4217 currency code of the payment (absent on some payloads).
    #[serde(default)]
    currency: Option<String>,
    /// Custom fields copied from the payment link/order; `order_id` is ours.
    #[serde(default)]
    notes: HashMap<String, String>,
}
