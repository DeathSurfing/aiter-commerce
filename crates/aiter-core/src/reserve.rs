//! UPI Reserve Pay consent ledger (issue #22).
//!
//! NPCI Single-Block Mandate Debit (SBMD) model, mirrored by Razorpay's UPI
//! Reserve Pay: the user consents **once** — a [`Consent`] record carrying a
//! spending limit and the device it was granted on — after which an authorised
//! agent may debit against the limit without per-transaction authentication.
//! Enforcement lives at the server layer ([`aiter_server::reserve`]): limit
//! checks happen *before* any debit, and a debit from a device that differs
//! from the consenting device requires an explicit re-auth confirmation.
//!
//! # Gating: Razorpay early-access sandbox simulation
//!
//! UPI Reserve Pay is a Razorpay **early-access** product. This module and the
//! `/reserve_pay/*` routes implement the consent + agent-debit ledger as a
//! **local sandbox simulation only**: no Razorpay calls, no PSP mandate API,
//! no settlement — money never leaves the in-memory store. Do not wire this to
//! live funds until Razorpay grants access and the debit path is connected to
//! the real Reserve Pay mandate API.

use serde::{Deserialize, Serialize};

use crate::amount::Amount;

/// Lifecycle of a [`Consent`]. Debits are only allowed while `Active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsentStatus {
    Active,
    Revoked,
}

/// A one-time payment mandate: user consents once, an agent may debit against
/// `spend_limit` without re-authentication while the consent is `Active`.
/// Full serialization round-trips over the wire (and through the audit trail).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consent {
    pub consent_id: String,
    pub user_id: String,
    pub agent_id: String,
    /// Ceiling the agent may draw against, in integer minor units.
    pub spend_limit: Amount,
    /// Minor units debited so far against this consent.
    pub total_spent: Amount,
    /// Device the consent was granted on (e.g. `"mobile"` / `"pc"`);
    /// agent debits from a different device require re-auth confirmation.
    pub device: String,
    pub status: ConsentStatus,
    /// Unix timestamp (seconds) of consent capture.
    pub created_at: i64,
}

impl Consent {
    /// A fresh, `Active` consent with nothing spent yet.
    pub fn new(
        consent_id: String,
        user_id: String,
        agent_id: String,
        spend_limit: Amount,
        device: String,
        created_at: i64,
    ) -> Self {
        Consent {
            consent_id,
            user_id,
            agent_id,
            spend_limit,
            total_spent: Amount::zero(spend_limit.currency()),
            device,
            status: ConsentStatus::Active,
            created_at,
        }
    }

    /// Minor units still drawable: `spend_limit - total_spent`.
    pub fn remaining(&self) -> i64 {
        self.spend_limit.units() - self.total_spent.units()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_serde_round_trip() {
        let consent = Consent::new(
            "cons-0".to_string(),
            "user-1".to_string(),
            "agent-1".to_string(),
            Amount::new(10_000, crate::amount::Currency::USD),
            "mobile".to_string(),
            1_700_000_000,
        );
        let wire = serde_json::to_string(&consent).unwrap();
        let back: Consent = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, consent);
        // Field-level spot checks so the wire shape is pinned too.
        let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(v["consent_id"], "cons-0");
        assert_eq!(v["spend_limit"]["units"], 10_000);
        assert_eq!(v["spend_limit"]["currency"], "USD");
        assert_eq!(v["status"], "Active");
        assert_eq!(v["total_spent"]["units"], 0);
    }

    #[test]
    fn fresh_consent_is_active_with_full_limit_remaining() {
        let consent = Consent::new(
            "cons-1".to_string(),
            "user-1".to_string(),
            "agent-1".to_string(),
            Amount::new(5_000, crate::amount::Currency::USD),
            "pc".to_string(),
            0,
        );
        assert_eq!(consent.status, ConsentStatus::Active);
        assert_eq!(consent.total_spent.units(), 0);
        assert_eq!(consent.remaining(), 5_000);
    }
}
