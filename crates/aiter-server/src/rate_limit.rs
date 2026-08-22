//! Rate limiting + abuse protection (issue #35).
//!
//! Two throttling tiers, both implemented as axum middleware
//! ([`middleware::from_fn_with_state`]) backed by a small in-memory
//! **fixed-window** counter shared through [`crate::catalog::AppState`]:
//!
//! 1. **Per-agent writes** — [`limit_writes`] keys on the *verified* agent id
//!    (the [`VerifiedAgent`](crate::auth::VerifiedAgent) extension installed by
//!    [`auth::require_signed`](crate::auth::require_signed)), so unsigned
//!    garbage cannot burn a real agent's quota. Applied to every write route
//!    behind `require_signed`.
//! 2. **Per-IP public reads** — [`limit_reads`] keys on the client's IP,
//!    derived in order of trust: the TCP peer address (`ConnectInfo`, installed
//!    by `into_make_service_with_connect_info` in `main.rs`), else the first
//!    `x-forwarded-for` entry (ponytail: only trustworthy when a proxy
//!    overwrites it — noted as a known ceiling), else the literal key `"local"`
//!    (oneshot/integration tests never carry socket info; every test request
//!    shares one key, which is exactly what the tests drive).
//!
//! Limits are configurable via environment:
//!
//! * `RATE_LIMIT_WRITES_PER_MIN` — writes per verified agent per 60s window
//!   (default [`DEFAULT_WRITES_PER_MIN`],
//!   [`crate::catalog::AppState::with_rate_limits`] bypasses env for tests).
//! * `RATE_LIMIT_READS_PER_MIN` — reads per client IP per 60s window (default
//!   [`DEFAULT_READS_PER_MIN`]).
//!
//! An over-limit request is rejected with `429 Too Many Requests`, a
//! `Retry-After: 1` header and a JSON body `{"error":"rate limit exceeded"}`.
//! Unparseable env values fall back to the defaults.
//!
//! ponytail ceilings, upgrade when real traffic demands it:
//! * a single global `Mutex` guards the whole counter map — shard per key or
//!   switch to a token bucket if throughput ever makes it a contention point;
//! * entries are never evicted while a window is open (a distinct identity
//!   per minute is a tiny `(u64, u32)` row) — prune periodically if identity
//!   cardinality grows unbounded;
//! * fixed 60s windows, not sliding — bursty-but-under-window traffic passes.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use tokio::sync::Mutex;

use crate::auth::VerifiedAgent;
use crate::catalog::AppState;

/// Default write quota: 60 verified writes per agent per minute.
pub const DEFAULT_WRITES_PER_MIN: u32 = 60;
/// Default read quota: 120 reads per client IP per minute.
pub const DEFAULT_READS_PER_MIN: u32 = 120;

/// Fixed 60s window — one refill boundary per minute, per the env contract.
const WINDOW_SECS: u64 = 60;

/// Per-identity fixed-window counter: key -> (window start secs, hits).
///
/// One `RateLimiter` per tier, each with its own quota; keys are namespaced
/// (`w:` for writes, `r:` for reads) so tiers never share a window.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, (u64, u32)>>>,
    limit: u32,
}

impl RateLimiter {
    pub(crate) fn new(limit: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            limit,
        }
    }

    /// Count one hit for `key`. Returns `true` when still within quota.
    async fn check(&self, key: &str) -> bool {
        let now = now_secs();
        let mut windows = self.inner.lock().await;
        let entry = windows.entry(key.to_string()).or_insert((now, 0));
        if now >= entry.0 + WINDOW_SECS {
            *entry = (now, 0); // fresh window: counter clears
        }
        entry.1 += 1;
        entry.1 <= self.limit
    }
}

/// Write-tier limiter. Runs *inside* [`auth::require_signed`] (see
/// `router()` in `lib.rs`), so the verified agent id extension is always
/// present; a request with no verified identity is passed through untouched.
pub async fn limit_writes(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(agent) = request.extensions().get::<VerifiedAgent>() else {
        // ponytail: nothing to attribute — `require_signed` (outer) has
        // already rejected unverified requests before we ever run.
        return next.run(request).await;
    };
    if state.write_limiter.check(&format!("w:{}", agent.0)).await {
        next.run(request).await
    } else {
        rate_limited()
    }
}

/// Read-tier limiter for public routes, keyed by client IP (see module docs
/// for the key-preference order and its trust caveat).
pub async fn limit_reads(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if state
        .read_limiter
        .check(&format!("r:{}", client_key(&request)))
        .await
    {
        next.run(request).await
    } else {
        rate_limited()
    }
}

/// Client identity for read throttling, in order of trust.
fn client_key(request: &Request) -> String {
    if let Some(peer) = request.extensions().get::<ConnectInfo<SocketAddr>>() {
        return peer.0.ip().to_string();
    }
    if let Some(forwarded) = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
    {
        let ip = forwarded.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    // ponytail: oneshot tests (and any server without connect-info) share one
    // key deliberately — per-IP limiting is a proxy/deployment concern.
    "local".to_string()
}

/// `429` + `Retry-After: 1` + the documented JSON error body.
fn rate_limited() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "1")],
        Json(json!({ "error": "rate limit exceeded" })),
    )
        .into_response()
}

/// Read an env var as `u32`, falling back to `default` when unset/unparseable.
pub(crate) fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
