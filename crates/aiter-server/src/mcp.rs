//! MCP stdio binding (issue #28).
//!
//! Exposes catalog lookup and checkout to ChatGPT / Claude / Gemini agents as
//! Model Context Protocol tools (`list_products`, `get_product`,
//! `create_cart`, `complete_checkout`) over newline-delimited JSON-RPC 2.0 on
//! stdin/stdout. The tools share the [`AppState`] and core handlers used by
//! the HTTP router — `src/bin/mcp.rs` is a thin stdio loop around
//! [`handle_request`].
//!
//! Run it: `cargo run --bin mcp` (or point an MCP client at the binary).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use aiter_core::store::StoreError;

use crate::catalog::{AppState, ListParams};
use crate::checkout;
use crate::error::{ApiError, ReserveError};

/// Server name advertised in the `initialize` handshake.
const SERVER_NAME: &str = "aiter-commerce-mcp";
/// Protocol version reported when the client does not request one.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// One MCP tool definition (name + description + JSON-Schema input).
struct McpTool {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
}

/// The four tools: catalog lookup + checkout, wired to the same core handlers
/// as the HTTP router.
fn tools() -> Vec<McpTool> {
    vec![
        McpTool {
            name: "list_products",
            description: "List every product in the catalog with id, title, price and availability.",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        McpTool {
            name: "get_product",
            description: "Look up a single product by id.",
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        },
        McpTool {
            name: "create_cart",
            description: "Create a cart from line items and return its id and totals.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "currency": {"type": "string"},
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "product_id": {"type": "string"},
                                "quantity": {"type": "integer"}
                            },
                            "required": ["product_id", "quantity"]
                        }
                    }
                },
                "required": ["items"]
            }),
        },
        McpTool {
            name: "complete_checkout",
            description: "Check out a cart: snapshot it into a checkout session, mark it paid, and return the order.",
            input_schema: json!({
                "type": "object",
                "properties": {"cart_id": {"type": "string"}},
                "required": ["cart_id"]
            }),
        },
    ]
}

/// Answer one JSON-RPC message. Returns `None` for notifications (messages
/// without an id), which the protocol says must never be answered.
pub async fn handle_request(state: AppState, request: Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let is_notification = request.get("id").is_none();
    let method = request.get("method").and_then(Value::as_str);
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    if is_notification {
        return None; // e.g. `notifications/initialized`
    }
    let response = match method {
        Some("initialize") => success(id, initialize_result(&params)),
        Some("tools/list") => success(id, tools_list()),
        Some("tools/call") => match dispatch(&state, &params).await {
            Ok(result) => success(id, result),
            Err((code, message)) => error(id, code, message),
        },
        Some(other) => error(id, -32601, format!("method not found: {other}")),
        None => error(id, -32600, "invalid request: missing method".to_string()),
    };
    Some(response)
}

/// The JSON-RPC error response for a line that failed to parse.
pub fn handle_parse_error(err: &serde_json::Error) -> Value {
    error(None, -32700, format!("parse error: {err}"))
}

fn initialize_result(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": SERVER_NAME, "version": aiter_core::VERSION},
    })
}

fn tools_list() -> Value {
    let tools: Vec<Value> = tools()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })
        })
        .collect();
    json!({"tools": tools})
}

/// Route a `tools/call` to the matching core handler. Tool failures become
/// JSON-RPC errors (`-32602`), so an MCP client sees a clean error message.
async fn dispatch(state: &AppState, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "missing tool name".to_string()))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let text = match name {
        "list_products" => list_products(state).await,
        "get_product" => get_product(state, &arguments).await,
        "create_cart" => create_cart(state, &arguments).await,
        "complete_checkout" => complete_checkout(state, &arguments).await,
        other => return Err((-32602, format!("unknown tool: {other}"))),
    }
    .map_err(|message| (-32602, message))?;
    Ok(json!({"content": [{"type": "text", "text": text}]}))
}

/// Reuse the HTTP catalog feed handler ([`crate::catalog::list_products`]).
async fn list_products(state: &AppState) -> Result<String, String> {
    let Json(body) =
        crate::catalog::list_products(State(state.clone()), Query(ListParams::default())).await;
    Ok(body.to_string())
}

/// Reuse the HTTP product lookup handler ([`crate::catalog::get_product`]);
/// an unknown id surfaces as a JSON-RPC error instead of a 404.
async fn get_product(state: &AppState, arguments: &Value) -> Result<String, String> {
    let id = string_arg(arguments, "id")?;
    let response = crate::catalog::get_product(State(state.clone()), Path(id)).await;
    if response.status() == StatusCode::NOT_FOUND {
        return Err("product not found".to_string());
    }
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;
    String::from_utf8(bytes.to_vec()).map_err(|e| format!("invalid utf-8 in response: {e}"))
}

/// Reuse the HTTP cart-creation handler ([`crate::checkout::create_cart`]):
/// same store, same id generator, pricing resolved from the served catalog.
async fn create_cart(state: &AppState, arguments: &Value) -> Result<String, String> {
    let request: checkout::CreateCartRequest =
        serde_json::from_value(arguments.clone()).map_err(|e| format!("invalid arguments: {e}"))?;
    let Json(cart) = checkout::create_cart(State(state.clone()), Json(request))
        .await
        .map_err(api_error)?;
    serde_json::to_string(&cart).map_err(|e| format!("serialize: {e}"))
}

/// Check out a cart through the same session/order flow as the HTTP routes:
/// snapshot the cart into a checkout session, then complete it into an order.
async fn complete_checkout(state: &AppState, arguments: &Value) -> Result<String, String> {
    let cart_id = string_arg(arguments, "cart_id")?;
    let session_request: checkout::CreateSessionRequest =
        serde_json::from_value(json!({"cart_id": cart_id}))
            .map_err(|e| format!("invalid arguments: {e}"))?;
    let Json(session) =
        checkout::create_checkout_session(State(state.clone()), Json(session_request))
            .await
            .map_err(api_error)?;
    let Json(order) = checkout::complete_checkout(State(state.clone()), None, Path(session.id))
        .await
        .map_err(api_error)?;
    serde_json::to_string(&order).map_err(|e| format!("serialize: {e}"))
}

fn string_arg(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing argument: {key}"))
}

/// Render an [`ApiError`] as a plain message for the MCP client.
fn api_error(err: ApiError) -> String {
    match err {
        ApiError::NotFound => "not found".to_string(),
        ApiError::Conflict(message) => message,
        ApiError::Store(StoreError::NotFound) => "not found".to_string(),
        ApiError::Store(StoreError::AlreadyExists) => "already exists".to_string(),
        ApiError::Checkout(e) => format!("illegal checkout transition: {e:?}"),
        ApiError::Pricing(e) => format!("unpriced item: {e:?}"),
        ApiError::UnknownProduct(id) => format!("unknown product: {id}"),
        ApiError::CurrencyMismatch {
            product_id,
            expected,
            got,
        } => format!(
            "product {product_id} is priced in {} but the cart is in {}",
            got.code(),
            expected.code()
        ),
        ApiError::Razorpay(e) => e.to_string(),
        ApiError::SpendLimit(message) => message,
        ApiError::ConsentNotFound => "consent not found".to_string(),
        ApiError::Reserve(e) => match e {
            ReserveError::NotActive => "consent is not active".to_string(),
            ReserveError::LimitExceeded { .. } => "spend limit exceeded".to_string(),
            ReserveError::DeviceMismatch => {
                "device_mismatch: confirm re-auth via ?confirm=true".to_string()
            }
            ReserveError::CurrencyMismatch(currency) => {
                format!("currency mismatch: consent limit is in {}", currency.code())
            }
            ReserveError::InvalidAmount(message) => message,
        },
    }
}

fn success(id: Option<Value>, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Option<Value>, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::seed_catalog;

    fn state() -> AppState {
        AppState::new(seed_catalog())
    }

    #[tokio::test]
    async fn initialize_returns_mcp_handshake() {
        let resp = handle_request(
            state(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"},
                },
            }),
        )
        .await
        .expect("initialize is a request, not a notification");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "aiter-commerce-mcp");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_exposes_exactly_the_four_tools() {
        let resp = handle_request(
            state(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await
        .expect("tools/list is a request");
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "list_products",
                "get_product",
                "create_cart",
                "complete_checkout"
            ]
        );
        for tool in resp["result"]["tools"].as_array().unwrap() {
            assert!(
                tool["inputSchema"].is_object(),
                "every tool has an input schema"
            );
        }
    }

    #[tokio::test]
    async fn notifications_are_ignored_and_unknown_methods_error() {
        // notifications/initialized carries no id -> the server stays silent.
        let resp = handle_request(
            state(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert!(resp.is_none(), "notifications must not be answered");

        // Unknown method -> JSON-RPC error -32601.
        let resp = handle_request(
            state(),
            json!({"jsonrpc": "2.0", "id": 9, "method": "bogus"}),
        )
        .await
        .expect("request expects a response");
        assert_eq!(resp["error"]["code"], -32601);

        // A request missing the method field is invalid (-32600).
        let resp = handle_request(state(), json!({"jsonrpc": "2.0", "id": 10}))
            .await
            .expect("request expects a response");
        assert_eq!(resp["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn missing_tool_arguments_return_invalid_params_error() {
        let resp = handle_request(
            state(),
            json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tools/call",
                "params": {"name": "get_product", "arguments": {}},
            }),
        )
        .await
        .expect("tools/call is a request");
        assert_eq!(resp["error"]["code"], -32602);
        let message = resp["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("missing argument: id"),
            "unexpected message: {message}"
        );
    }
}
