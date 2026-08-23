//! Razorpay client: Orders API + payment links + config/error plumbing.
//! Credentials are never logged: every `Debug` impl redacts the secret.

use std::fmt;

use aiter_core::amount::Currency;
use hmac::{Hmac, Mac};
use reqwest::Request;
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;

use crate::catalog::AppState;

/// HMAC-SHA256, the Razorpay webhook signature algorithm (#20).
type HmacSha256 = Hmac<Sha256>;

/// Default Razorpay API base URL. Razorpay serves both sandbox (test keys,
/// `rzp_test_…`) and live (`rzp_live_…`) from the same host; the key pair
/// selects the mode. Override with `RAZORPAY_BASE_URL` for gateways/proxies.
pub(crate) const DEFAULT_BASE_URL: &str = "https://api.razorpay.com";

/// Payment mode: explicit sandbox vs live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RazorpayMode {
    Sandbox,
    Live,
}

impl RazorpayMode {
    fn from_env(value: &str) -> Result<Self, RazorpayError> {
        match value {
            "sandbox" => Ok(RazorpayMode::Sandbox),
            "live" => Ok(RazorpayMode::Live),
            other => Err(RazorpayError::Config(format!(
                "RAZORPAY_MODE must be 'sandbox' or 'live', got '{other}'"
            ))),
        }
    }
}

/// Typed Razorpay configuration, loaded from the environment (issue #23).
///
/// `Debug` redacts `key_secret`: the secret is never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct RazorpayConfig {
    pub key_id: String,
    pub key_secret: String,
    pub mode: RazorpayMode,
    pub base_url: String,
    /// `RAZORPAY_WEBHOOK_SECRET` — required to verify webhooks (#20). `None`
    /// means webhook processing fails closed.
    pub webhook_secret: Option<String>,
}

impl RazorpayConfig {
    /// Load from env vars. Requires `RAZORPAY_KEY_ID` + `RAZORPAY_KEY_SECRET`
    /// (clear error if missing), defaults `RAZORPAY_MODE` to `sandbox`, and
    /// honors a `RAZORPAY_BASE_URL` override. `RAZORPAY_WEBHOOK_SECRET` is
    /// optional here — webhook verification refuses to run without it.
    pub fn from_env() -> Result<Self, RazorpayError> {
        let key_id = read_env("RAZORPAY_KEY_ID")?;
        let key_secret = read_env("RAZORPAY_KEY_SECRET")?;
        let mode = match std::env::var("RAZORPAY_MODE") {
            Ok(value) => RazorpayMode::from_env(&value)?,
            Err(std::env::VarError::NotPresent) => RazorpayMode::Sandbox,
            Err(_) => {
                return Err(RazorpayError::Config(
                    "RAZORPAY_MODE must be a valid UTF-8 string".to_string(),
                ))
            }
        };
        let base_url = match std::env::var("RAZORPAY_BASE_URL") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => DEFAULT_BASE_URL.to_string(),
            Err(_) => {
                return Err(RazorpayError::Config(
                    "RAZORPAY_BASE_URL must be a valid UTF-8 string".to_string(),
                ))
            }
        };
        let webhook_secret = std::env::var("RAZORPAY_WEBHOOK_SECRET").ok();
        Ok(RazorpayConfig {
            key_id,
            key_secret,
            mode,
            base_url,
            webhook_secret,
        })
    }
}

impl fmt::Debug for RazorpayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RazorpayConfig")
            .field("key_id", &self.key_id)
            .field("key_secret", &"<redacted>")
            .field("mode", &self.mode)
            .field("base_url", &self.base_url)
            .field("webhook_secret", &"<redacted>")
            .finish()
    }
}

/// Minimal Razorpay Orders API client (issue #18).
///
/// Basic auth (`Key:Secret`), `POST /v1/orders`, sandbox base URL by default.
/// `Debug` never prints the secret.
pub struct RazorpayClient {
    pub(crate) config: RazorpayConfig,
    http: reqwest::Client,
}

impl RazorpayClient {
    pub fn new(config: RazorpayConfig) -> Self {
        RazorpayClient {
            config,
            http: reqwest::Client::new(),
        }
    }

    /// Build a client from the environment (see [`RazorpayConfig::from_env`]).
    pub fn from_env() -> Result<Self, RazorpayError> {
        Ok(RazorpayClient::new(RazorpayConfig::from_env()?))
    }

    /// Build a client from an [`AppState`]'s resolved Razorpay settings
    /// (issue #34): when the server was started with externally-loaded
    /// config (`defaults < config file < env`), those settings are used;
    /// otherwise (`None` — the legacy path taken by tests and the `mcp` /
    /// `aiter-cli` binaries) this falls back to [`RazorpayClient::from_env`],
    /// i.e. reads `RAZORPAY_*` env vars live per request, exactly as before
    /// the config loader existed.
    pub fn for_state(state: &AppState) -> Result<Self, RazorpayError> {
        match &state.razorpay {
            Some(settings) => Ok(RazorpayClient::new(settings.to_razorpay_config()?)),
            None => RazorpayClient::from_env(),
        }
    }

    /// Create an order and return its `order_id`.
    ///
    /// `amount_minor` is the amount in the currency's smallest unit (paise for
    /// INR). `receipt` is optional and omitted from the request when `None`.
    pub async fn create_order(
        &self,
        amount_minor: i64,
        currency: Currency,
        receipt: Option<&str>,
    ) -> Result<String, RazorpayError> {
        let request = self.build_order_request(amount_minor, currency, receipt)?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|err| RazorpayError::Http(err.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(512)
                .collect::<String>();
            return Err(RazorpayError::Api { status, body });
        }
        let order: OrderResponse = response
            .json()
            .await
            .map_err(|err| RazorpayError::Http(format!("failed to parse order response: {err}")))?;
        Ok(order.id)
    }

    /// Build the `POST /v1/orders` request (exposed for unit-testing the wire
    /// format without a network round-trip).
    pub(crate) fn build_order_request(
        &self,
        amount_minor: i64,
        currency: Currency,
        receipt: Option<&str>,
    ) -> Result<Request, RazorpayError> {
        let url = format!("{}/v1/orders", self.config.base_url.trim_end_matches('/'));
        let mut body = serde_json::Map::new();
        body.insert("amount".to_string(), json!(amount_minor));
        body.insert("currency".to_string(), json!(currency));
        if let Some(receipt) = receipt {
            body.insert("receipt".to_string(), json!(receipt));
        }
        self.http
            .post(url)
            .basic_auth(&self.config.key_id, Some(&self.config.key_secret))
            .json(&serde_json::Value::Object(body))
            .build()
            .map_err(|err| RazorpayError::Http(format!("failed to build request: {err}")))
    }

    /// Verify a Razorpay webhook signature (#20).
    ///
    /// Computes HMAC-SHA256 over the **raw** body bytes keyed with
    /// `RAZORPAY_WEBHOOK_SECRET` and compares it to the hex signature in the
    /// `x-razorpay-signature` header with a constant-time comparison. Fails
    /// closed (config error naming the env var) when no secret is configured
    /// — a webhook can never be processed without one.
    pub fn verify_webhook_signature(
        &self,
        body: &[u8],
        signature: &str,
    ) -> Result<(), RazorpayError> {
        let secret = self.config.webhook_secret.as_deref().ok_or_else(|| {
            RazorpayError::Config(
                "RAZORPAY_WEBHOOK_SECRET is not set; refusing to verify webhook".to_string(),
            )
        })?;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|err| RazorpayError::Http(format!("failed to init HMAC: {err}")))?;
        mac.update(body);
        let expected = to_hex(&mac.finalize().into_bytes());
        if constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
            Ok(())
        } else {
            Err(RazorpayError::Signature(
                "x-razorpay-signature mismatch".to_string(),
            ))
        }
    }

    /// Create a Razorpay payment link and return the `short_url` the buyer
    /// opens (issue #19).
    ///
    /// `amount_minor` is the amount in the currency's smallest unit (paise
    /// for INR). `order_id` is carried in `notes.order_id` so the `payment.paid`
    /// webhook can be reconciled back to our order (#21).
    pub async fn create_payment_link(
        &self,
        amount_minor: i64,
        currency: Currency,
        order_id: Option<&str>,
    ) -> Result<String, RazorpayError> {
        let request = self.build_payment_link_request(amount_minor, currency, order_id)?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|err| RazorpayError::Http(err.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(512)
                .collect::<String>();
            return Err(RazorpayError::Api { status, body });
        }
        let link: PaymentLinkResponse = response.json().await.map_err(|err| {
            RazorpayError::Http(format!("failed to parse payment link response: {err}"))
        })?;
        Ok(link.short_url)
    }

    /// Build the `POST /v1/payment_links` request (exposed for unit-testing
    /// the wire format without a network round-trip).
    pub(crate) fn build_payment_link_request(
        &self,
        amount_minor: i64,
        currency: Currency,
        order_id: Option<&str>,
    ) -> Result<Request, RazorpayError> {
        let url = format!(
            "{}/v1/payment_links",
            self.config.base_url.trim_end_matches('/')
        );
        let mut body = serde_json::Map::new();
        body.insert("amount".to_string(), json!(amount_minor));
        body.insert("currency".to_string(), json!(currency));
        body.insert("accept_partial".to_string(), json!(false));
        if let Some(order_id) = order_id {
            body.insert("notes".to_string(), json!({ "order_id": order_id }));
        }
        self.http
            .post(url)
            .basic_auth(&self.config.key_id, Some(&self.config.key_secret))
            .json(&serde_json::Value::Object(body))
            .build()
            .map_err(|err| RazorpayError::Http(format!("failed to build request: {err}")))
    }
}

impl fmt::Debug for RazorpayClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RazorpayClient")
            .field("config", &self.config)
            .finish()
    }
}

/// Razorpay order creation response — only the id is needed.
#[derive(Deserialize)]
struct OrderResponse {
    id: String,
}

/// Razorpay payment link response — only the `short_url` is needed (#19).
#[derive(Deserialize)]
struct PaymentLinkResponse {
    short_url: String,
}

/// Errors surfaced by the payments module. Never contains credentials: config
/// errors name only the offending env var, transport errors carry no auth
/// material, and API errors expose only the status + (truncated) body.
#[derive(Debug)]
pub enum RazorpayError {
    /// Environment configuration problem (missing/invalid variable).
    Config(String),
    /// HTTP/transport failure talking to Razorpay.
    Http(String),
    /// Razorpay returned a non-success status.
    Api { status: u16, body: String },
    /// Webhook signature verification failed (#20).
    Signature(String),
}

impl fmt::Display for RazorpayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RazorpayError::Config(msg) => write!(f, "razorpay config error: {msg}"),
            RazorpayError::Http(msg) => write!(f, "razorpay request failed: {msg}"),
            RazorpayError::Api { status, body } => {
                write!(f, "razorpay API error (HTTP {status}): {body}")
            }
            RazorpayError::Signature(msg) => {
                write!(f, "razorpay webhook signature verification failed: {msg}")
            }
        }
    }
}

impl std::error::Error for RazorpayError {}

fn read_env(name: &str) -> Result<String, RazorpayError> {
    std::env::var(name).map_err(|_| RazorpayError::Config(format!("{name} is required")))
}

/// Constant-time byte comparison: the loop runs over the full buffer and
/// never short-circuits on the first mismatching byte, so a timing side
/// channel cannot reveal how much of a webhook signature matched (#20).
/// Length mismatch returns `false` immediately (length is not secret for a
/// fixed-size HMAC-SHA256 hex digest).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Lowercase hex encoding (Razorpay webhook signatures are hex HMAC-SHA256).
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
