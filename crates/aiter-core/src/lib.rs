//! AITER COMMERCE — core library.
//!
//! Pure-Rust primitives for agentic commerce: merchant identity, catalog,
//! checkout-session and payment-protocol schemas, and the logic an agent-facing
//! merchant needs to be "AI-buyable".
//!
//! This crate is intentionally dependency-light (`serde` + `serde_json` for
//! wire-format round-tripping, plus the `ed25519-dalek`/`sha2`/`base64` stack
//! used by [`signing`] for agent identity). Protocol/schema code that can live
//! without framework baggage belongs here; HTTP/runtime concerns live in
//! `aiter-server`.
//!
//! Money is everywhere represented as integer minor units (see [`amount`]) —
//! never floats.

pub mod amount;
pub mod cart;
pub mod catalog;
pub mod checkout;
pub mod merchant;
pub mod order;
pub mod pricing;
pub mod receipt;
pub mod reserve;
pub mod signing;
pub mod store;

/// Human-facing name of the project.
pub const NAME: &str = "AITER COMMERCE";

/// Current crate version, mirrored from the workspace manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_version_are_defined() {
        assert!(!NAME.is_empty());
        assert!(!VERSION.is_empty());
    }
}
