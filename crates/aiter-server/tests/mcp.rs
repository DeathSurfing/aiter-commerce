//! MCP stdio integration tests (issue #28).
//!
//! Drives the real `mcp` binary exactly like an MCP client: newline-delimited
//! JSON-RPC 2.0 requests on its stdin, responses on its stdout. Covers the
//! handshake, tool discovery, and a full catalog + checkout purchase flow.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// Spawn the MCP binary with piped stdio. Returns the child so the test can
/// close stdin (EOF) and reap it, plus its stdin/stdout handles.
fn spawn() -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn mcp binary");
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

fn send(stdin: &mut ChildStdin, message: Value) {
    writeln!(stdin, "{message}").expect("write request");
    stdin.flush().expect("flush request");
}

fn recv(stdout: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read response line");
    assert!(!line.is_empty(), "server closed stdout unexpectedly");
    serde_json::from_str(&line).expect("response is one JSON object per line")
}

/// `tools/call` helper: send the request and return the full response.
fn call_tool(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    send(
        stdin,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }),
    );
    recv(stdout)
}

/// The JSON payload an MCP tool returns inside `content[0].text`.
fn tool_text(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool result carries text content")
        .to_string()
}

#[test]
fn mcp_client_exchange_drives_catalog_and_checkout() {
    let (mut child, mut stdin, mut stdout) = spawn();

    // 1. initialize handshake.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "integration-test", "version": "1.0"},
            },
        }),
    );
    let init = recv(&mut stdout);
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "aiter-commerce-mcp");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    // 2. tools/list -> exactly the four expected tools, each with a schema.
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let listing = recv(&mut stdout);
    let names: Vec<&str> = listing["result"]["tools"]
        .as_array()
        .expect("tools array")
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
    for tool in listing["result"]["tools"].as_array().unwrap() {
        assert!(
            tool["inputSchema"].is_object(),
            "every tool has an input schema"
        );
        assert!(
            tool["description"].is_string(),
            "every tool has a description"
        );
    }

    // 3. list_products -> at least one product, including the seeded latte.
    let resp = call_tool(&mut stdin, &mut stdout, 3, "list_products", json!({}));
    assert!(resp.get("error").is_none(), "list_products must succeed");
    let listed: Value = serde_json::from_str(&tool_text(&resp)).unwrap();
    let items = listed["items"].as_array().expect("items array");
    assert!(!items.is_empty(), "catalog must not be empty");
    let ids: Vec<&str> = items.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"p-latte"));

    // 4. get_product on a known id -> the product.
    let resp = call_tool(
        &mut stdin,
        &mut stdout,
        4,
        "get_product",
        json!({"id": "p-latte"}),
    );
    assert!(resp.get("error").is_none(), "known product must resolve");
    let product: Value = serde_json::from_str(&tool_text(&resp)).unwrap();
    assert_eq!(product["title"], "Caffè Latte");
    assert_eq!(product["price"]["units"], 450);

    // 5. get_product on an unknown id -> JSON-RPC error response.
    let resp = call_tool(
        &mut stdin,
        &mut stdout,
        5,
        "get_product",
        json!({"id": "nope"}),
    );
    let err = resp["error"]
        .as_object()
        .expect("missing product is an error");
    assert_eq!(err["code"], -32602);
    assert_eq!(err["message"], "product not found");

    // 6. create_cart with a served-catalog item -> cart id + totals from the
    //    served catalog pricing (p-latte x2 = 900 units, i.e. $9.00).
    let resp = call_tool(
        &mut stdin,
        &mut stdout,
        6,
        "create_cart",
        json!({"items": [{"product_id": "p-latte", "quantity": 2}]}),
    );
    assert!(resp.get("error").is_none(), "create_cart must succeed");
    let cart: Value = serde_json::from_str(&tool_text(&resp)).unwrap();
    let cart_id = cart["id"].as_str().expect("cart id").to_string();
    assert!(cart_id.starts_with("cart-"));
    assert_eq!(cart["totals"]["subtotal"]["units"], 900); // 2 x $4.50 (p-latte)

    // 7. create_cart with an unknown product id -> JSON-RPC error (the HTTP
    //    handler now rejects unknown ids with a 400, never a null-totals cart).
    let resp = call_tool(
        &mut stdin,
        &mut stdout,
        7,
        "create_cart",
        json!({"items": [{"product_id": "p1", "quantity": 1}]}),
    );
    let err = resp["error"]
        .as_object()
        .expect("unknown product must be an error");
    assert_eq!(err["code"], -32602);
    assert_eq!(err["message"], "unknown product: p1");
    assert!(
        resp.get("result").is_none(),
        "an errored call carries an error member, never a result"
    );

    // 8. complete_checkout on that cart -> order in Placed status.
    let resp = call_tool(
        &mut stdin,
        &mut stdout,
        8,
        "complete_checkout",
        json!({"cart_id": cart_id}),
    );
    assert!(resp.get("error").is_none(), "priced cart must check out");
    let order: Value = serde_json::from_str(&tool_text(&resp)).unwrap();
    assert_eq!(order["status"], "Placed");
    assert!(order["checkout_session_id"]
        .as_str()
        .unwrap()
        .starts_with("cs-"));
    assert_eq!(order["totals"]["subtotal"]["units"], 900);

    // 9. complete_checkout on a missing cart -> error response.
    let resp = call_tool(
        &mut stdin,
        &mut stdout,
        9,
        "complete_checkout",
        json!({"cart_id": "cart-nope"}),
    );
    assert!(resp["error"].is_object(), "missing cart is an error");

    // Close stdin so the server sees EOF and exits; reap the child.
    drop(stdin);
    drop(stdout);
    let _ = child.wait();
}
