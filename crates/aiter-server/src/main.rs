//! AITER COMMERCE — thin HTTP server binary (issue #34 CLI).
//!
//! `aiter-server` is a tiny `run | init | seed` CLI (no clap, plain
//! `std::env::args` matching):
//!
//! * `aiter-server run` (the default when no command is given, so existing
//!   invocations keep working) — seeds shared catalog state, reads the
//!   external config (see [`aiter_server::config`]), binds the listener and
//!   serves with `ConnectInfo`. All routes live in the library crate
//!   (`aiter_server::router`) so integration tests drive them without
//!   opening a socket.
//! * `aiter-server init` — writes a commented `KEY=VALUE` config template
//!   (current defaults) to the configured path ([`aiter_server::config`]);
//!   refuses to overwrite an existing file.
//! * `aiter-server seed` — prints the embedded demo catalog fixture
//!   (`aiter_server::seed::demo_catalog`): product ids + titles, no network
//!   server involved.

use std::net::SocketAddr;

use aiter_server::catalog::{seed_catalog, AppState};
use aiter_server::config;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aiter_server=info,tower_http=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("run");
    let rest: &[String] = args.get(1..).unwrap_or(&[]);
    let exit_code = match command {
        "run" => run(rest).await,
        "init" => init_cmd(rest),
        "seed" => seed_cmd(rest),
        "help" | "--help" | "-h" => {
            print_usage();
            0
        }
        other => {
            eprintln!("aiter-server: unknown command '{other}'");
            print_usage();
            2
        }
    };
    std::process::exit(exit_code);
}

/// `aiter-server run` — the server: load config (defaults < config file <
/// env), bind, serve with ConnectInfo so the read-tier rate limiter keys on
/// the real peer IP (issue #35).
async fn run(_args: &[String]) -> i32 {
    let config = match config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("aiter-server: configuration error: {err}");
            return 1;
        }
    };

    let port = config.port;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    // Resolved Razorpay settings (env-overlaid) feed the lazy payment
    // handlers; `None` would mean legacy env-only reads.
    let state = AppState::with_razorpay_settings(seed_catalog(), Some(config.razorpay.clone()));
    let app = aiter_server::router(state);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("aiter-server: failed to bind {addr}: {err}");
            return 1;
        }
    };
    // The secrets are redacted in Debug (config::RazorpaySettings).
    tracing::info!(
        config_file = %config.config_path.display(),
        config_file_loaded = config.file_loaded,
        razorpay = ?config.razorpay,
        "aiter-server listening on {addr}"
    );
    if let Err(err) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        eprintln!("aiter-server: server error: {err}");
        return 1;
    }
    0
}

/// `aiter-server init` — write a commented `KEY=VALUE` config template with
/// the current defaults to the configured path; never clobbers an existing
/// file (it may hold real secrets).
fn init_cmd(_args: &[String]) -> i32 {
    let path = config::config_path();
    match config::write_template(&path) {
        Ok(()) => {
            println!("wrote config template to {}", path.display());
            println!(
                "edit it, then run `aiter-server run` (or AITER_CONFIG=<path> aiter-server run)"
            );
            0
        }
        Err(config::ConfigError::AlreadyExists(_)) => {
            eprintln!(
                "aiter-server: {} already exists — refusing to overwrite (it may hold secrets)",
                path.display()
            );
            eprintln!("remove it first, or point AITER_CONFIG at a new path");
            1
        }
        Err(err) => {
            eprintln!("aiter-server: init failed: {err}");
            1
        }
    }
}

/// `aiter-server seed` — print the embedded demo catalog fixture
/// (`seed::demo_catalog`): product ids + titles, no server needed.
fn seed_cmd(_args: &[String]) -> i32 {
    match aiter_server::seed::demo_catalog() {
        Ok(catalog) => {
            println!("demo seed catalog ({} products):", catalog.products.len());
            for product in &catalog.products {
                println!("  {} — {}", product.id, product.title);
            }
            0
        }
        Err(err) => {
            eprintln!("aiter-server: failed to load demo catalog: {err}");
            1
        }
    }
}

fn print_usage() {
    println!("aiter-server {} — run | init | seed", aiter_core::VERSION);
    println!();
    println!("USAGE:");
    println!("    aiter-server [COMMAND]");
    println!();
    println!("COMMANDS:");
    println!("    run    Start the HTTP server (default when no command is given)");
    println!("    init   Write a commented KEY=VALUE config template to the");
    println!("           configured path (AITER_CONFIG env var, or ./aiter.env)");
    println!("    seed   Print the embedded demo catalog fixture (ids + titles)");
    println!("    help   Show this usage text");
}
