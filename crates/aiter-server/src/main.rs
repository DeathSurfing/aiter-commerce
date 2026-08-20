//! AITER COMMERCE — thin HTTP server binary.
//!
//! Seeds shared catalog state and binds the listener. All routes live in the
//! library crate (`aiter_server::router`) so integration tests can drive them
//! without opening a socket.

use std::net::SocketAddr;

use aiter_server::catalog::{seed_catalog, AppState};

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

    let state = AppState::new(seed_catalog());
    let app = aiter_server::router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");
    tracing::info!("aiter-server listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
