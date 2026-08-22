//! Example agent client — the `aiter-cli` flow (issue #29).
//!
//! [`run_flow`] drives the full demo against any AITER merchant over HTTP and
//! is shared by the `aiter-cli` binary (`bin/aiter-cli.rs`) and the
//! integration tests (`tests/cli.rs`): discover the catalog, build a cart,
//! create a checkout session, complete it into an order, then mint a payment
//! link and report its `short_url`.
//!
//! Every write is signed with the merchant's well-known **demo agent**
//! keypair ([`crate::catalog::demo_agent`]) — the same fixed public seed the
//! server registers in [`crate::catalog::AppState`] — so a fresh
//! `AppState::default()` accepts the whole flow out of the box. The demo key
//! is deliberately public: it exists for demos and tests only.

use std::fmt;

use serde_json::{json, Value};

use crate::auth::{AGENT_ID_HEADER, SIGNATURE_HEADER};
use crate::catalog::demo_agent;

/// A request built exactly as the CLI sends it: the headers proving
/// authorship plus the body bytes the signature covers. `uri` is the
/// origin-form path served by the merchant (`/carts`, ...) — which is what
/// the server reconstructs as the signed target URI.
pub struct SignedRequest {
    /// HTTP method the signature covers.
    pub method: &'static str,
    /// Origin-form request path (as signed).
    pub uri: String,
    /// Exact body bytes the signature covers.
    pub body: String,
    /// Value for the `x-agent-id` header.
    pub agent_id_header: String,
    /// JSON-serialized signature envelope for the `x-request-signature`
    /// header.
    pub signature_header: String,
}

/// Build a signed write request as the demo agent (issue #29).
///
/// This is the CLI's single request-building entry point: it signs method,
/// target URI, body digest, timestamp and agent id exactly the way
/// [`crate::auth::require_signed`] verifies them, so anything built here is
/// accepted by a server whose [`crate::catalog::AppState`] registered the
/// demo agent (as `AppState::default()` does).
pub fn build_signed_request(uri: &str, body: Value) -> SignedRequest {
    let (keypair, identity) = demo_agent();
    let body_str = body.to_string();
    let signature = keypair.sign_request(&identity.id, "POST", uri, body_str.as_bytes(), now());
    SignedRequest {
        method: "POST",
        uri: uri.to_string(),
        body: body_str,
        agent_id_header: identity.id,
        signature_header: serde_json::to_string(&signature).expect("serialize signature"),
    }
}

/// Where a successful demo run stops: the order the checkout produced and the
/// payment link a buyer opens.
#[derive(Debug, Clone)]
pub struct FlowResult {
    /// Product the cart was built around.
    pub product_id: String,
    /// Cart id (`cart-…`).
    pub cart_id: String,
    /// Checkout session id (`cs-…`).
    pub session_id: String,
    /// Order id (`ord-…`).
    pub order_id: String,
    /// Razorpay payment link `short_url` — what the buyer opens to pay.
    pub short_url: String,
}

/// Why a demo run failed.
#[derive(Debug)]
pub enum CliError {
    /// Transport-level failure talking to the merchant.
    Transport(String),
    /// The merchant answered with a non-success status.
    Http { status: u16, body: String },
    /// The catalog contained no products to buy.
    EmptyCatalog,
    /// The requested product id is not in the served catalog.
    UnknownProduct(String),
    /// A response did not have the shape the flow expects.
    Malformed(String),
}

impl CliError {
    /// True when the merchant rejected a write for auth reasons (missing or
    /// invalid signature, or an unregistered agent).
    pub fn is_auth_failure(&self) -> bool {
        matches!(
            self,
            CliError::Http {
                status: 401 | 403,
                ..
            }
        )
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Transport(message) => write!(f, "request failed: {message}"),
            CliError::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            CliError::EmptyCatalog => write!(f, "the catalog is empty — nothing to buy"),
            CliError::UnknownProduct(id) => write!(f, "unknown product: {id}"),
            CliError::Malformed(message) => write!(f, "unexpected server response: {message}"),
        }
    }
}

impl std::error::Error for CliError {}

/// Run the example agent flow against a merchant (issue #29).
///
/// `base_url` is the merchant's origin (e.g. `http://localhost:8080`).
/// `product_id` selects an item from `GET /catalog/products`; `None` buys the
/// first product the catalog returns. `qty` is the line quantity.
///
/// Returns the order id and the payment-link `short_url` (the buyer-facing
/// artifact the demo reports).
pub async fn run_flow(
    http: &reqwest::Client,
    base_url: &str,
    product_id: Option<&str>,
    qty: u32,
) -> Result<FlowResult, CliError> {
    let base = base_url.trim_end_matches('/');

    // 1. Discover the catalog (public read).
    let catalog = get_json(http, &format!("{base}/catalog/products")).await?;
    let items = catalog
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| CliError::Malformed("catalog envelope has no items array".into()))?;
    let chosen = match product_id {
        Some(id) => items
            .iter()
            .find(|item| item["id"] == id)
            .ok_or_else(|| CliError::UnknownProduct(id.to_string()))?,
        None => items.first().ok_or(CliError::EmptyCatalog)?,
    };
    let product_id = chosen["id"]
        .as_str()
        .ok_or_else(|| CliError::Malformed("catalog item has no id".into()))?;

    // 2. Build a signed cart.
    let cart = post_signed(
        http,
        base,
        "/carts",
        json!({
            "currency": "USD",
            "items": [{ "product_id": product_id, "quantity": qty }],
        }),
    )
    .await?;
    let cart_id = str_field(&cart, "id", "/carts")?;

    // 3. Snapshot the cart into a signed checkout session.
    let session = post_signed(
        http,
        base,
        "/checkout_sessions",
        json!({ "cart_id": cart_id }),
    )
    .await?;
    let session_id = str_field(&session, "id", "/checkout_sessions")?;

    // 4. Complete the session (signed) into an order.
    let order = post_signed(
        http,
        base,
        &format!("/checkout_sessions/{session_id}/complete"),
        json!({}),
    )
    .await?;
    let order_id = str_field(&order, "id", "checkout completion")?;

    // 5. Mint a signed payment link for the order and report the short_url.
    let link = post_signed(
        http,
        base,
        &format!("/orders/{order_id}/payment_link"),
        json!({}),
    )
    .await?;
    let short_url = str_field(&link, "short_url", "payment link")?;

    Ok(FlowResult {
        product_id: product_id.to_string(),
        cart_id,
        session_id,
        order_id,
        short_url,
    })
}

/// `GET` a JSON resource (public read, no signature needed).
async fn get_json(http: &reqwest::Client, url: &str) -> Result<Value, CliError> {
    let response = http
        .get(url)
        .send()
        .await
        .map_err(|err| CliError::Transport(err.to_string()))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http {
            status: status.as_u16(),
            body: truncate(&text),
        });
    }
    serde_json::from_str(&text).map_err(|err| CliError::Malformed(format!("{url}: {err}")))
}

/// `POST` a JSON resource signed by the demo agent (issue #29).
async fn post_signed(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    body: Value,
) -> Result<Value, CliError> {
    let signed = build_signed_request(path, body);
    let response = http
        .post(format!("{base}{}", signed.uri))
        .header("content-type", "application/json")
        .header(AGENT_ID_HEADER, &signed.agent_id_header)
        .header(SIGNATURE_HEADER, &signed.signature_header)
        .body(signed.body)
        .send()
        .await
        .map_err(|err| CliError::Transport(err.to_string()))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http {
            status: status.as_u16(),
            body: truncate(&text),
        });
    }
    serde_json::from_str(&text).map_err(|err| CliError::Malformed(format!("{path}: {err}")))
}

/// Pull a required string field out of a JSON response.
fn str_field(value: &Value, field: &str, what: &str) -> Result<String, CliError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CliError::Malformed(format!("{what}: missing or non-string \"{field}\"")))
}

/// Cap error bodies so a hostile/broken server cannot flood the terminal.
fn truncate(text: &str) -> String {
    text.chars().take(512).collect()
}

/// Unix seconds — the timestamp convention used across the checkout flow.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
