//! `aiter-cli` — example agent client for an AITER merchant (issue #29).
//!
//! One command buys a product and prints the buyer's payment link:
//!
//! ```text
//! cargo run --bin aiter-cli -- --base http://localhost:8080 [PRODUCT_ID] [QTY]
//! ```
//!
//! The flow (see [`aiter_server::cli::run_flow`]): discover the catalog, build
//! a cart, create a checkout session, complete it into an order, then mint a
//! payment link and print its `short_url`. Every write is signed with the
//! merchant's well-known **demo agent** keypair (fixed public seed — see
//! [`aiter_server::catalog::DEMO_AGENT_ID`]), which `AppState::default()`
//! pre-registers, so the demo runs against a fresh server with zero setup.

use std::process::ExitCode;

use aiter_server::catalog::DEMO_AGENT_ID;
use aiter_server::cli::{run_flow, CliError, FlowResult};

/// Print usage and exit with code 2.
fn usage() -> ! {
    eprintln!("usage: aiter-cli [--base BASE_URL] [PRODUCT_ID] [QTY]");
    eprintln!();
    eprintln!("Runs the example agent flow against an AITER merchant (issue #29):");
    eprintln!("discover catalog -> signed cart -> signed checkout session -> signed");
    eprintln!("complete -> signed payment link, then prints the buyer's short_url.");
    eprintln!();
    eprintln!("options:");
    eprintln!("  --base BASE_URL  merchant origin (default: http://localhost:8080)");
    eprintln!("  PRODUCT_ID       product to buy (default: first catalog item)");
    eprintln!("  QTY              line quantity, >= 1 (default: 1)");
    eprintln!();
    eprintln!("Every write is signed as the well-known demo agent '{DEMO_AGENT_ID}'");
    eprintln!("(fixed public seed); the server must have registered it —");
    eprintln!("AppState::default() does. Payment links need RAZORPAY_KEY_ID and");
    eprintln!("RAZORPAY_KEY_SECRET (sandbox keys) at runtime.");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut base = "http://localhost:8080".to_string();
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--base" => {
                base = iter.next().unwrap_or_else(|| usage());
            }
            "-h" | "--help" => usage(),
            flag if flag.starts_with("--") => {
                eprintln!("aiter-cli: unknown flag {flag}");
                usage();
            }
            value => positional.push(value.to_string()),
        }
    }

    let product_id = positional.first().map(String::as_str);
    let qty: u32 = match positional.get(1) {
        Some(raw) => match raw.parse::<u32>() {
            Ok(qty) if qty >= 1 => qty,
            _ => {
                eprintln!("aiter-cli: invalid QTY '{raw}' (must be an integer >= 1)");
                usage();
            }
        },
        None => 1,
    };

    let http = reqwest::Client::new();
    match run_flow(&http, &base, product_id, qty).await {
        Ok(flow) => {
            print_result(&flow);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("aiter-cli: {err}");
            if err.is_auth_failure() {
                eprintln!();
                eprintln!("hint: the merchant rejected a write (401/403). Is the demo agent");
                eprintln!("      registered? The server's AppState::default() registers");
                eprintln!("      '{DEMO_AGENT_ID}' (fixed public seed); this client signs with");
                eprintln!("      that same keypair.");
            }
            if let CliError::Http { status: 503, .. } = &err {
                eprintln!();
                eprintln!("hint: payment links need Razorpay credentials; set RAZORPAY_KEY_ID");
                eprintln!("      and RAZORPAY_KEY_SECRET (and optionally RAZORPAY_BASE_URL)");
                eprintln!("      before running.");
            }
            ExitCode::FAILURE
        }
    }
}

/// Print the demo result: every id the flow produced plus the payment link.
fn print_result(flow: &FlowResult) {
    println!("product:      {}", flow.product_id);
    println!("cart:         {}", flow.cart_id);
    println!("session:      {}", flow.session_id);
    println!("order:        {}", flow.order_id);
    println!();
    println!("payment link: {}", flow.short_url);
}
