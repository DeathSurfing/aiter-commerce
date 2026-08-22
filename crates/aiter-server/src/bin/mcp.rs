//! AITER COMMERCE — MCP stdio server binary (issue #28).
//!
//! Newline-delimited JSON-RPC 2.0 on stdin/stdout, backed by the same
//! [`AppState`] and core handlers as the HTTP router. Drive it from an MCP
//! client (ChatGPT / Claude / Gemini) or test it by piping JSON-RPC lines.

use std::io::{BufRead, Write};

use aiter_server::catalog::AppState;
use serde_json::Value;

#[tokio::main]
async fn main() {
    let state = AppState::default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break, // stdin closed
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                let response = aiter_server::mcp::handle_parse_error(&err);
                let _ = writeln!(stdout, "{response}");
                let _ = stdout.flush();
                continue;
            }
        };
        if let Some(response) = aiter_server::mcp::handle_request(state.clone(), request).await {
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}
