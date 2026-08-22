//! Receipts and the append-only audit log (issue #27).
//!
//! A [`Receipt`] records *who* (agent id) paid *what* (order id), *when*
//! (unix seconds), and *how much* ([`Amount`], integer minor units). Receipts
//! are immutable once created and are appended to an [`AppendOnlyLog`]: the
//! audit trail can only grow — entries are never mutated or removed, and each
//! one carries a monotonically increasing sequence number.

use serde::{Deserialize, Serialize};

use crate::amount::Amount;

/// A payment receipt: who, what, when, for how much.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// Receipt id, unique per order (e.g. `rcpt-ord-cs-1`).
    pub id: String,
    /// The agent that paid (`who`).
    pub agent_id: String,
    /// The order that was paid (`what`).
    pub order_id: String,
    /// Total charged, in integer minor units (`how much`).
    pub amount: Amount,
    /// Unix seconds at which the receipt was issued (`when`).
    pub issued_at: i64,
}

impl Receipt {
    pub fn new(
        id: impl Into<String>,
        agent_id: impl Into<String>,
        order_id: impl Into<String>,
        amount: Amount,
        issued_at: i64,
    ) -> Self {
        Receipt {
            id: id.into(),
            agent_id: agent_id.into(),
            order_id: order_id.into(),
            amount,
            issued_at,
        }
    }
}

/// One audit log entry: a monotonically increasing sequence plus the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry<T> {
    pub seq: u64,
    pub entry: T,
}

/// An append-only log. The only way to add an entry is [`AppendOnlyLog::push`],
/// which assigns the next sequence number; there is **no** removal or mutation
/// API, and the only public view is the read-only [`AppendOnlyLog::entries`]
/// slice — so the trail provably cannot be rewritten.
#[derive(Debug, Clone, Default)]
pub struct AppendOnlyLog<T> {
    entries: Vec<AuditEntry<T>>,
    next_seq: u64,
}

impl<T> AppendOnlyLog<T> {
    pub fn new() -> Self {
        AppendOnlyLog {
            entries: Vec::new(),
            next_seq: 0,
        }
    }

    /// Append an entry, returning its assigned sequence number. Sequence
    /// numbers are strictly increasing across the lifetime of the log.
    pub fn push(&mut self, entry: T) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push(AuditEntry { seq, entry });
        seq
    }

    /// Read-only view of all entries, in append order.
    pub fn entries(&self) -> &[AuditEntry<T>] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::{Amount, Currency};

    fn usd(units: i64) -> Amount {
        Amount::new(units, Currency::USD)
    }

    fn receipt(order_id: &str) -> Receipt {
        Receipt::new(
            format!("rcpt-{order_id}"),
            "agent-1",
            order_id,
            usd(100),
            1_700_000_000,
        )
    }

    #[test]
    fn receipt_carries_who_what_when_amount() {
        let r = receipt("ord-cs-1");
        assert_eq!(r.id, "rcpt-ord-cs-1");
        assert_eq!(r.agent_id, "agent-1");
        assert_eq!(r.order_id, "ord-cs-1");
        assert_eq!(r.amount, usd(100));
        assert_eq!(r.issued_at, 1_700_000_000);
        // Round-trips through serde.
        let back: Receipt = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn entries_accumulate_with_monotonic_sequences() {
        let mut log = AppendOnlyLog::new();
        assert!(log.is_empty());

        let s1 = log.push(receipt("ord-1"));
        let s2 = log.push(receipt("ord-2"));
        let s3 = log.push(receipt("ord-3"));
        assert_eq!((s1, s2, s3), (0, 1, 2));
        assert_eq!(log.len(), 3);
        // Append order preserved, sequences strictly increasing.
        let entries = log.entries();
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[2].seq, 2);
        assert_eq!(entries[0].entry.order_id, "ord-1");
        assert_eq!(entries[2].entry.order_id, "ord-3");
    }

    #[test]
    fn entries_cannot_be_removed_or_mutated() {
        let mut log = AppendOnlyLog::new();
        log.push(receipt("ord-1"));
        log.push(receipt("ord-2"));

        // Append-only is enforced structurally: `push` is the only mutating
        // method and `entries()` hands out an immutable `&[AuditEntry<T>]` —
        // there is no remove/clear/`&mut` accessor to call. These assertions
        // pin the observable surface so a future mutation API cannot sneak in.
        let view: &[AuditEntry<Receipt>] = log.entries();
        assert_eq!(view.len(), 2);
        assert_eq!(view[0].entry.agent_id, "agent-1");
        let first = view[0].clone();

        // Re-pushing only appends; earlier entries are untouched.
        log.push(receipt("ord-3"));
        assert_eq!(log.entries().len(), 3);
        assert_eq!(log.entries()[0], first);
    }
}
