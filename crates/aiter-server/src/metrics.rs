//! Request tracing + metrics (issue #33).
//!
//! Hand-rolled observability — no Prometheus/tracing-extra crates:
//!
//! * [`request_trace`] — axum middleware over the whole router: opens a
//!   `tracing::info_span!` per request (`method`, `uri`, `status` recorded
//!   after the response) and bumps the shared request counters.
//! * [`metrics_handler`] — `GET /metrics`: a public, plain-text counter dump.
//!
//! Counters are plain [`AtomicU64`]s on [`Metrics`], shared via the
//! [`AppState`]; handlers bump the domain counters (`checkouts_completed`,
//! `webhooks_verified`, `reserve_debits`) at their success points.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::catalog::AppState;

/// Hand-rolled request/order counters. Atomically bumped by the middleware
/// and handlers, read by `GET /metrics`.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Every request the router saw (including `/metrics` itself).
    pub requests_total: AtomicU64,
    /// Requests that ended in a 4xx status.
    pub requests_4xx: AtomicU64,
    /// Requests that ended in a 5xx status.
    pub requests_5xx: AtomicU64,
    /// Checkout sessions completed into an order.
    pub checkouts_completed: AtomicU64,
    /// Razorpay webhook deliveries whose HMAC verified and were reconciled.
    pub webhooks_verified: AtomicU64,
    /// Reserve Pay debits that succeeded.
    pub reserve_debits: AtomicU64,
}

/// Per-request span + counter middleware (issue #33), applied to the whole
/// router in [`crate::router`]. Records method and uri up front, then the
/// response status once the inner service has run.
pub async fn request_trace(State(st): State<AppState>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let span = tracing::info_span!("request", method = %method, uri = %uri, status = tracing::field::Empty);
    // Owned guard (`EnteredSpan`), so the middleware future stays 'static.
    let _guard = span.enter();
    let response = next.run(request).await;
    let status = response.status();
    span.record("status", status.as_u16());
    st.metrics.requests_total.fetch_add(1, Ordering::Relaxed);
    if status.is_client_error() {
        st.metrics.requests_4xx.fetch_add(1, Ordering::Relaxed);
    } else if status.is_server_error() {
        st.metrics.requests_5xx.fetch_add(1, Ordering::Relaxed);
    }
    response
}

/// `GET /metrics` — plain-text counter dump. Public by design (no agent
/// signature needed): observability endpoints must stay reachable for
/// operators and scrapers.
pub(crate) async fn metrics_handler(State(st): State<AppState>) -> String {
    let m = &st.metrics;
    format!(
        concat!(
            "aiter_requests_total {}\n",
            "aiter_requests_4xx {}\n",
            "aiter_requests_5xx {}\n",
            "aiter_checkouts_completed {}\n",
            "aiter_webhooks_verified {}\n",
            "aiter_reserve_debits {}\n",
        ),
        m.requests_total.load(Ordering::Relaxed),
        m.requests_4xx.load(Ordering::Relaxed),
        m.requests_5xx.load(Ordering::Relaxed),
        m.checkouts_completed.load(Ordering::Relaxed),
        m.webhooks_verified.load(Ordering::Relaxed),
        m.reserve_debits.load(Ordering::Relaxed),
    )
}
