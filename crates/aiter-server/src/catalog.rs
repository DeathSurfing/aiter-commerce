//! Catalog REST surface (issues #8–#11, #42–#46).
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
//! Returns a paginated **envelope** (`{ items, total, limit, offset, has_more }`)
//! so a client can walk every page and know when to stop. `items` is the array
//! of `aiter_core::Product` objects (the same shape produced by the core
//! `Product` serialization), in stable id-ascending order unless `?search=`
//! re-ranks them. Each element is:
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
//! * `limit`  — max number of items to return on this page (default 25, cap 100).
//! * `offset` — number of items to skip (start of page).
//! * `tag`    — keep only products that carry this tag (case-insensitive).
//! * `search` — keyword matched against title, tags and description; ranks
//!   title matches above tag-only above description-only, then stable id order.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
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

/// Default page size for the catalog feed when `?limit=` is not supplied.
const DEFAULT_PAGE_LIMIT: usize = 25;
/// Hard cap so a single response is always bounded (see #43).
const MAX_PAGE_LIMIT: usize = 100;

/// Shared application state: the in-memory catalog plus the Day-1 checkout
/// stores (carts / sessions / orders). One state type backs the whole router
/// so catalog and checkout handlers share it. Prices are never stored
/// separately: `price_of` resolves them from `products`, so the served
/// catalog and the checkout price source can never diverge.
#[derive(Clone)]
pub struct AppState {
    products: Arc<Vec<Product>>,
    pub(crate) carts: Arc<Mutex<InMemoryStore<String, Cart>>>,
    /// Cart ids that have been cancelled (idempotent, see `POST /carts/{id}/cancel`).
    pub(crate) cancelled_carts: Arc<Mutex<HashSet<String>>>,
    pub(crate) sessions: Arc<Mutex<InMemoryStore<String, CheckoutSession>>>,
    pub(crate) orders: Arc<Mutex<InMemoryStore<String, Order>>>,
    next_id: Arc<AtomicU64>,
}

impl AppState {
    /// Build state from a product list (stored id-ascending) plus fresh, empty
    /// checkout stores. Prices are resolved from the served catalog itself.
    pub fn new(mut products: Vec<Product>) -> Self {
        products.sort_by(|a, b| a.id.cmp(&b.id));
        AppState {
            products: Arc::new(products),
            carts: Arc::new(Mutex::new(InMemoryStore::new())),
            cancelled_carts: Arc::new(Mutex::new(HashSet::new())),
            sessions: Arc::new(Mutex::new(InMemoryStore::new())),
            orders: Arc::new(Mutex::new(InMemoryStore::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Generate the next sequential id for a store (`cart-0`, `cs-1`, ...).
    pub(crate) fn gen_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}-{n}")
    }

    /// Look up a product's unit price in the served catalog.
    pub(crate) fn price_of(&self, id: &str) -> Option<Amount> {
        self.products.iter().find(|p| p.id == id).map(|p| p.price)
    }
}

impl Default for AppState {
    /// Shared demo-state reuse path (issue #28): the seeded catalog plus fresh,
    /// empty checkout stores. Used by the MCP stdio server so it runs against
    /// the same in-memory state as the HTTP router. Prices come from the served
    /// catalog itself (`price_of`) — never a separate price book.
    fn default() -> Self {
        AppState::new(seed_catalog())
    }
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

    // Tag filter (#8, case-insensitive per #42).
    if let Some(tag) = &params.tag {
        let tag = tag.trim();
        items.retain(|p| p.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)));
    }

    // Keyword search (#9): keep matches across title, tags and description,
    // then rank title > tag > description (earlier title position wins),
    // falling back to stable id order within a rank (#44).
    if let Some(q) = &params.search {
        let q = q.to_lowercase();
        items.retain(|p| search_rank(p, &q) > 0);
        items.sort_by(|a, b| {
            search_rank(b, &q)
                .cmp(&search_rank(a, &q))
                .then_with(|| a.id.cmp(&b.id))
        });
    }

    let total = items.len();
    let offset = params.offset.unwrap_or(0);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .min(MAX_PAGE_LIMIT);
    let page: Vec<Product> = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect();
    let has_more = offset + page.len() < total;

    Json(json!({
        "items": page,
        "total": total,
        "limit": limit,
        "offset": offset,
        "has_more": has_more,
    }))
}

/// Relevance score for a search keyword against a product (#44).
///
/// Higher is more relevant. Title matches dominate (scaled by earlier match
/// position), then tag matches, then description matches; non-matches score 0.
fn search_rank(product: &Product, q: &str) -> u64 {
    if let Some(pos) = product.title.to_lowercase().find(q) {
        return 100_000 - pos as u64;
    }
    if product.tags.iter().any(|t| t.to_lowercase().contains(q)) {
        return 1_000;
    }
    if product.description.to_lowercase().contains(q) {
        return 100;
    }
    0
}

/// `GET /catalog/products/{id}` — single product lookup, `404` for unknown (#9).
pub async fn get_product(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.products.iter().find(|p| p.id == id) {
        Some(product) => Json(product).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "product not found"})),
        )
            .into_response(),
    }
}

/// `GET /.well-known/agent-card.json` — A2A-style discovery card (#10).
///
/// Endpoints are advertised as absolute URLs resolved from the request host
/// (honouring `X-Forwarded-Proto`/`X-Forwarded-Host` when present) so a fresh
/// agent can call them without external context (#46).
pub async fn agent_card(headers: HeaderMap) -> Json<Value> {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("host").and_then(|v| v.to_str().ok()))
        .unwrap_or("localhost:8080");
    let base_url = format!("{scheme}://{host}");

    Json(json!({
        "agent": {
            "name": aiter_core::NAME,
            "version": aiter_core::VERSION,
        },
        "url": "https://github.com/DeathSurfing/aiter-commerce",
        "service": base_url,
        "capabilities": [
            "catalog",
            "search",
            "discovery",
            "llms",
            "carts",
            "checkout_sessions",
            "seed",
            "health",
        ],
        "endpoints": {
            "catalog": format!("{base_url}/catalog/products"),
            "product_lookup": format!("{base_url}/catalog/products/{{id}}"),
            "search": format!("{base_url}/catalog/products?search={{query}}"),
            "discovery": format!("{base_url}/.well-known/agent-card.json"),
            "llms": format!("{base_url}/llms.txt"),
            "carts": format!("{base_url}/carts"),
            "checkout_sessions": format!("{base_url}/checkout_sessions"),
            "seed": format!("{base_url}/seed/catalog"),
            "health": format!("{base_url}/agentic/health"),
        },
    }))
}

/// `GET /llms.txt` — deterministic, plain-text catalog export, llms.txt-shaped (#11, #45).
///
/// Follows the de-facto [llms.txt](https://llmstxt.org) convention: a `#` title,
/// a `>` blockquote intro with links, then a `## Products` section of markdown
/// links resolved against the public catalog path. Deterministic and
/// id-ordered so generic llms.txt tools can parse it without `aiter-server`
/// specific logic.
pub async fn llms_txt(State(state): State<AppState>) -> String {
    let mut out = String::new();
    out.push_str("# AITER COMMERCE catalog\n\n");
    out.push_str("> Machine-readable catalog of products available from this merchant.\n");
    out.push_str("> Served in stable (id-ascending) order.\n");
    out.push_str("> - [Browse catalog](/catalog/products)\n");
    out.push_str("> - [Agent card](/.well-known/agent-card.json)\n\n");
    out.push_str("## Products\n\n");

    for product in state.products.iter() {
        let desc = if product.description.is_empty() {
            format_amount(&product.price)
        } else {
            product.description.clone()
        };
        out.push_str(&format!(
            "- [{}](/catalog/products/{}): {}\n",
            product.title, product.id, desc
        ));
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
