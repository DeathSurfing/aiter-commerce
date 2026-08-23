// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use aiter_core::amount::Currency;
use aiter_core::checkout::CheckoutError;
use aiter_core::pricing::PricingError;
use aiter_core::store::StoreError;

use crate::payments::RazorpayError;

#[derive(Debug)]
pub(crate) enum ApiError {
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, message): (StatusCode, String) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::Store(StoreError::NotFound) => {
                (StatusCode::NOT_FOUND, "not found".to_string())
            }
            ApiError::Store(StoreError::AlreadyExists) => {
                (StatusCode::CONFLICT, "already exists".to_string())
            }
            ApiError::Checkout(e) => (
                StatusCode::CONFLICT,
                format!("illegal checkout transition: {e:?}"),
            ),
            ApiError::Pricing(e) => (StatusCode::BAD_REQUEST, format!("unpriced item: {e:?}")),
            ApiError::Razorpay(e) => match &e {
                RazorpayError::Config(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg.clone()),
                RazorpayError::Signature(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
                _ => (StatusCode::BAD_GATEWAY, e.to_string()),
            },
            ApiError::UnknownProduct(id) => {
                (StatusCode::BAD_REQUEST, format!("unknown product: {id}"))
            }
            ApiError::CurrencyMismatch {
                product_id,
                expected,
                got,
            } => (
                StatusCode::BAD_REQUEST,
                format!(
                    "product {product_id} is priced in {} but the cart is in {}; \
                     refusing to price a mixed-currency cart",
                    got.code(),
                    expected.code()
                ),
            ),
            ApiError::SpendLimit(m) => (StatusCode::FORBIDDEN, m),
        };
        (code, Json(json!({ "error": message }))).into_response()
    }
}
