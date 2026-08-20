//! AITER COMMERCE — thin HTTP server.
//!
//! Exposes the agent-facing + merchant-facing surface. Keep this crate thin:
//! protocol and business logic live in `aiter-core`.

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;

fn router() -> Router {
    Router::new()
        .route("/", get(service_info))
        .route("/agentic/health", get(health))
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aiter_server=info,tower_http=info".into()),
        )
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let app = router();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");
    tracing::info!("aiter-server listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
