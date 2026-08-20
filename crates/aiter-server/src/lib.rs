//! AITER COMMERCE — thin HTTP server library.
//!
//! Exposes the axum router and catalog surface so integration tests can drive
//! it with `tower::ServiceExt::oneshot`. The binary (`main.rs`) is a thin
//! wrapper that seeds state and binds the listener.

pub mod catalog;

use axum::routing::get;
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
        .with_state(state)
}

/// Service identity for any agent or client that discovers us.
async fn service_info() -> Json<Value> {
    Json(json!({
        "name": aiter_core::NAME,
        "version": aiter_core::VERSION,
        "repo": "https://github.com/DeathSurfing/aiter-commerce",
        "agentic": true,
    }))
}

/// Liveness + version probe.
async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": aiter_core::VERSION,
    }))
}
