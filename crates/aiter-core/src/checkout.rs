//! Checkout session: a snapshot of a cart held for payment.
//!
//! A [`CheckoutSession`] pins the cart (and its computed [`Totals`]), the
//! currency, the chosen [`Fulfillment`], an expiry, and a [`CheckoutStatus`].
//! Status changes are driven as an explicitly-tested state machine (see
//! [`can_transition`]) that also honours **idempotency**: re-applying the event
//! that produced the current status is a legal no-op, so a retried "mark paid"
//! can never double-effect.

use serde::{Deserialize, Serialize};

use crate::amount::Currency;
use crate::cart::Cart;
use crate::pricing::Totals;

/// Lifecycle of a checkout session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckoutStatus {
    Pending,
    Ready,
    Paid,
    Cancelled,
    Failed,
}

impl CheckoutStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            CheckoutStatus::Paid | CheckoutStatus::Cancelled | CheckoutStatus::Failed
        )
    }
}

/// Shipment / collection / digital delivery choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fulfillment {
    Shipping { address: String },
    Pickup,
    Digital,
}

/// Driver events for the checkout state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutEvent {
    MarkReady,
    MarkPaid,
    MarkCancelled,
    MarkFailed,
}

impl CheckoutEvent {
    fn target_status(&self) -> CheckoutStatus {
        match self {
            CheckoutEvent::MarkReady => CheckoutStatus::Ready,
            CheckoutEvent::MarkPaid => CheckoutStatus::Paid,
            CheckoutEvent::MarkCancelled => CheckoutStatus::Cancelled,
            CheckoutEvent::MarkFailed => CheckoutStatus::Failed,
        }
    }
}

/// Errors from driving the checkout state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutError {
    IllegalTransition {
        from: CheckoutStatus,
        event: CheckoutEvent,
    },
}

/// Whether an event is legal to apply from a status.
///
/// An event whose target equals the current status counts as an *idempotent
/// retry* — legal, and a no-op when applied. Any other edge not listed is
/// rejected.
pub fn can_transition(from: CheckoutStatus, event: CheckoutEvent) -> bool {
    let target = event.target_status();
    if from == target {
        return true; // idempotent retry
    }
    matches!(
        (from, event),
        (CheckoutStatus::Pending, CheckoutEvent::MarkReady)
            | (CheckoutStatus::Pending, CheckoutEvent::MarkCancelled)
            | (CheckoutStatus::Pending, CheckoutEvent::MarkFailed)
            | (CheckoutStatus::Ready, CheckoutEvent::MarkPaid)
            | (CheckoutStatus::Ready, CheckoutEvent::MarkCancelled)
            | (CheckoutStatus::Ready, CheckoutEvent::MarkFailed)
    )
}

/// A checkout session ready to be paid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutSession {
    pub id: String,
    /// Snapshot of the cart being checked out.
    pub cart: Cart,
    pub currency: Currency,
    pub fulfillment: Fulfillment,
    pub status: CheckoutStatus,
    /// Unix seconds at which the session expires.
    pub expires_at: i64,
    pub totals: Totals,
}

impl CheckoutSession {
    pub fn new(
        id: impl Into<String>,
        cart: Cart,
        fulfillment: Fulfillment,
        expires_at: i64,
        totals: Totals,
    ) -> Self {
        CheckoutSession {
            id: id.into(),
            currency: cart.currency(),
            cart,
            fulfillment,
            status: CheckoutStatus::Pending,
            expires_at,
            totals,
        }
    }

    /// Apply an event. Illegal transitions are rejected; idempotent retries
    /// (event whose target is already the status) are no-ops.
    pub fn apply_event(&mut self, event: CheckoutEvent) -> Result<(), CheckoutError> {
        if !can_transition(self.status, event) {
            return Err(CheckoutError::IllegalTransition {
                from: self.status,
                event,
            });
        }
        self.status = event.target_status();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::{Amount, Currency};
    use crate::cart::Cart;
    use crate::pricing::Totals;

    fn session() -> CheckoutSession {
        let totals = Totals {
            subtotal: Amount::new(100, Currency::USD),
            tax: Amount::zero(Currency::USD),
            total: Amount::new(100, Currency::USD),
        };
        CheckoutSession::new(
            "cs1",
            Cart::new(Currency::USD),
            Fulfillment::Pickup,
            1_800_000_000,
            totals,
        )
    }

    #[test]
    fn session_starts_pending_and_round_trips() {
        let s = session();
        assert_eq!(s.status, CheckoutStatus::Pending);
        assert_eq!(s.currency, Currency::USD);
        let json = serde_json::to_string(&s).expect("serialize");
        let back: CheckoutSession = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }

    #[test]
    fn legal_transition_pending_to_ready() {
        let mut s = session();
        s.apply_event(CheckoutEvent::MarkReady).unwrap();
        assert_eq!(s.status, CheckoutStatus::Ready);
    }

    #[test]
    fn legal_transition_ready_to_paid() {
        let mut s = session();
        s.apply_event(CheckoutEvent::MarkReady).unwrap();
        s.apply_event(CheckoutEvent::MarkPaid).unwrap();
        assert_eq!(s.status, CheckoutStatus::Paid);
    }

    #[test]
    fn terminal_statuses_reject_further_events() {
        let mut s = session();
        s.apply_event(CheckoutEvent::MarkReady).unwrap();
        s.apply_event(CheckoutEvent::MarkPaid).unwrap();
        assert_eq!(
            s.apply_event(CheckoutEvent::MarkFailed),
            Err(CheckoutError::IllegalTransition {
                from: CheckoutStatus::Paid,
                event: CheckoutEvent::MarkFailed,
            })
        );
        assert_eq!(s.status, CheckoutStatus::Paid);
    }

    #[test]
    fn paid_cannot_be_reached_directly_from_pending() {
        let mut s = session();
        assert_eq!(
            s.apply_event(CheckoutEvent::MarkPaid),
            Err(CheckoutError::IllegalTransition {
                from: CheckoutStatus::Pending,
                event: CheckoutEvent::MarkPaid,
            })
        );
        assert_eq!(s.status, CheckoutStatus::Pending);
    }

    #[test]
    fn cancelled_cannot_be_revived() {
        let mut s = session();
        s.apply_event(CheckoutEvent::MarkCancelled).unwrap();
        assert_eq!(
            s.apply_event(CheckoutEvent::MarkReady),
            Err(CheckoutError::IllegalTransition {
                from: CheckoutStatus::Cancelled,
                event: CheckoutEvent::MarkReady,
            })
        );
    }

    #[test]
    fn idempotent_retry_is_a_no_op() {
        let mut s = session();
        s.apply_event(CheckoutEvent::MarkReady).unwrap();
        // Re-applying MarkReady: same target as current -> Ok, no double effect.
        s.apply_event(CheckoutEvent::MarkReady).unwrap();
        assert_eq!(s.status, CheckoutStatus::Ready);
    }

    #[test]
    fn idempotent_paid_then_ready_retry_keeps_paid() {
        let mut s = session();
        s.apply_event(CheckoutEvent::MarkReady).unwrap();
        s.apply_event(CheckoutEvent::MarkPaid).unwrap();
        // A retried MarkPaid is a no-op and does not regress status.
        s.apply_event(CheckoutEvent::MarkPaid).unwrap();
        assert_eq!(s.status, CheckoutStatus::Paid);
    }

    #[test]
    fn user_visible_statuses_are_serializable() {
        for status in [
            CheckoutStatus::Pending,
            CheckoutStatus::Ready,
            CheckoutStatus::Paid,
            CheckoutStatus::Cancelled,
            CheckoutStatus::Failed,
        ] {
            let json = serde_json::to_string(&status).expect("serialize");
            let back: CheckoutStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(status, back);
        }
    }
}
