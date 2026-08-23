//! Agent request-verification middleware (issue #25).
//!
//! Every mutating endpoint requires a request signed by a *registered* agent.
//! The middleware:
//!
//! 1. reads `x-agent-id` (which agent claims the request) and
//!    `x-request-signature` (the JSON-serialized
//!    [`aiter_core::signing::RequestSignature`] envelope),
//! 2. resolves the agent's Ed25519 public key from the shared agent registry
//!    ([`crate::catalog::AppState::agents`]) — unregistered agents get `403`,
//! 3. buffers the (tiny) request body and re-verifies the RFC 9421-style
//!    signature over method, target URI, body digest and agent id using
//!    [`aiter_core::signing::verify_request`] — failures get `401`,
//! 4. on success tags the request with [`VerifiedAgent`] so downstream
//!    handler's `Json` extractor still sees it,
//! 5. enforces signature freshness against this server's own clock — a valid
//!    signature older than [`MAX_SIGNATURE_AGE_SECS`] (or stamped further than
//!    that window into the future) gets `401`, so captured requests cannot be
//!    replayed after the window closes.
//!
//! Requests are tiny JSON documents, so buffering the whole body to recompute
//! the content digest is fine (ponytail: 64 KiB ceiling, oversized bodies are
//! rejected). Verification here covers integrity, identity and freshness; the
//! signing module only verifies the fields themselves.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use aiter_core::signing::{verify_request, RequestSignature};

use crate::catalog::AppState;

/// Header carrying the agent id of a signed request.
pub const AGENT_ID_HEADER: &str = "x-agent-id";
/// Header carrying the JSON-serialized [`RequestSignature`] envelope.
pub const SIGNATURE_HEADER: &str = "x-request-signature";

/// Ceiling for buffered request bodies (checkout payloads are tiny JSON).
const MAX_BODY_BYTES: usize = 64 * 1024;

/// How far from this server's clock a signature's `@created` timestamp may
/// sit and still verify: 5 minutes in either direction (past or future).
/// Replay window for captured requests; one comparison covers both staleness
/// and clock skew.
// ponytail: fixed 5-minute window, env knob if clock skew demands.
pub(crate) const MAX_SIGNATURE_AGE_SECS: u64 = 300;

/// Current unix seconds (mirrors `rate_limit::now_secs`).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Extension inserted by [`require_signed`] so downstream handlers know which
/// verified agent issued the request.
#[derive(Debug, Clone)]
pub struct VerifiedAgent(pub String);

/// axum middleware guarding write routes. See the module docs.
pub async fn require_signed(
    State(st): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let (agent_id, signature) = match extract_signature(&headers) {
        Ok(pair) => pair,
        Err(message) => return unauthorized(&message),
    };

    // Replay guard (issue #67): the signature cryptographically covers its
    // own `@created` timestamp, so a captured request verifies forever unless
    // something compares it to a clock. One symmetric comparison bounds both
    // staleness and future skew.
    if signature.timestamp.abs_diff(now_secs()) > MAX_SIGNATURE_AGE_SECS {
        return unauthorized("signature timestamp outside acceptable window");
    }

    // Only registered agents may mutate state: resolve the public key.
    let identity = {
        let agents = st.agents.lock().await;
        match agents.get(&agent_id) {
            Some(record) => record.identity.clone(),
            None => return forbidden(&format!("unknown agent {agent_id}")),
        }
    };

    // Buffer the whole body so the content digest can be recomputed.
    let (parts, body) = request.into_parts();
    let body_bytes = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return unauthorized("failed to read request body"),
    };

    if let Err(err) = verify_request(
        &identity,
        &signature,
        parts.method.as_str(),
        &target_uri(&parts.uri),
        &body_bytes,
    ) {
        return unauthorized(&format!("invalid request signature: {err:?}"));
    }

    let mut request = Request::from_parts(parts, Body::from(body_bytes));
    request.extensions_mut().insert(VerifiedAgent(agent_id));
    next.run(request).await
}

/// Pull the agent id + signature envelope out of the headers, or name what is
/// missing/malformed so the caller can respond `401`.
fn extract_signature(headers: &HeaderMap) -> Result<(String, RequestSignature), String> {
    let agent_id = headers
        .get(AGENT_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| format!("missing {AGENT_ID_HEADER} header"))?;
    let signature_json = headers
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| format!("missing {SIGNATURE_HEADER} header"))?;
    serde_json::from_str::<RequestSignature>(signature_json)
        .map(|signature| (agent_id, signature))
        .map_err(|_| format!("malformed {SIGNATURE_HEADER} header"))
}

/// Reconstruct the target URI exactly as the signer saw it: absolute-form when
/// the request carries a scheme+authority, otherwise origin-form (path+query).
fn target_uri(uri: &Uri) -> String {
    if let (Some(scheme), Some(authority)) = (uri.scheme(), uri.authority()) {
        let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("");
        format!("{scheme}://{authority}{path}")
    } else {
        uri.path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
            .to_string()
    }
}

fn unauthorized(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
}

fn forbidden(message: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;
    use axum::Router;
    use tower::ServiceExt;

    use crate::test_util::{demo_agent, register_test_agent};

    async fn app() -> Router {
        let st = AppState::default();
        register_test_agent(&st).await;
        crate::router(st)
    }

    /// Drive POST /carts signed by the shared test agent (the identity
    /// [`app`] registers) with an explicit signature timestamp — everything
    /// else about the request is valid.
    async fn post_signed_at(app: &Router, timestamp: u64) -> (StatusCode, serde_json::Value) {
        let (keypair, identity) = demo_agent();
        let uri = "/carts";
        let body_str =
            json!({"currency": "USD", "items": [{"product_id": "p-latte", "quantity": 1}]})
                .to_string();
        let signature = keypair.sign_request(
            &identity.id,
            Method::POST.as_str(),
            uri,
            body_str.as_bytes(),
            timestamp,
        );
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .header(AGENT_ID_HEADER, &identity.id)
            .header(SIGNATURE_HEADER, serde_json::to_string(&signature).unwrap())
            .body(Body::from(body_str))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn fresh_timestamp_passes() {
        let app = app().await;
        let (status, body) = post_signed_at(&app, now_secs()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn stale_signature_is_rejected() {
        let app = app().await;
        let (status, body) = post_signed_at(&app, now_secs() - MAX_SIGNATURE_AGE_SECS - 1).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(
            body["error"],
            "signature timestamp outside acceptable window"
        );
    }

    #[tokio::test]
    async fn far_future_signature_is_rejected_by_the_same_comparison() {
        let app = app().await;
        let (status, body) = post_signed_at(&app, now_secs() + MAX_SIGNATURE_AGE_SECS * 10).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(
            body["error"],
            "signature timestamp outside acceptable window"
        );
    }
}
