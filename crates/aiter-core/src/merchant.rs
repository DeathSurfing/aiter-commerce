//! Merchant identity and actor roles.
//!
//! # Role model
//!
//! An agentic-commerce transaction involves three actor roles:
//!
//! * [`ActorRole::Merchant`] — the seller. Identified by a [`MerchantProfile`]
//!   (id, name, pay-to destination, public key / payout URL).
//! * [`ActorRole::Agent`] — the AI buyer acting on a user's behalf.
//! * [`ActorRole::Processor`] — the payment/fulfilment processor in the middle.
//!
//! These are plain serializable enums so an agent can reason about *who* it is
//! talking to, and a merchant can publish its own profile for discovery.

use serde::{Deserialize, Serialize};

/// How a merchant is paid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayToDestination {
    /// A crypto / wallet address.
    Wallet(String),
    /// A bank transfer destination.
    Bank { account: String, routing: String },
}

/// Public identity of a merchant that wants to be "AI-buyable".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerchantProfile {
    pub id: String,
    pub name: String,
    pub pay_to: PayToDestination,
    /// Verification / signing public key, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Endpoint processors or agents use to initiate payment, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout_url: Option<String>,
}

/// One of the three roles in an agentic commerce interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActorRole {
    Merchant,
    Agent,
    Processor,
}

/// An actor bound to a role, e.g. this merchant, that agent, that processor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    pub role: ActorRole,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merchant_profile_serializes_and_round_trips() {
        let profile = MerchantProfile {
            id: "m-1".to_string(),
            name: "Acme".to_string(),
            pay_to: PayToDestination::Wallet("0xabc".to_string()),
            public_key: Some("pubkey-xyz".to_string()),
            payout_url: Some("https://acme.example/pay".to_string()),
        };
        let json = serde_json::to_string(&profile).expect("serialize");
        assert!(json.contains("\"pay_to\""));
        let back: MerchantProfile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(profile, back);
    }

    #[test]
    fn merchant_profile_tolerates_missing_optional_fields() {
        let json =
            r#"{"id":"m-2","name":"Bean Co","pay_to":{"Bank":{"account":"123","routing":"456"}}}"#;
        let profile: MerchantProfile = serde_json::from_str(json).expect("deserialize");
        assert!(profile.public_key.is_none());
        assert!(profile.payout_url.is_none());
        assert_eq!(
            profile.pay_to,
            PayToDestination::Bank {
                account: "123".to_string(),
                routing: "456".to_string(),
            }
        );
    }

    #[test]
    fn bank_pay_to_round_trips() {
        let profile = MerchantProfile {
            id: "m-3".to_string(),
            name: "Wire Inc".to_string(),
            pay_to: PayToDestination::Bank {
                account: "acc".to_string(),
                routing: "rt".to_string(),
            },
            public_key: None,
            payout_url: None,
        };
        let json = serde_json::to_string(&profile).expect("serialize");
        let back: MerchantProfile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(profile, back);
    }

    #[test]
    fn all_actor_roles_serialize_and_round_trip() {
        for role in [ActorRole::Merchant, ActorRole::Agent, ActorRole::Processor] {
            let actor = Actor {
                id: "a-1".to_string(),
                role,
            };
            let json = serde_json::to_string(&actor).expect("serialize");
            let back: Actor = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(actor, back);
            assert_eq!(actor.role, role);
        }
    }
}
