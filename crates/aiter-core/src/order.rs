//! Order model: the post-payment record of a checkout.
//!
//! An [`Order`] references the [`crate::checkout::CheckoutSession`] that
//! created it, carries its [`Totals`], a [`OrderStatus`], and an append-only
//! timeline of status changes. Status transitions are constrained to the
//! allowed edges (see [`order_can_transition`]).

use serde::{Deserialize, Serialize};

use crate::pricing::Totals;

/// Lifecycle of an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Placed,
    Confirmed,
    Shipped,
    Delivered,
    Cancelled,
}

impl OrderStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, OrderStatus::Delivered | OrderStatus::Cancelled)
    }
}

/// Driver events for the order state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderEvent {
    Confirm,
    Ship,
    Deliver,
    Cancel,
}

impl OrderEvent {
    fn target_status(&self) -> OrderStatus {
        match self {
            OrderEvent::Confirm => OrderStatus::Confirmed,
            OrderEvent::Ship => OrderStatus::Shipped,
            OrderEvent::Deliver => OrderStatus::Delivered,
            OrderEvent::Cancel => OrderStatus::Cancelled,
        }
    }
}

/// Errors from driving the order state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderError {
    IllegalTransition {
        from: OrderStatus,
        event: OrderEvent,
    },
}

/// Whether an event is legal to apply from a status. An event whose target
/// equals the current status is an idempotent retry (legal no-op).
pub fn order_can_transition(from: OrderStatus, event: OrderEvent) -> bool {
    let target = event.target_status();
    if from == target {
        return true; // idempotent retry
    }
    matches!(
        (from, event),
        (OrderStatus::Placed, OrderEvent::Confirm)
            | (OrderStatus::Placed, OrderEvent::Cancel)
            | (OrderStatus::Confirmed, OrderEvent::Ship)
            | (OrderStatus::Confirmed, OrderEvent::Cancel)
            | (OrderStatus::Shipped, OrderEvent::Deliver)
    )
}

/// One recorded status change on the order timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub to: OrderStatus,
    /// Unix seconds at which the change happened.
    pub at: i64,
}

/// A placed order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub checkout_session_id: String,
    pub totals: Totals,
    pub status: OrderStatus,
    pub timeline: Vec<TimelineEntry>,
}

impl Order {
    /// Create a new order in `Placed` status with an opening timeline entry.
    pub fn new(
        id: impl Into<String>,
        checkout_session_id: impl Into<String>,
        totals: Totals,
        now: i64,
    ) -> Self {
        Order {
            id: id.into(),
            checkout_session_id: checkout_session_id.into(),
            totals,
            status: OrderStatus::Placed,
            timeline: vec![TimelineEntry {
                to: OrderStatus::Placed,
                at: now,
            }],
        }
    }

    /// Apply an event, pushing a timeline entry on success. Illegal transitions
    /// are rejected; idempotent retries are no-ops (no duplicate timeline entry).
    pub fn apply_event(&mut self, event: OrderEvent, now: i64) -> Result<(), OrderError> {
        if !order_can_transition(self.status, event) {
            return Err(OrderError::IllegalTransition {
                from: self.status,
                event,
            });
        }
        if self.status == event.target_status() {
            return Ok(()); // idempotent: no new timeline entry
        }
        self.status = event.target_status();
        self.timeline.push(TimelineEntry {
            to: self.status,
            at: now,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::{Amount, Currency};

    fn totals() -> Totals {
        Totals {
            subtotal: Amount::new(100, Currency::USD),
            tax: Amount::new(10, Currency::USD),
            total: Amount::new(110, Currency::USD),
        }
    }

    #[test]
    fn new_order_is_placed_with_opening_timeline_entry() {
        let o = Order::new("o1", "cs1", totals(), 1000);
        assert_eq!(o.status, OrderStatus::Placed);
        assert_eq!(o.timeline.len(), 1);
        assert_eq!(o.timeline[0].to, OrderStatus::Placed);
        assert_eq!(o.timeline[0].at, 1000);
    }

    #[test]
    fn serde_round_trips_an_order() {
        let mut o = Order::new("o1", "cs1", totals(), 1000);
        o.apply_event(OrderEvent::Confirm, 1100).unwrap();
        let json = serde_json::to_string(&o).expect("serialize");
        let back: Order = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(o, back);
    }

    #[test]
    fn happy_path_placed_confirmed_shipped_delivered() {
        let mut o = Order::new("o1", "cs1", totals(), 1000);
        o.apply_event(OrderEvent::Confirm, 1100).unwrap();
        o.apply_event(OrderEvent::Ship, 1200).unwrap();
        o.apply_event(OrderEvent::Deliver, 1300).unwrap();
        assert_eq!(o.status, OrderStatus::Delivered);
        assert_eq!(o.timeline.len(), 4);
        assert!(o.status.is_terminal());
    }

    #[test]
    fn placed_can_be_cancelled() {
        let mut o = Order::new("o1", "cs1", totals(), 1000);
        o.apply_event(OrderEvent::Cancel, 1050).unwrap();
        assert_eq!(o.status, OrderStatus::Cancelled);
        assert!(o.status.is_terminal());
    }

    #[test]
    fn shipped_cannot_skip_to_cancelled_or_delivered_from_placed() {
        let mut o = Order::new("o1", "cs1", totals(), 1000);
        assert_eq!(
            o.apply_event(OrderEvent::Deliver, 1100),
            Err(OrderError::IllegalTransition {
                from: OrderStatus::Placed,
                event: OrderEvent::Deliver,
            })
        );
        assert_eq!(
            o.apply_event(OrderEvent::Ship, 1100),
            Err(OrderError::IllegalTransition {
                from: OrderStatus::Placed,
                event: OrderEvent::Ship,
            })
        );
        assert_eq!(o.status, OrderStatus::Placed);
    }

    #[test]
    fn delivered_is_terminal_and_rejects_events() {
        let mut o = Order::new("o1", "cs1", totals(), 1000);
        o.apply_event(OrderEvent::Confirm, 1100).unwrap();
        o.apply_event(OrderEvent::Ship, 1200).unwrap();
        o.apply_event(OrderEvent::Deliver, 1300).unwrap();
        assert_eq!(
            o.apply_event(OrderEvent::Cancel, 1400),
            Err(OrderError::IllegalTransition {
                from: OrderStatus::Delivered,
                event: OrderEvent::Cancel,
            })
        );
    }

    #[test]
    fn idempotent_retry_does_not_duplicate_timeline_entry() {
        let mut o = Order::new("o1", "cs1", totals(), 1000);
        o.apply_event(OrderEvent::Confirm, 1100).unwrap();
        let len_before = o.timeline.len();
        o.apply_event(OrderEvent::Confirm, 1101).unwrap(); // retry
        assert_eq!(o.status, OrderStatus::Confirmed);
        assert_eq!(o.timeline.len(), len_before);
    }
}
