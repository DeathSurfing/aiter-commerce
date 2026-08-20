//! AITER COMMERCE — core library.
//!
//! Pure-Rust primitives for agentic commerce: merchant identity, catalog,
//! checkout-session and payment-protocol schemas, and the logic an agent-facing
//! merchant needs to be "AI-buyable".
//!
//! This crate is intentionally dependency-light. Protocol/schema code that can
//! live without framework baggage belongs here; HTTP/runtime concerns live in
//! `aiter-server`.

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
