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
//!    handlers know which agent they are acting for, and restores the body so
//!    the handler's `Json` extractor still sees it.
//!
//! Requests are tiny JSON documents, so buffering the whole body to recompute
//! the content digest is fine (ponytail: 64 KiB ceiling, oversized bodies are
//! rejected). Timestamp freshness is the merchant's job (see the signing
//! module docs); verification here covers integrity and identity only.

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
