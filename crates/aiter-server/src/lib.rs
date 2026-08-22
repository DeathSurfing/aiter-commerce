//! AITER COMMERCE — thin HTTP server library.
//!
//! Exposes the axum router and catalog surface so integration tests can drive
//! it with `tower::ServiceExt::oneshot`. The binary (`main.rs`) is a thin
//! wrapper that seeds state and binds the listener.

pub mod catalog;
pub mod checkout;
pub mod mcp;
pub mod payments;
pub mod seed;

use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use catalog::AppState;

/// Build the full application router with a shared [`AppState`].
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(service_info))
        .route("/agentic/health", get(health))
        .route("/catalog/products", get(catalog::list_products))
        .route("/catalog/products/{id}", get(catalog::get_product))
        .route("/.well-known/agent-card.json", get(catalog::agent_card))
        .route("/llms.txt", get(catalog::llms_txt))
        .route("/seed/catalog", get(seed::seed_catalog))
        .route("/carts", post(checkout::create_cart))
        .route(
            "/carts/{id}",
            get(checkout::get_cart).put(checkout::update_cart),
        )
        .route("/carts/{id}/cancel", post(checkout::cancel_cart))
        .route(
            "/checkout_sessions",
            post(checkout::create_checkout_session),
        )
        .route(
            "/checkout_sessions/{id}/complete",
            post(checkout::complete_checkout),
        )
        .route(
            "/checkout_sessions/{id}/cancel",
            post(checkout::cancel_checkout),
        )
        .with_state(state)
}

/// Service identity for any agent or client that discovers us.
async fn service_info() -> Json<Value> {
    Json(json!({
        "name": aiter_core::NAME,
        "version": aiter_core::VERSION,
        "repo": "https://github.com/DeathSurfing/aiter-commerce",
        "agentic": true,
        "status": "ok",
    }))
}

/// Liveness + version probe.
async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": aiter_core::VERSION,
    }))
}
