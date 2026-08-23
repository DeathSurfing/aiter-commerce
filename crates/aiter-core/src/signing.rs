//! Agent identity and RFC 9421-style HTTP request signing.
//!
//! # Trust model (UCP / AP2 mirror)
//!
//! An agent proves *who* it is and *what it intends* by signing the requests
//! it sends to a merchant. The merchant knows the agent's [`AgentIdentity`]
//! (id + Ed25519 public key) out of band — e.g. from an earlier handshake or
//! published profile — and verifies each incoming request against it. Because
//! the method, target URI, body digest, timestamp **and** agent id are all
//! covered by the signature, a valid signature is cryptographic proof of
//! intent: nothing about the request can be changed after signing without the
//! signature breaking. Integrity alone does not stop replays, though: a valid
//! signature could be re-presented forever unless its `@created` timestamp is
//! compared against a clock. This module verifies fields only — freshness is
//! enforced by the merchant *server* itself (`aiter-server`'s `require_signed`
//! middleware rejects timestamps outside a fixed window around its clock).
//!
//! # Signature envelope (RFC 9421 simplified)
//!
//! A signed request carries a [`RequestSignature`] — the analog of RFC 9421's
//! `signature` header, with the covered components spelled out as fields (the
//! analog of `signature-input`). The canonical signing string mirrors
//! RFC 9421's `"name": value` component serialization, one component per line:
//!
//! ```text
//! "@method": <HTTP method>
//! "@target-uri": <request target URI>
//! "@created": <unix seconds>
//! "content-digest": sha-256=:<base64 SHA-256 of body>:
//! "x-agent-id": <agent id>
//! ```
//!
//! Verification recomputes the content digest from the presented body,
//! asserts the presented method/target-URI match the signed ones, and
//! re-verifies the Ed25519 signature over the reconstructed string with the
//! agent's public key.
//!
//! This crate has no HTTP stack on purpose (that lives in `aiter-server`);
//! here we sign/verify the *fields* of a request. Server middleware, spend
//! limits and receipts are separate concerns.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Ed25519 signing secret for an agent. Never serialized; stays server-side.
#[derive(Debug, Clone)]
pub struct AgentKeypair {
    signing_key: SigningKey,
}

/// Public identity of an agent: an id bound to an Ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub id: String,
    pub public_key: VerifyingKey,
}

/// RFC 9421-style signature envelope attached to a signed request.
///
/// Carries the covered components (method, target URI, body digest, timestamp,
/// agent id) plus the Ed25519 signature over their canonical serialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSignature {
    pub agent_id: String,
    pub method: String,
    pub target_uri: String,
    /// `sha-256=:<base64>:`, per RFC 9530 content-digest notation.
    pub content_digest: String,
    /// Unix seconds, RFC 9421 `@created`.
    pub timestamp: u64,
    /// Base64-encoded Ed25519 signature.
    pub signature: String,
}

/// Why a signed request failed verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningError {
    /// Signed agent id does not match the presented identity.
    AgentMismatch,
    /// Body hash does not match the signed content-digest.
    DigestMismatch,
    /// Presented method or target URI differs from the signed ones.
    RequestMismatch,
    /// Signature is not valid for this identity / was tampered with.
    InvalidSignature,
}

impl AgentKeypair {
    /// Generate a fresh Ed25519 keypair from OS entropy.
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
        }
    }

    /// Deterministically derive a keypair from a fixed 32-byte seed.
    ///
    /// The seed is **not** a secret — anyone holding it can reconstruct the
    /// signing key — so this is for well-known demo identities and tests only.
    /// The demo agent (issue #29) works because the merchant and the example
    /// client agree on one fixed public seed, giving both processes the same
    /// keypair without any key exchange.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&seed),
        }
    }

    /// Derive the public [`AgentIdentity`] (id + public key) for this keypair.
    pub fn identity(&self, id: impl Into<String>) -> AgentIdentity {
        AgentIdentity {
            id: id.into(),
            public_key: self.signing_key.verifying_key(),
        }
    }

    /// Sign an HTTP request for `agent_id`, returning its signature envelope.
    pub fn sign_request(
        &self,
        agent_id: &str,
        method: &str,
        target_uri: &str,
        body: &[u8],
        timestamp: u64,
    ) -> RequestSignature {
        let content_digest = content_digest(body);
        let base = signing_string(method, target_uri, timestamp, &content_digest, agent_id);
        let signature = self.signing_key.sign(base.as_bytes());
        RequestSignature {
            agent_id: agent_id.to_string(),
            method: method.to_string(),
            target_uri: target_uri.to_string(),
            content_digest,
            timestamp,
            signature: B64.encode(signature.to_bytes()),
        }
    }
}

/// Verify a signed request against the agent's public identity.
///
/// Recomputes the body digest, checks the presented request matches the signed
/// fields, reconstructs the canonical signing string and re-verifies the
/// Ed25519 signature.
pub fn verify_request(
    identity: &AgentIdentity,
    signature: &RequestSignature,
    method: &str,
    target_uri: &str,
    body: &[u8],
) -> Result<(), SigningError> {
    if signature.agent_id != identity.id {
        return Err(SigningError::AgentMismatch);
    }
    if signature.content_digest != content_digest(body) {
        return Err(SigningError::DigestMismatch);
    }
    if signature.method != method || signature.target_uri != target_uri {
        return Err(SigningError::RequestMismatch);
    }
    let base = signing_string(
        &signature.method,
        &signature.target_uri,
        signature.timestamp,
        &signature.content_digest,
        &signature.agent_id,
    );
    let bytes = B64
        .decode(&signature.signature)
        .map_err(|_| SigningError::InvalidSignature)?;
    let sig_bytes: [u8; 64] = bytes
        .try_into()
        .map_err(|_| SigningError::InvalidSignature)?;
    let sig = Signature::from_bytes(&sig_bytes);
    identity
        .public_key
        .verify(base.as_bytes(), &sig)
        .map_err(|_| SigningError::InvalidSignature)
}

/// RFC 9530-style content digest: `sha-256=:<base64>:`. Recomputable from any
/// presented body, so tampering is caught before signature verification.
fn content_digest(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    format!("sha-256=:{}:", B64.encode(digest))
}

/// Canonical RFC 9421-style signing string: one `"name": value` component per
/// line. Any component change invalidates the signature.
fn signing_string(
    method: &str,
    target_uri: &str,
    timestamp: u64,
    content_digest: &str,
    agent_id: &str,
) -> String {
    format!(
        "\"@method\": {method}\n\"@target-uri\": {target_uri}\n\"@created\": {timestamp}\n\"content-digest\": {content_digest}\n\"x-agent-id\": {agent_id}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const METHOD: &str = "POST";
    const TARGET_URI: &str = "https://merchant.example/orders";
    const BODY: &[u8] = br#"{"sku":"aiter-1","qty":2}"#;
    const TIMESTAMP: u64 = 1_700_000_000;

    #[test]
    fn sign_then_verify_round_trips() {
        let keypair = AgentKeypair::generate();
        let identity = keypair.identity("agent-1");
        let signature = keypair.sign_request("agent-1", METHOD, TARGET_URI, BODY, TIMESTAMP);

        assert_eq!(
            verify_request(&identity, &signature, METHOD, TARGET_URI, BODY),
            Ok(())
        );
    }

    #[test]
    fn from_seed_is_deterministic_and_signs() {
        let seed = [7u8; 32];
        let first = AgentKeypair::from_seed(seed);
        let second = AgentKeypair::from_seed(seed);

        assert_eq!(
            first.identity("agent-demo"),
            second.identity("agent-demo"),
            "the same seed must always derive the same identity"
        );

        // The seeded keypair is fully functional: sign + verify round-trips.
        let identity = first.identity("agent-demo");
        let signature = first.sign_request("agent-demo", METHOD, TARGET_URI, BODY, TIMESTAMP);
        assert_eq!(
            verify_request(&identity, &signature, METHOD, TARGET_URI, BODY),
            Ok(())
        );
    }

    #[test]
    fn tampered_body_is_rejected() {
        let keypair = AgentKeypair::generate();
        let identity = keypair.identity("agent-1");
        let signature = keypair.sign_request("agent-1", METHOD, TARGET_URI, BODY, TIMESTAMP);

        let tampered = b"{\"sku\":\"aiter-1\",\"qty\":999}";
        assert_eq!(
            verify_request(&identity, &signature, METHOD, TARGET_URI, tampered),
            Err(SigningError::DigestMismatch)
        );
    }

    #[test]
    fn wrong_agent_id_is_rejected() {
        let keypair = AgentKeypair::generate();
        let signature = keypair.sign_request("agent-1", METHOD, TARGET_URI, BODY, TIMESTAMP);

        // Same key, different id.
        let other_id = keypair.identity("agent-2");
        assert_eq!(
            verify_request(&other_id, &signature, METHOD, TARGET_URI, BODY),
            Err(SigningError::AgentMismatch)
        );

        // Same id, different key.
        let impostor = AgentKeypair::generate().identity("agent-1");
        assert_eq!(
            verify_request(&impostor, &signature, METHOD, TARGET_URI, BODY),
            Err(SigningError::InvalidSignature)
        );
    }

    #[test]
    fn counterfeit_signature_is_rejected() {
        let keypair = AgentKeypair::generate();
        let identity = keypair.identity("agent-1");
        let signature = keypair.sign_request("agent-1", METHOD, TARGET_URI, BODY, TIMESTAMP);

        // Presenting a different method/URI than the one signed.
        assert_eq!(
            verify_request(&identity, &signature, "GET", TARGET_URI, BODY),
            Err(SigningError::RequestMismatch)
        );

        // Envelope fields swapped after signing (method covers them): the
        // presented request matches the forged envelope, so only the
        // signature can catch the tampering.
        let mut forged = signature.clone();
        forged.method = "GET".to_string();
        assert_eq!(
            verify_request(&identity, &forged, "GET", TARGET_URI, BODY),
            Err(SigningError::InvalidSignature)
        );

        // Random signature bytes.
        let mut forged = signature.clone();
        forged.signature = B64.encode([7u8; 64]);
        assert_eq!(
            verify_request(&identity, &forged, METHOD, TARGET_URI, BODY),
            Err(SigningError::InvalidSignature)
        );
    }

    #[test]
    fn identity_and_signature_serialize_and_round_trip() {
        let keypair = AgentKeypair::generate();
        let identity = keypair.identity("agent-1");
        let signature = keypair.sign_request("agent-1", METHOD, TARGET_URI, BODY, TIMESTAMP);

        let id_json = serde_json::to_string(&identity).expect("serialize identity");
        let id_back: AgentIdentity = serde_json::from_str(&id_json).expect("deserialize identity");
        assert_eq!(identity, id_back);

        let sig_json = serde_json::to_string(&signature).expect("serialize signature");
        let sig_back: RequestSignature =
            serde_json::from_str(&sig_json).expect("deserialize signature");
        assert_eq!(signature, sig_back);
    }
}
