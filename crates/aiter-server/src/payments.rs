//! Razorpay payments rail (issues #18, #23).
//!
//! Minimal Razorpay Orders API client + environment-driven config. Sandbox is
//! the default mode. Credentials come from `RAZORPAY_KEY_ID` /
//! `RAZORPAY_KEY_SECRET` and are **never** logged: every `Debug` impl and
//! error path in this module redacts the secret.

use std::fmt;

use aiter_core::amount::Currency;
use reqwest::Request;
use serde::Deserialize;
use serde_json::json;

/// Default Razorpay API base URL. Razorpay serves both sandbox (test keys,
/// `rzp_test_…`) and live (`rzp_live_…`) from the same host; the key pair
/// selects the mode. Override with `RAZORPAY_BASE_URL` for gateways/proxies.
const DEFAULT_BASE_URL: &str = "https://api.razorpay.com";

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
}

impl RazorpayConfig {
    /// Load from env vars. Requires `RAZORPAY_KEY_ID` + `RAZORPAY_KEY_SECRET`
    /// (clear error if missing), defaults `RAZORPAY_MODE` to `sandbox`, and
    /// honors a `RAZORPAY_BASE_URL` override.
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
        Ok(RazorpayConfig {
            key_id,
            key_secret,
            mode,
            base_url,
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
            .finish()
    }
}

/// Minimal Razorpay Orders API client (issue #18).
///
/// Basic auth (`Key:Secret`), `POST /v1/orders`, sandbox base URL by default.
/// `Debug` never prints the secret.
pub struct RazorpayClient {
    config: RazorpayConfig,
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
    fn build_order_request(
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
}

impl fmt::Display for RazorpayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RazorpayError::Config(msg) => write!(f, "razorpay config error: {msg}"),
            RazorpayError::Http(msg) => write!(f, "razorpay request failed: {msg}"),
            RazorpayError::Api { status, body } => {
                write!(f, "razorpay API error (HTTP {status}): {body}")
            }
        }
    }
}

impl std::error::Error for RazorpayError {}

fn read_env(name: &str) -> Result<String, RazorpayError> {
    std::env::var(name).map_err(|_| RazorpayError::Config(format!("{name} is required")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiter_core::amount::Currency;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    // --- test helpers ------------------------------------------------------

    /// Set/clear the given env vars for the duration of `f`, restoring prior
    /// values afterwards. Serializes env-mutating tests (Rust test threads run
    /// in parallel, and `std::env` is process-global).
    fn with_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _lock = ENV_LOCK.lock().unwrap();

        struct Restore<'a>(Vec<(&'a str, Option<String>)>);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                for (key, value) in &self.0 {
                    match value {
                        Some(value) => std::env::set_var(key, value.as_str()),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }

        let before = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var(key).ok()))
            .collect::<Vec<_>>();
        let _restore = Restore(before);
        for (key, value) in vars {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        f();
    }

    /// Expected `Authorization` header for basic auth with the given key pair.
    fn basic_auth_value(key_id: &str, key_secret: &str) -> String {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        format!(
            "Basic {}",
            STANDARD.encode(format!("{key_id}:{key_secret}"))
        )
    }

    fn test_client() -> RazorpayClient {
        RazorpayClient::new(RazorpayConfig {
            key_id: "rzp_test_keyid".to_string(),
            key_secret: "rzp_test_secret".to_string(),
            mode: RazorpayMode::Sandbox,
            base_url: "https://api.razorpay.com".to_string(),
        })
    }

    /// Bind a throwaway mock Razorpay server on a random local port.
    async fn mock_server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    // --- (a) request construction -----------------------------------------

    #[test]
    fn order_request_sends_post_to_v1_orders_with_basic_auth_and_body() {
        let req = test_client()
            .build_order_request(499, Currency::INR, Some("receipt_1"))
            .unwrap();
        assert_eq!(req.method().as_str(), "POST");
        assert_eq!(req.url().as_str(), "https://api.razorpay.com/v1/orders");
        let auth = req
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, basic_auth_value("rzp_test_keyid", "rzp_test_secret"));
        let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["amount"], 499);
        assert_eq!(body["currency"], "INR");
        assert_eq!(body["receipt"], "receipt_1");
    }

    #[test]
    fn order_request_omits_receipt_when_none() {
        let req = test_client()
            .build_order_request(100, Currency::USD, None)
            .unwrap();
        let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["amount"], 100);
        assert_eq!(body["currency"], "USD");
        assert!(body.get("receipt").is_none());
    }

    #[test]
    fn client_and_config_debug_redact_secret() {
        let client = test_client();
        let cfg_debug = format!("{:?}", client.config);
        let client_debug = format!("{:?}", client);
        assert!(!cfg_debug.contains("rzp_test_secret"));
        assert!(!client_debug.contains("rzp_test_secret"));
        assert!(cfg_debug.contains("rzp_test_keyid"));
    }

    // --- (b) integration-style against a local mock server ----------------

    #[tokio::test]
    async fn create_order_returns_order_id_from_mock_server() {
        let seen_auth: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let seen_body: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
        let auth = seen_auth.clone();
        let body = seen_body.clone();
        let app = Router::new().route(
            "/v1/orders",
            post(move |req: Request<Body>| {
                let auth = auth.clone();
                let body = body.clone();
                async move {
                    let header = req
                        .headers()
                        .get("authorization")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    *auth.lock().unwrap() = header;
                    *body.lock().unwrap() = serde_json::from_slice(&bytes).unwrap();
                    Json(json!({ "id": "order_mock_123" }))
                }
            }),
        );
        let base_url = mock_server(app).await;
        let client = RazorpayClient::new(RazorpayConfig {
            key_id: "rzp_test_keyid".to_string(),
            key_secret: "rzp_test_secret".to_string(),
            mode: RazorpayMode::Sandbox,
            base_url,
        });
        let order_id = client
            .create_order(499, Currency::INR, Some("receipt_1"))
            .await
            .unwrap();
        assert_eq!(order_id, "order_mock_123");
        assert_eq!(
            *seen_auth.lock().unwrap(),
            basic_auth_value("rzp_test_keyid", "rzp_test_secret")
        );
        let body = seen_body.lock().unwrap().clone();
        assert_eq!(body["amount"], 499);
        assert_eq!(body["currency"], "INR");
        assert_eq!(body["receipt"], "receipt_1");
    }

    #[tokio::test]
    async fn api_error_reports_status_and_never_leaks_secret() {
        let app = Router::new().route(
            "/v1/orders",
            post(|| async { (StatusCode::UNAUTHORIZED, "bad or missing keys") }),
        );
        let base_url = mock_server(app).await;
        let client = RazorpayClient::new(RazorpayConfig {
            key_id: "rzp_test_keyid".to_string(),
            key_secret: "rzp_test_secret".to_string(),
            mode: RazorpayMode::Sandbox,
            base_url,
        });
        let err = client
            .create_order(100, Currency::INR, None)
            .await
            .unwrap_err();
        assert!(matches!(&err, RazorpayError::Api { status: 401, .. }));
        let debug = format!("{err:?}");
        assert!(debug.contains("401"));
        assert!(!debug.contains("rzp_test_secret"));
    }

    #[tokio::test]
    async fn transport_error_never_leaks_secret() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // nothing is listening anymore -> connection refused
        let client = RazorpayClient::new(RazorpayConfig {
            key_id: "rzp_test_keyid".to_string(),
            key_secret: "rzp_test_secret".to_string(),
            mode: RazorpayMode::Sandbox,
            base_url: format!("http://{addr}"),
        });
        let err = client
            .create_order(100, Currency::INR, None)
            .await
            .unwrap_err();
        assert!(matches!(&err, RazorpayError::Http(_)));
        assert!(!format!("{err:?}").contains("rzp_test_secret"));
    }

    // --- (c) config --------------------------------------------------------

    const KEY_ID: (&str, Option<&str>) = ("RAZORPAY_KEY_ID", Some("rzp_test_keyid"));
    const KEY_SECRET: (&str, Option<&str>) = ("RAZORPAY_KEY_SECRET", Some("rzp_test_secret"));
    const MODE: (&str, Option<&str>) = ("RAZORPAY_MODE", None);
    const BASE_URL: (&str, Option<&str>) = ("RAZORPAY_BASE_URL", None);

    #[test]
    fn config_defaults_to_sandbox_mode_and_api_base() {
        with_env(&[KEY_ID, KEY_SECRET, MODE, BASE_URL], || {
            let cfg = RazorpayConfig::from_env().unwrap();
            assert_eq!(cfg.key_id, "rzp_test_keyid");
            assert_eq!(cfg.key_secret, "rzp_test_secret");
            assert_eq!(cfg.mode, RazorpayMode::Sandbox);
            assert_eq!(cfg.base_url, "https://api.razorpay.com");
        });
    }

    #[test]
    fn config_switches_mode_via_env() {
        with_env(
            &[
                KEY_ID,
                KEY_SECRET,
                ("RAZORPAY_MODE", Some("live")),
                BASE_URL,
            ],
            || {
                assert_eq!(RazorpayConfig::from_env().unwrap().mode, RazorpayMode::Live);
            },
        );
        with_env(
            &[
                KEY_ID,
                KEY_SECRET,
                ("RAZORPAY_MODE", Some("sandbox")),
                BASE_URL,
            ],
            || {
                assert_eq!(
                    RazorpayConfig::from_env().unwrap().mode,
                    RazorpayMode::Sandbox
                );
            },
        );
    }

    #[test]
    fn config_rejects_unknown_mode() {
        with_env(
            &[
                KEY_ID,
                KEY_SECRET,
                ("RAZORPAY_MODE", Some("prod")),
                BASE_URL,
            ],
            || {
                let err = RazorpayConfig::from_env().unwrap_err();
                assert!(matches!(&err, RazorpayError::Config(_)));
                let msg = err.to_string();
                assert!(msg.contains("RAZORPAY_MODE"));
                assert!(msg.contains("sandbox"));
                assert!(msg.contains("live"));
            },
        );
    }

    #[test]
    fn config_missing_key_id_is_clear_error() {
        with_env(
            &[("RAZORPAY_KEY_ID", None), KEY_SECRET, MODE, BASE_URL],
            || {
                let err = RazorpayConfig::from_env().unwrap_err();
                assert!(err.to_string().contains("RAZORPAY_KEY_ID"));
            },
        );
    }

    #[test]
    fn config_missing_key_secret_is_clear_error() {
        with_env(
            &[KEY_ID, ("RAZORPAY_KEY_SECRET", None), MODE, BASE_URL],
            || {
                let err = RazorpayConfig::from_env().unwrap_err();
                assert!(err.to_string().contains("RAZORPAY_KEY_SECRET"));
            },
        );
    }

    #[test]
    fn config_base_url_override_wins() {
        with_env(
            &[
                KEY_ID,
                KEY_SECRET,
                MODE,
                ("RAZORPAY_BASE_URL", Some("http://localhost:1234")),
            ],
            || {
                let cfg = RazorpayConfig::from_env().unwrap();
                assert_eq!(cfg.base_url, "http://localhost:1234");
                assert_eq!(cfg.mode, RazorpayMode::Sandbox);
            },
        );
    }
}
