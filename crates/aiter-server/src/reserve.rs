//! UPI Reserve Pay REST surface (issue #22).
//!
//! NPCI Single-Block Mandate Debit (SBMD) model, mirrored by Razorpay's UPI
//! Reserve Pay: the user consents **once** to an agent drawing against a
//! spending limit, and the agent can then debit without re-authentication
//! until the limit is exhausted or the consent is revoked.
//!
//! * `POST /reserve_pay/consent` — capture a one-time consent (`user_id`,
//!   `agent_id`, `spend_limit_minor`, `currency`, `device`) and return the new
//!   `Active` [`Consent`]. `currency` defaults to the store currency (USD)
//!   and any other supported currency is accepted as-is (no conversion).
//! * `POST /reserve_pay/debit` — agent debit against a consent. Enforced
//!   **before** any state change: missing consent `404`, non-`Active` `403`,
//!   over-limit `403` (with `remaining`), device mismatch `409` unless the
//!   request confirms re-auth via `?confirm=true`. On success `total_spent`
//!   is incremented and the new `remaining` is returned.
//!
//! The ledger is **standalone**: it does not couple to carts, checkout
//! sessions, orders or the payments rail. Both routes are write routes, so
//! (per issue #25's convention) they sit behind
//! [`crate::auth::require_signed`] like every other mutation.
//!
//! # Gating: Razorpay early-access sandbox simulation
//!
//! UPI Reserve Pay is a Razorpay **early-access** product. This surface is a
//! **local sandbox simulation**: consents and debits mutate the in-memory
//! ledger only — no Razorpay calls, no PSP mandate registration, no
//! settlement. Do not enable for live funds until Razorpay grants access and
//! the debit path is wired to the real Reserve Pay mandate API.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

use aiter_core::amount::{Amount, Currency};
use aiter_core::reserve::{Consent, ConsentStatus};
use aiter_core::store::{Store, StoreError};
use aiter_core::util::now;

use crate::catalog::AppState;

/// Store default currency for consents: matches the seeded catalog (USD).
const DEFAULT_CURRENCY: Currency = Currency::USD;

// ---------------------------------------------------------------------------
// Consent capture
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct ConsentRequest {
    user_id: String,
    agent_id: String,
    /// Spending ceiling, in integer minor units of `currency`.
    spend_limit_minor: i64,
    /// Defaults to the store currency (USD); other currencies accepted as-is.
    #[serde(default)]
    currency: Option<Currency>,
    /// Device granting the consent, e.g. `"mobile"` or `"pc"`.
    device: String,
}

/// `POST /reserve_pay/consent` — capture a one-time consent and return the
/// new `Active` [`Consent`] (`200`).
pub(crate) async fn create_consent(
    State(st): State<AppState>,
    Json(body): Json<ConsentRequest>,
) -> Result<Json<Consent>, ApiError> {
    validate_positive_minor(body.spend_limit_minor, "spend_limit_minor")?;
    let currency = body.currency.unwrap_or(DEFAULT_CURRENCY);
    let spend_limit = Amount::new(body.spend_limit_minor, currency);
    let id = st.gen_id("cons");
    let consent = Consent::new(
        id,
        body.user_id,
        body.agent_id,
        spend_limit,
        body.device,
        now(),
    );
    st.consents
        .lock()
        .await
        .create(consent.consent_id.clone(), consent.clone())
        .map_err(ApiError::Store)?;
    Ok(Json(consent))
}

// ---------------------------------------------------------------------------
// Agent debit
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct DebitRequest {
    consent_id: String,
    /// Amount to draw, in integer minor units of `currency`.
    amount_minor: i64,
    currency: Currency,
    /// Device the debit originates from.
    device: String,
}

/// Query parameters for `POST /reserve_pay/debit`.
#[derive(Deserialize, Default)]
pub(crate) struct DebitQuery {
    /// `?confirm=true` — user explicitly re-authenticated on the mismatched
    /// device, overriding the device-mismatch guard.
    #[serde(default)]
    confirm: bool,
}

/// `POST /reserve_pay/debit` — draw against a consent, enforcing limit and
/// device checks **before** any state change:
///
/// * unknown consent -> `404`,
/// * not `Active` (e.g. revoked) -> `403`,
/// * `amount_minor` > remaining limit -> `403 {"error":"spend limit
///   exceeded","remaining":N}`,
/// * debit device differs from the consenting device -> `409
///   {"error":"device_mismatch","detail":"confirm re-auth via ?confirm=true"}`
///   unless `?confirm=true`,
/// * currency differing from the consent's limit currency -> `403` (the limit
///   is denominated in one currency; cross-currency debits are refused).
///
/// On success `total_spent` is incremented and the new `remaining` is
/// returned: `{"status":"debited","consent_id":"…","remaining":N}`.
pub(crate) async fn debit(
    State(st): State<AppState>,
    Query(query): Query<DebitQuery>,
    Json(body): Json<DebitRequest>,
) -> Result<Json<Value>, ApiError> {
    validate_positive_minor(body.amount_minor, "amount_minor")?;

    // Observability (issue #33): one span per debit attempt, success or not.
    let span = tracing::info_span!(
        "reserve_debit",
        consent_id = %body.consent_id,
        amount_minor = body.amount_minor
    );
    let _guard = span.enter();
    let mut consents = st.consents.lock().await;
    let consent = consents
        .get(&body.consent_id)
        .cloned()
        .ok_or(ApiError::NotFound)?;

    if consent.status != ConsentStatus::Active {
        return Err(ApiError::NotActive);
    }
    if consent.spend_limit.currency() != body.currency {
        return Err(ApiError::CurrencyMismatch(consent.spend_limit.currency()));
    }
    let remaining = consent.remaining();
    if body.amount_minor > remaining {
        return Err(ApiError::LimitExceeded { remaining });
    }
    if body.device != consent.device && !query.confirm {
        return Err(ApiError::DeviceMismatch);
    }

    let mut updated = consent.clone();
    updated.total_spent = Amount::new(
        updated.total_spent.units() + body.amount_minor,
        updated.total_spent.currency(),
    );
    consents
        .update(body.consent_id.clone(), updated)
        .map_err(ApiError::Store)?;

    // Observability (issue #33): count the successful debit.
    st.metrics.reserve_debits.fetch_add(1, Ordering::Relaxed);

    Ok(Json(json!({
        "status": "debited",
        "consent_id": body.consent_id,
        "remaining": remaining - body.amount_minor,
    })))
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum ApiError {
    NotFound,
    Store(StoreError),
    /// Consent exists but is not `Active` (revoked).
    NotActive,
    /// Debit would exceed the consent's remaining limit.
    LimitExceeded {
        remaining: i64,
    },
    /// Debit device differs from the consenting device.
    DeviceMismatch,
    /// Debit currency differs from the consent's limit currency.
    CurrencyMismatch(Currency),
    /// A minor-unit amount was not a positive integer (negative debits would
    /// inflate the remaining limit; non-positive spend limits are meaningless).
    InvalidAmount(String),
}

/// Reject non-positive minor-unit amounts before any ledger math (#36).
///
/// A negative `amount_minor` would pass the `> remaining` guard and then
/// *decrease* `total_spent`, inflating the consent's remaining limit — the
/// agent could mint spend headroom out of thin air. A non-positive spend
/// limit is equally meaningless. Both cart mutators' sibling checks use this
/// one guard so every money-taking route rejects `<= 0` the same way.
fn validate_positive_minor(units: i64, field: &str) -> Result<(), ApiError> {
    if units <= 0 {
        return Err(ApiError::InvalidAmount(format!(
            "{field} must be a positive integer number of minor units, got {units}"
        )));
    }
    Ok(())
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError::Store(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, body): (StatusCode, Value) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, json!({"error": "consent not found"})),
            ApiError::Store(StoreError::NotFound) => {
                (StatusCode::NOT_FOUND, json!({"error": "consent not found"}))
            }
            ApiError::Store(StoreError::AlreadyExists) => {
                (StatusCode::CONFLICT, json!({"error": "already exists"}))
            }
            ApiError::NotActive => (
                StatusCode::FORBIDDEN,
                json!({"error": "consent is not active"}),
            ),
            ApiError::LimitExceeded { remaining } => (
                StatusCode::FORBIDDEN,
                json!({"error": "spend limit exceeded", "remaining": remaining}),
            ),
            ApiError::DeviceMismatch => (
                StatusCode::CONFLICT,
                json!({
                    "error": "device_mismatch",
                    "detail": "confirm re-auth via ?confirm=true",
                }),
            ),
            ApiError::CurrencyMismatch(currency) => (
                StatusCode::FORBIDDEN,
                json!({"error": "currency mismatch", "limit_currency": currency.code()}),
            ),
            ApiError::InvalidAmount(message) => {
                (StatusCode::BAD_REQUEST, json!({ "error": message }))
            }
        };
        (code, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::seed_catalog;
    use crate::test_util::{call, register_test_agent};
    use axum::http::Method;
    use axum::Router;
    use serde_json::Value;

    async fn app() -> Router {
        let st = AppState::new(seed_catalog());
        // Register the demo agent so signed writes pass the middleware.
        register_test_agent(&st).await;
        crate::router(st)
    }

    async fn create_consent(app: &Router, limit_minor: i64, device: &str) -> (StatusCode, Value) {
        call(
            app,
            Method::POST,
            "/reserve_pay/consent",
            Some(json!({
                "user_id": "user-1",
                "agent_id": "agent-1",
                "spend_limit_minor": limit_minor,
                "currency": "USD",
                "device": device,
            })),
        )
        .await
    }

    fn consent_id(resp: &Value) -> String {
        resp["consent_id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn consent_creation_returns_active_consent() {
        let app = app().await;
        let (status, consent) = create_consent(&app, 10_000, "mobile").await;
        assert_eq!(status, StatusCode::OK);
        assert!(consent_id(&consent).starts_with("cons-"));
        assert_eq!(consent["user_id"], "user-1");
        assert_eq!(consent["agent_id"], "agent-1");
        assert_eq!(consent["spend_limit"]["units"], 10_000);
        assert_eq!(consent["spend_limit"]["currency"], "USD");
        assert_eq!(consent["total_spent"]["units"], 0);
        assert_eq!(consent["device"], "mobile");
        assert_eq!(consent["status"], "Active");
        assert!(consent["created_at"].as_i64().unwrap() > 0);
    }

    #[tokio::test]
    async fn debit_ok_decrements_remaining() {
        let app = app().await;
        let (_, consent) = create_consent(&app, 10_000, "mobile").await;
        let id = consent_id(&consent);

        let (status, resp) = call(
            &app,
            Method::POST,
            "/reserve_pay/debit",
            Some(json!({
                "consent_id": id,
                "amount_minor": 3_500,
                "currency": "USD",
                "device": "mobile",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["status"], "debited");
        assert_eq!(resp["consent_id"], id);
        assert_eq!(resp["remaining"], 6_500);

        // A second debit reduces remaining further.
        let (status, resp) = call(
            &app,
            Method::POST,
            "/reserve_pay/debit",
            Some(json!({
                "consent_id": id,
                "amount_minor": 1_500,
                "currency": "USD",
                "device": "mobile",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["remaining"], 5_000);
    }

    #[tokio::test]
    async fn over_limit_debit_is_403_with_remaining() {
        let app = app().await;
        let (_, consent) = create_consent(&app, 5_000, "mobile").await;
        let id = consent_id(&consent);

        let (status, resp) = call(
            &app,
            Method::POST,
            "/reserve_pay/debit",
            Some(json!({
                "consent_id": id,
                "amount_minor": 5_001,
                "currency": "USD",
                "device": "mobile",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(resp["error"], "spend limit exceeded");
        assert_eq!(resp["remaining"], 5_000);
    }

    #[tokio::test]
    async fn partial_debit_then_over_limit() {
        let app = app().await;
        let (_, consent) = create_consent(&app, 5_000, "mobile").await;
        let id = consent_id(&consent);

        // Stay under the limit once…
        let (status, _) = call(
            &app,
            Method::POST,
            "/reserve_pay/debit",
            Some(json!({
                "consent_id": id,
                "amount_minor": 4_000,
                "currency": "USD",
                "device": "mobile",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // …then the leftover 1_000 cannot cover 1_500, exact remaining reported.
        let (status, resp) = call(
            &app,
            Method::POST,
            "/reserve_pay/debit",
            Some(json!({
                "consent_id": id,
                "amount_minor": 1_500,
                "currency": "USD",
                "device": "mobile",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(resp["error"], "spend limit exceeded");
        assert_eq!(resp["remaining"], 1_000);
    }

    #[tokio::test]
    async fn missing_consent_is_404() {
        let app = app().await;
        let (status, resp) = call(
            &app,
            Method::POST,
            "/reserve_pay/debit",
            Some(json!({
                "consent_id": "cons-does-not-exist",
                "amount_minor": 100,
                "currency": "USD",
                "device": "mobile",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(resp["error"], "consent not found");
    }

    #[tokio::test]
    async fn revoked_consent_is_403() {
        let st = AppState::new(seed_catalog());
        register_test_agent(&st).await;
        let app = crate::router(st.clone());

        let (status, consent) = create_consent(&app, 5_000, "mobile").await;
        assert_eq!(status, StatusCode::OK);
        let id = consent_id(&consent);

        // Revoke the stored consent directly (no revoke endpoint in scope).
        {
            let mut consents = st.consents.lock().await;
            let mut c = consents.get(&id).cloned().unwrap();
            c.status = ConsentStatus::Revoked;
            consents.update(id.clone(), c).unwrap();
        }

        // A debit against the revoked consent is forbidden, not a 404.
        let (status, resp) = call(
            &app,
            Method::POST,
            "/reserve_pay/debit",
            Some(json!({
                "consent_id": id,
                "amount_minor": 100,
                "currency": "USD",
                "device": "mobile",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(resp["error"], "consent is not active");
    }

    #[tokio::test]
    async fn device_mismatch_conflicts_then_confirm_succeeds() {
        let app = app().await;
        let (_, consent) = create_consent(&app, 10_000, "mobile").await;
        let id = consent_id(&consent);

        // Same amount from a different device -> 409 device_mismatch.
        let (status, resp) = call(
            &app,
            Method::POST,
            "/reserve_pay/debit",
            Some(json!({
                "consent_id": id,
                "amount_minor": 2_000,
                "currency": "USD",
                "device": "pc",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(resp["error"], "device_mismatch");
        assert_eq!(resp["detail"], "confirm re-auth via ?confirm=true");

        // With ?confirm=true the same request succeeds.
        let (status, resp) = call(
            &app,
            Method::POST,
            "/reserve_pay/debit?confirm=true",
            Some(json!({
                "consent_id": id,
                "amount_minor": 2_000,
                "currency": "USD",
                "device": "pc",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["status"], "debited");
        assert_eq!(resp["remaining"], 8_000);
    }
}
