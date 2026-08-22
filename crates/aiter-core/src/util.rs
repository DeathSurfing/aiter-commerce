//! Tiny shared helpers.

/// Unix seconds since the epoch — the timestamp convention used across the
/// checkout flow (orders, receipts, consents, checkout sessions).
///
/// `i64` by convention: the core schema constructors (`Order::new`,
/// `Receipt::new`, `Consent::new`, `CheckoutSession::new`) take `i64`
/// timestamps. Signing sites need `u64` (`RequestSignature.timestamp`); they
/// cast with `now() as u64`, which is lossless while wall-clock seconds fit
/// in `i64::MAX`.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
