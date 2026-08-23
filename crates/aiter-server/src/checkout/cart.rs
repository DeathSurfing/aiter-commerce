//! Cart REST surface: create / read / replace / cancel carts.

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use aiter_core::amount::Currency;
use aiter_core::cart::{Cart, LineItem};
use aiter_core::pricing::{compute_totals, NoTax, Totals};
use aiter_core::store::Store;

use crate::catalog::AppState;
use crate::checkout::error::ApiError;

// ---------------------------------------------------------------------------
// Cart API
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub(crate) struct CreateCartRequest {
    #[serde(default)]
    currency: Option<Currency>,
    #[serde(default)]
    items: Vec<LineItem>,
}

/// A cart as seen by clients: the core [`Cart`] plus its id, cancelled flag,
/// and live re-derived totals (totals are not stored on the cart; they are
/// always computed at pricing time, see [`aiter_core::cart`]).
#[derive(Serialize)]
pub(crate) struct CartResponse {
    id: String,
    cancelled: bool,
    cart: Cart,
    totals: Option<Totals>,
}

/// Build a [`CartResponse`] with totals re-derived from the catalog.
fn cart_response(st: &AppState, id: &str, cart: &Cart, cancelled: bool) -> CartResponse {
    let totals = compute_totals(cart, |p| st.price_of(p), &NoTax).ok();
    CartResponse {
        id: id.to_string(),
        cancelled,
        cart: cart.clone(),
        totals,
    }
}

/// Reject any line item whose product id is not in the served catalog, or
/// whose price currency differs from the cart's currency (#36).
///
/// Both cart mutators call this up front so an unknown id or a cross-currency
/// line is a 400 with a clear error, never a 200 cart with `totals: null`
/// (a mixed-currency cart cannot be priced — subtotal math refuses to combine
/// currencies, which would otherwise surface only later as a null-totals
/// cart or a session-time 400).
fn validate_catalog_items(
    st: &AppState,
    currency: Currency,
    items: &[LineItem],
) -> Result<(), ApiError> {
    for item in items {
        match st.price_of(&item.product_id) {
            None => return Err(ApiError::UnknownProduct(item.product_id.clone())),
            Some(price) if price.currency() != currency => {
                return Err(ApiError::CurrencyMismatch {
                    product_id: item.product_id.clone(),
                    expected: currency,
                    got: price.currency(),
                });
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// `POST /carts` — create a cart, returning its id.
pub(crate) async fn create_cart(
    State(st): State<AppState>,
    Json(body): Json<CreateCartRequest>,
) -> Result<Json<CartResponse>, ApiError> {
    let currency = body.currency.unwrap_or(Currency::USD);
    validate_catalog_items(&st, currency, &body.items)?;
    let mut cart = Cart::new(currency);
    for item in body.items {
        cart.update(&item.product_id, item.quantity);
    }
    let id = st.gen_id("cart");
    st.carts
        .lock()
        .await
        .create(id.clone(), cart.clone())
        .map_err(ApiError::Store)?;
    Ok(Json(cart_response(&st, &id, &cart, false)))
}

/// `GET /carts/{id}` — read a cart.
pub(crate) async fn get_cart(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CartResponse>, ApiError> {
    let cart = st
        .carts
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or(ApiError::NotFound)?;
    let cancelled = st.cancelled_carts.lock().await.contains(&id);
    Ok(Json(cart_response(&st, &id, &cart, cancelled)))
}

#[derive(Deserialize)]
pub(crate) struct UpdateCartRequest {
    /// Full replacement body. Quantity 0 removes a line.
    items: Vec<LineItem>,
}

/// `PUT /carts/{id}` — replace the cart's items and re-derive totals.
pub(crate) async fn update_cart(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCartRequest>,
) -> Result<Json<CartResponse>, ApiError> {
    let mut stores = st.carts.lock().await;
    let cart = stores.get(&id).cloned().ok_or(ApiError::NotFound)?;
    // A cancelled cart is frozen (#68): its items can no longer be rewritten.
    if st.cancelled_carts.lock().await.contains(&id) {
        return Err(ApiError::Conflict("cart is cancelled".to_string()));
    }
    // Reject unknown product ids / cross-currency lines up front (400),
    // never a null-totals cart.
    validate_catalog_items(&st, cart.currency, &body.items)?;
    let mut updated = Cart::new(cart.currency);
    for item in body.items {
        updated.update(&item.product_id, item.quantity);
    }
    stores
        .update(id.clone(), updated.clone())
        .map_err(ApiError::Store)?;
    let cancelled = st.cancelled_carts.lock().await.contains(&id);
    Ok(Json(cart_response(&st, &id, &updated, cancelled)))
}

/// `POST /carts/{id}/cancel` — cancel a cart. Idempotent: cancelling an
/// already-cancelled (or missing-but-registered) cart is a no-op.
pub(crate) async fn cancel_cart(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CartResponse>, ApiError> {
    let cart = st
        .carts
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or(ApiError::NotFound)?;
    st.cancelled_carts.lock().await.insert(id.clone());
    Ok(Json(cart_response(&st, &id, &cart, true)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::seed_catalog;
    use crate::test_util::{call, register_test_agent};
    use axum::http::{Method, StatusCode};
    use axum::Router;
    use serde_json::json;

    async fn app() -> Router {
        let st = AppState::new(seed_catalog());
        register_test_agent(&st).await;
        crate::router(st)
    }

    #[tokio::test]
    async fn cart_crud_and_idempotent_cancel() {
        let app = app().await;

        // Create a cart.
        let (status, created) = call(
            &app,
            Method::POST,
            "/carts",
            Some(json!({"currency": "USD", "items": [{"product_id": "p-latte", "quantity": 2}]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = created["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("cart-"));
        assert_eq!(created["totals"]["subtotal"]["units"], 900); // p-latte x2 = 900 units

        // Read it back.
        let (status, got) = call(&app, Method::GET, &format!("/carts/{id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got["cart"]["items"][0]["quantity"], 2);
        assert_eq!(got["cancelled"], false);

        // Update it; totals are re-derived.
        let (status, updated) = call(
            &app,
            Method::PUT,
            &format!("/carts/{id}"),
            Some(json!({"items": [{"product_id": "p-espresso", "quantity": 2}]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["totals"]["subtotal"]["units"], 600); // 2 * 300

        // Cancel (idempotent).
        let (status, cancelled) =
            call(&app, Method::POST, &format!("/carts/{id}/cancel"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cancelled["cancelled"], true);

        let (status, cancelled_again) =
            call(&app, Method::POST, &format!("/carts/{id}/cancel"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cancelled_again["cancelled"], true);
    }

    #[tokio::test]
    async fn missing_cart_is_404() {
        let app = app().await;
        let (status, _) = call(&app, Method::GET, "/carts/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_product_in_cart_is_rejected_with_400_and_totals_never_null() {
        let app = app().await;

        // Unknown product ids are rejected up front with a clear JSON error.
        let (status, body) = call(
            &app,
            Method::POST,
            "/carts",
            Some(json!({
                "currency": "USD",
                "items": [{"product_id": "ghost", "quantity": 1}]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("ghost"),
            "error should name the unknown product"
        );

        // Known ids always carry non-null totals.
        let (status, created) = call(
            &app,
            Method::POST,
            "/carts",
            Some(json!({"currency": "USD", "items": [{"product_id": "p-latte", "quantity": 2}]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(created["totals"]["subtotal"]["units"], 900);
        let id = created["id"].as_str().unwrap().to_string();

        // Replacing items with an unknown id is also a 400, not a null-totals 200.
        let (status, body) = call(
            &app,
            Method::PUT,
            &format!("/carts/{id}"),
            Some(json!({"items": [{"product_id": "ghost", "quantity": 1}]})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("ghost"));
    }
}
