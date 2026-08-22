//! AITER COMMERCE — thin HTTP server library.
//!
//! Exposes the axum router and catalog surface so integration tests can drive
//! it with `tower::ServiceExt::oneshot`. The binary (`main.rs`) is a thin
//! wrapper that seeds state and binds the listener.
//!
//! # Public vs protected routes (issue #25)
//!
//! The router splits into a **public** read/discovery surface and a
//! **protected** mutation surface. Anyone may call:
//!
//! * `GET /` and `GET /agentic/health` — service identity + liveness,
//! * `GET /catalog/products` and `GET /catalog/products/{id}` — catalog reads,
//! * `GET /.well-known/agent-card.json`, `GET /llms.txt` — discovery,
//! * `GET /seed/catalog` — demo seed export,
//! * `GET /metrics` — plain-text request/order counter dump (issue #33; public
//!   so operators and scrapers can reach it without agent signatures),
//! * `GET /carts/{id}` — cart reads (no state mutates on a read),
//! * `POST /webhooks/razorpay` — Razorpay webhook delivery. **Deliberate
//!   exception to the signed-write rule**: Razorpay authenticates webhooks
//!   with its own HMAC-SHA256 signature (`x-razorpay-signature`,
//!   [`payments::verify_webhook_signature`]) — the agent protocol cannot
//!   produce that, so the route stays public and the handler verifies the
//!   HMAC itself (fails closed without `RAZORPAY_WEBHOOK_SECRET`).
//!
//! Every other **mutating** endpoint requires a request signed by a
//! registered agent ([`auth::require_signed`] middleware), including:
//!
//! * `POST /carts`, `PUT /carts/{id}`, `POST /carts/{id}/cancel`,
//! * `POST /checkout_sessions`, `POST /checkout_sessions/{id}/complete`,
//!   `POST /checkout_sessions/{id}/cancel`,
//! * `POST /orders/{id}/payment_link` — mint a Razorpay payment link for an
//!   order (agent-facing write with an external side effect: every call hits
//!   the Razorpay API, so it must be attributable to a verified agent),
//! * any future write routes (e.g. `/webhooks/*`) must be added behind the
//!   same middleware — except webhooks whose authenticity is proven by the
//!   provider's own signature (like `/webhooks/razorpay` above).
//!
//! A signed request carries `x-agent-id` (agent id) and `x-request-signature`
//! (JSON-serialized [`aiter_core::signing::RequestSignature`]) headers;
//! unregistered agents get `403`, missing/invalid signatures get `401`. The
//! spend cap enforced at checkout completion is the *same* agent identity the
//! middleware verified against (issues #26/#27 interact by design: the agent
//! recorded in the receipt/audit log is the one whose signature passed).
//!
//! # Rate limiting + abuse protection (issue #35)
//!
//! Two in-memory fixed-window tiers (see the [`rate_limit`] module):
//! **writes** are throttled per *verified agent* (keyed on the
//! [`auth::VerifiedAgent`] extension installed by `require_signed`, so
//! unsigned garbage never burns a real quota) and **public reads** per client
//! IP (TCP peer address, else `x-forwarded-for`, else `"local"` for
//! sockless tests). Limits default to
//! `RATE_LIMIT_WRITES_PER_MIN=60` / `RATE_LIMIT_READS_PER_MIN=120` and are
//! read from env at state construction (or pinned per-instance via
//! [`AppState::with_rate_limits`] in tests). Over-limit requests get
//! `429` + `Retry-After: 1` + `{"error":"rate limit exceeded"}`.
//!
//! Example clients work out of the box: [`catalog::AppState`] pre-registers a
//! well-known **demo agent** ([`catalog::DEMO_AGENT_ID`]) whose Ed25519
//! keypair is derived from a fixed, public seed ([`catalog::demo_agent`]), so
//! the `aiter-cli` binary can run against a fresh server without any setup
//! (issue #29). The demo key is public by design — demos and tests only.

pub mod auth;
pub mod catalog;
pub mod checkout;
pub mod cli;
pub mod mcp;
pub mod metrics;
pub mod payments;
pub mod rate_limit;
pub mod reserve;
pub mod seed;
pub mod test_util;

use axum::middleware;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};

use catalog::AppState;

/// Build the full application router with a shared [`AppState`].
///
/// Write routes are wrapped in [`auth::require_signed`] via
/// `route_layer`, so signature verification (issue #25) runs before every
/// mutation; read/discovery routes stay public (see the module docs).
///
/// Both rate-limiting tiers (issue #35) are also `route_layer`s. For writes
/// the limiter is stacked *inside* `require_signed` (added first, so the
/// signature check runs outermost and installs the [`auth::VerifiedAgent`]
/// extension the limiter keys on); reads get the per-IP limiter directly.
pub fn router(state: AppState) -> Router {
    // Same middleware instance shape for every write route: it borrows a clone
    // of the state (agent registry + rate limiters) consumed by with_state.
    let require_signed = || middleware::from_fn_with_state(state.clone(), auth::require_signed);
    let rate_limit_writes =
        || middleware::from_fn_with_state(state.clone(), rate_limit::limit_writes);
    let rate_limit_reads =
        || middleware::from_fn_with_state(state.clone(), rate_limit::limit_reads);

    Router::new()
        .route("/", get(service_info).route_layer(rate_limit_reads()))
        .route(
            "/agentic/health",
            get(health).route_layer(rate_limit_reads()),
        )
        .route(
            "/catalog/products",
            get(catalog::list_products).route_layer(rate_limit_reads()),
        )
        .route(
            "/catalog/products/{id}",
            get(catalog::get_product).route_layer(rate_limit_reads()),
        )
        .route(
            "/.well-known/agent-card.json",
            get(catalog::agent_card).route_layer(rate_limit_reads()),
        )
        .route(
            "/llms.txt",
            get(catalog::llms_txt).route_layer(rate_limit_reads()),
        )
        .route(
            "/seed/catalog",
            get(seed::seed_catalog).route_layer(rate_limit_reads()),
        )
        .route(
            "/carts",
            post(checkout::create_cart)
                .route_layer(rate_limit_writes())
                .route_layer(require_signed()),
        )
        .route(
            "/carts/{id}",
            get(checkout::get_cart).route_layer(rate_limit_reads()).put(
                put(checkout::update_cart)
                    .route_layer(rate_limit_writes())
                    .route_layer(require_signed()),
            ),
        )
        .route(
            "/carts/{id}/cancel",
            post(checkout::cancel_cart)
                .route_layer(rate_limit_writes())
                .route_layer(require_signed()),
        )
        .route(
            "/checkout_sessions",
            post(checkout::create_checkout_session)
                .route_layer(rate_limit_writes())
                .route_layer(require_signed()),
        )
        .route(
            "/checkout_sessions/{id}/complete",
            post(checkout::complete_checkout)
                .route_layer(rate_limit_writes())
                .route_layer(require_signed()),
        )
        .route(
            "/checkout_sessions/{id}/cancel",
            post(checkout::cancel_checkout)
                .route_layer(rate_limit_writes())
                .route_layer(require_signed()),
        )
        .route(
            "/reserve_pay/consent",
            post(reserve::create_consent)
                .route_layer(rate_limit_writes())
                .route_layer(require_signed()),
        )
        .route(
            "/reserve_pay/debit",
            post(reserve::debit)
                .route_layer(rate_limit_writes())
                .route_layer(require_signed()),
        )
        .route(
            "/orders/{id}/payment_link",
            post(payments::order_payment_link)
                .route_layer(rate_limit_writes())
                .route_layer(require_signed()),
        )
        // Deliberately PUBLIC: Razorpay signs webhook deliveries with its own
        // HMAC (`x-razorpay-signature`) — the agent does not — so require_signed
        // must NOT wrap this route; the handler verifies the HMAC itself
        // (see the module docs on the public/protected split). It still pays
        // the per-IP read quota as a public route (#35).
        .route(
            "/webhooks/razorpay",
            post(payments::razorpay_webhook).route_layer(rate_limit_reads()),
        )
        // Observability (issue #33): /metrics is public (no signature needed)
        // and a per-request span + counter layer wraps every route.
        .route("/metrics", get(metrics::metrics_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            metrics::request_trace,
        ))
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
