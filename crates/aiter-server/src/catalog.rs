//! Catalog REST surface (issues #8–#11).
//!
//! Provides the agent/LLM-facing read model of the merchant catalog:
//!
//! * `GET /catalog/products` — paginated, filterable catalog feed (#8).
//! * `GET /catalog/products/{id}` — single product lookup, `404` unknown (#9).
//! * `GET /.well-known/agent-card.json` — A2A-style discovery card (#10).
//! * `GET /llms.txt` — deterministic, LLM-readable catalog export (#11).
//!
//! State is a shared in-memory product list held in an [`AppState`]. Items are
//! stored id-ordered so every endpoint returns a stable, deterministic order.
//!
//! ## `GET /catalog/products` response schema
//!
//! Returns a JSON **array** of `aiter_core::Product` objects (the same shape
//! produced by the core `Product` serialization), in stable id-ascending order
//! unless `?search=` re-ranks them. Each element is:
//!
//! ```json
//! {
//!   "id": "p-latte",
//!   "title": "Caffè Latte",
//!   "price": { "units": 450, "currency": "USD" },
//!   "description": "Espresso with steamed milk",
//!   "tags": ["hot", "coffee"],
//!   "image_url": null,
//!   "available_qty": 10,
//!   "variant": null
//! }
//! ```
//!
//! Query parameters (all optional):
//! * `limit`  — max number of items to return per page.
//! * `offset` — number of items to skip (start of page).
//! * `tag`    — keep only products that carry this tag.
//! * `search` — keyword; ranks title matches above tag-only matches, then
//!   falls back to stable id order.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use aiter_core::amount::{Amount, Currency};
use aiter_core::cart::Cart;
use aiter_core::catalog::Product;
use aiter_core::checkout::CheckoutSession;
use aiter_core::order::Order;
use aiter_core::store::InMemoryStore;

/// Shared application state: the in-memory catalog plus the Day-1 checkout
/// stores (carts / sessions / orders) and the demo price book. One state type
/// backs the whole router so catalog and checkout handlers share it.
#[derive(Clone)]
pub struct AppState {
    products: Arc<Vec<Product>>,
    pub(crate) carts: Arc<Mutex<InMemoryStore<String, Cart>>>,
    /// Cart ids that have been cancelled (idempotent, see `POST /carts/{id}/cancel`).
    pub(crate) cancelled_carts: Arc<Mutex<HashSet<String>>>,
    pub(crate) sessions: Arc<Mutex<InMemoryStore<String, CheckoutSession>>>,
    pub(crate) orders: Arc<Mutex<InMemoryStore<String, Order>>>,
    /// Demo product price book used to re-derive totals at pricing time.
    prices: Arc<HashMap<String, Amount>>,
    next_id: Arc<AtomicU64>,
}

impl AppState {
    /// Build state from a product list (stored id-ascending) plus fresh, empty
    /// checkout stores and the demo price book.
    pub fn new(mut products: Vec<Product>) -> Self {
        products.sort_by(|a, b| a.id.cmp(&b.id));
        AppState {
            products: Arc::new(products),
            carts: Arc::new(Mutex::new(InMemoryStore::new())),
            cancelled_carts: Arc::new(Mutex::new(HashSet::new())),
            sessions: Arc::new(Mutex::new(InMemoryStore::new())),
            orders: Arc::new(Mutex::new(InMemoryStore::new())),
            prices: Arc::new(default_price_book()),
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Generate the next sequential id for a store (`cart-0`, `cs-1`, ...).
    pub(crate) fn gen_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}-{n}")
    }

    /// Look up a product's unit price in the demo price book.
    pub(crate) fn price_of(&self, id: &str) -> Option<Amount> {
        self.prices.get(id).copied()
    }
}

/// The demo product price book: product id -> unit price in USD.
fn default_price_book() -> HashMap<String, Amount> {
    HashMap::from([
        ("p1".to_string(), Amount::new(100, Currency::USD)), // $1.00
        ("p2".to_string(), Amount::new(350, Currency::USD)), // $3.50
        ("p3".to_string(), Amount::new(25, Currency::USD)),  // $0.25
        ("p4".to_string(), Amount::new(1200, Currency::USD)), // $12.00
        ("p5".to_string(), Amount::new(499, Currency::USD)), // $4.99
    ])
}

/// A small inline catalog used to seed state — no external storage required
/// for the Day-1 read model.
pub fn seed_catalog() -> Vec<Product> {
    vec![
        Product {
            id: "p-latte".to_string(),
            title: "Caffè Latte".to_string(),
            price: Amount::new(450, Currency::USD),
            description: "Espresso with steamed milk.".to_string(),
            tags: vec!["hot".to_string(), "coffee".to_string()],
            image_url: None,
            available_qty: 10,
            variant: None,
        },
        Product {
            id: "p-espresso".to_string(),
            title: "Espresso".to_string(),
            price: Amount::new(300, Currency::USD),
            description: "A single shot of rich espresso.".to_string(),
            tags: vec!["hot".to_string(), "coffee".to_string()],
            image_url: None,
            available_qty: 20,
            variant: None,
        },
        Product {
            id: "p-coldbrew".to_string(),
            title: "Cold Brew".to_string(),
            price: Amount::new(500, Currency::USD),
            description: "Smooth, slow-steeped cold brew coffee.".to_string(),
            tags: vec!["cold".to_string(), "coffee".to_string()],
            image_url: None,
            available_qty: 8,
            variant: None,
        },
    ]
}

/// Query parameters for the catalog feed (`GET /catalog/products`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListParams {
    limit: Option<usize>,
    offset: Option<usize>,
    tag: Option<String>,
    search: Option<String>,
}

/// `GET /catalog/products` — paginated, filtered catalog feed (#8, #9 search).
pub async fn list_products(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Json<Value> {
    let mut items: Vec<&Product> = state.products.iter().collect();

    // Tag filter (#8).
    if let Some(tag) = &params.tag {
        items.retain(|p| p.tags.iter().any(|t| t == tag));
    }

    // Keyword search (#9): keep matches, rank title matches above tag-only
    // matches, then stable id order within a rank.
    if let Some(q) = &params.search {
        let q = q.to_lowercase();
        items.retain(|p| {
            p.title.to_lowercase().contains(&q)
                || p.tags.iter().any(|t| t.to_lowercase().contains(&q))
        });
        items.sort_by(|a, b| {
            let a_title = a.title.to_lowercase().contains(&q);
            let b_title = b.title.to_lowercase().contains(&q);
            b_title.cmp(&a_title).then_with(|| a.id.cmp(&b.id))
        });
    }

    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(usize::MAX);
    let page: Vec<Product> = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect();

    Json(serde_json::to_value(page).expect("products serializable"))
}

/// `GET /catalog/products/{id}` — single product lookup, `404` for unknown (#9).
pub async fn get_product(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.products.iter().find(|p| p.id == id) {
        Some(product) => Json(product).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /.well-known/agent-card.json` — A2A-style discovery card (#10).
pub async fn agent_card() -> Json<Value> {
    Json(json!({
        "agent": {
            "name": aiter_core::NAME,
            "version": aiter_core::VERSION,
        },
        "url": "https://github.com/DeathSurfing/aiter-commerce",
        "capabilities": ["catalog", "search", "discovery", "llms"],
        "endpoints": {
            "catalog": "/catalog/products",
            "product_lookup": "/catalog/products/{id}",
            "search": "/catalog/products?search={query}",
            "discovery": "/.well-known/agent-card.json",
            "llms": "/llms.txt",
        },
    }))
}

/// `GET /llms.txt` — deterministic, plain-text, LLM-readable catalog export (#11).
///
/// Format: a plain-text document with a top-level heading, a short intro, and
/// one section per product (in stable id order). Each product section begins
/// with `# <title> (<id>)` and lists a description, price and tags on following
/// lines — chosen so an LLM can cheaply scan the whole catalog without JSON.
pub async fn llms_txt(State(state): State<AppState>) -> String {
    let mut out = String::new();
    out.push_str("# AITER COMMERCE catalog\n\n");
    out.push_str("Machine-readable catalog of products available from this merchant.\n");
    out.push_str("Served from state in stable (id-ascending) order.\n\n");

    for product in state.products.iter() {
        out.push_str(&format!("# {} ({})\n", product.title, product.id));
        if !product.description.is_empty() {
            out.push_str(&format!("Description: {}\n", product.description));
        }
        out.push_str(&format!("Price: {}\n", format_amount(&product.price)));
        if !product.tags.is_empty() {
            out.push_str(&format!("Tags: {}\n", product.tags.join(", ")));
        }
        out.push_str(&format!("Available: {}\n", product.available_qty));
        out.push('\n');
    }
    out
}

/// Format an [`Amount`] as a readable major-unit string (e.g. `4.50 USD`).
fn format_amount(amount: &Amount) -> String {
    let currency = amount.currency();
    let units = amount.units();
    let exponent = currency.minor_unit_exponent();
    let divisor = 10i64.pow(exponent);
    let major = units / divisor;
    let frac = (units % divisor).abs();
    let code = currency.code();
    if exponent == 0 {
        format!("{major} {code}")
    } else {
        format!("{major}.{:0width$} {code}", frac, width = exponent as usize)
    }
}
