// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------
//
//! The one handler-error type for the whole crate (issue #73): checkout,
//! reserve-pay and payments handlers all return `Result<_, ApiError>`.
//! Every HTTP status and JSON body below is byte-identical to the two
//! per-module enums this replaced — existing tests pin them exactly.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use aiter_core::amount::Currency;
use aiter_core::checkout::CheckoutError;
use aiter_core::pricing::PricingError;
use aiter_core::store::StoreError;

use crate::payments::RazorpayError;

#[derive(Debug)]
pub(crate) enum ApiError {
    /// Generic 404 ("not found").
    NotFound,
    Conflict(String),
    Store(StoreError),
    Checkout(CheckoutError),
    Pricing(PricingError),
    Razorpay(RazorpayError),
    /// A cart line references a product id that is not in the served catalog.
    UnknownProduct(String),
    /// A cart line's price currency differs from the cart's currency (#36).
    CurrencyMismatch {
        product_id: String,
        expected: Currency,
        got: Currency,
    },
    /// Spend-cap enforcement at checkout completion (issue #26).
    SpendLimit(String),
    /// Reserve-Pay route: the named consent does not exist (renders as
    /// "consent not found", distinct from the generic 404).
    ConsentNotFound,
    /// Reserve-Pay-specific failures (#22), rendered byte-identically to the
    /// former reserve-local enum (issue #73).
    Reserve(ReserveError),
}

/// Reserve-Pay-specific failures (#22, #73), nested so their bespoke response
/// bodies (e.g. `device_mismatch` with its `detail` field) render
/// byte-identically to the former reserve-local enum.
#[derive(Debug)]
pub(crate) enum ReserveError {
    /// Consent exists but is not `Active` (revoked).
    NotActive,
    /// Debit would exceed the consent's remaining limit.
    LimitExceeded { remaining: i64 },
    /// Debit device differs from the consenting device.
    DeviceMismatch,
    /// Debit currency differs from the consent's limit currency.
    CurrencyMismatch(Currency),
    /// A minor-unit amount was not a positive integer (negative debits would
    /// inflate the remaining limit; non-positive spend limits are meaningless).
    InvalidAmount(String),
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError::Store(e)
    }
}

impl From<CheckoutError> for ApiError {
    fn from(e: CheckoutError) -> Self {
        ApiError::Checkout(e)
    }
}

impl From<PricingError> for ApiError {
    fn from(e: PricingError) -> Self {
        ApiError::Pricing(e)
    }
}

impl From<RazorpayError> for ApiError {
    fn from(e: RazorpayError) -> Self {
        ApiError::Razorpay(e)
    }
}

impl From<ReserveError> for ApiError {
    fn from(e: ReserveError) -> Self {
        ApiError::Reserve(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, body): (StatusCode, Value) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, json!({ "error": "not found" })),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, json!({ "error": m })),
            ApiError::Store(StoreError::NotFound) => {
                (StatusCode::NOT_FOUND, json!({ "error": "not found" }))
            }
            ApiError::Store(StoreError::AlreadyExists) => {
                (StatusCode::CONFLICT, json!({ "error": "already exists" }))
            }
            ApiError::Checkout(e) => (
                StatusCode::CONFLICT,
                json!({ "error": format!("illegal checkout transition: {e:?}") }),
            ),
            ApiError::Pricing(e) => (
                StatusCode::BAD_REQUEST,
                json!({ "error": format!("unpriced item: {e:?}") }),
            ),
            ApiError::Razorpay(e) => match &e {
                RazorpayError::Config(msg) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({ "error": msg.clone() }),
                ),
                RazorpayError::Signature(msg) => {
                    (StatusCode::UNAUTHORIZED, json!({ "error": msg.clone() }))
                }
                _ => (StatusCode::BAD_GATEWAY, json!({ "error": e.to_string() })),
            },
            ApiError::UnknownProduct(id) => (
                StatusCode::BAD_REQUEST,
                json!({ "error": format!("unknown product: {id}") }),
            ),
            ApiError::CurrencyMismatch {
                product_id,
                expected,
                got,
            } => (
                StatusCode::BAD_REQUEST,
                json!({ "error": format!(
                    "product {product_id} is priced in {} but the cart is in {}; \
                     refusing to price a mixed-currency cart",
                    got.code(),
                    expected.code()
                )}),
            ),
            ApiError::SpendLimit(m) => (StatusCode::FORBIDDEN, json!({ "error": m })),
            ApiError::ConsentNotFound => (
                StatusCode::NOT_FOUND,
                json!({ "error": "consent not found" }),
            ),
            ApiError::Reserve(ReserveError::NotActive) => (
                StatusCode::FORBIDDEN,
                json!({ "error": "consent is not active" }),
            ),
            ApiError::Reserve(ReserveError::LimitExceeded { remaining }) => (
                StatusCode::FORBIDDEN,
                json!({ "error": "spend limit exceeded", "remaining": remaining }),
            ),
            ApiError::Reserve(ReserveError::DeviceMismatch) => (
                StatusCode::CONFLICT,
                json!({
                    "error": "device_mismatch",
                    "detail": "confirm re-auth via ?confirm=true",
                }),
            ),
            ApiError::Reserve(ReserveError::CurrencyMismatch(currency)) => (
                StatusCode::FORBIDDEN,
                json!({ "error": "currency mismatch", "limit_currency": currency.code() }),
            ),
            ApiError::Reserve(ReserveError::InvalidAmount(message)) => {
                (StatusCode::BAD_REQUEST, json!({ "error": message }))
            }
        };
        (code, Json(body)).into_response()
    }
}
