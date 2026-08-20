//! AITER COMMERCE — thin HTTP server.
//!
//! Exposes the agent-facing + merchant-facing surface. Keep this crate thin:
//! protocol and business logic live in `aiter-core`.
//!
//! # Catalog surface
//!
//! The catalog endpoints reuse `aiter-core`'s `Product` type and serve from
//! shared state:
//!
//! * `GET /catalog/products` — stable, id-ordered JSON list of `Product` with
//!   `?limit=&offset=` pagination, `?tag=` filter, and `?search=` keyword
//!   search ranked by title match. The response is a JSON array of products,
//!   each shaped `{id, title, price:{units,currency}, description, tags,
//!   image_url?, available_qty, variant?}`.
//! * `GET /catalog/products/{id}` — single product; `404` when unknown.
//! * `GET /.well-known/agent-card.json` — A2A-style merchant discovery card.
//! * `GET /llms.txt` — deterministic, LLM-readable plain-text catalog export.

use std::net::SocketAddr;
use std::sync::Arc;

use aiter_core::amount::{Amount, Currency};
use aiter_core::catalog::Product;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{routing::get, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

/// Shared application state: the in-memory product catalog in stable (`id`)
/// order. Wrapped in `Arc` so it is cheap to share across all handlers.
#[derive(Clone)]
struct AppState {
    catalog: Arc<Vec<Product>>,
}

impl AppState {
    /// Build state with a small inline seed catalog. Deterministic: products
    /// are stored sorted by `id` so every feed/export is stable.
    fn demo() -> Self {
        let mut products = vec![
            Product {
                id: "cold-brew".into(),
                title: "Cold Brew".into(),
                price: Amount::new(650, Currency::USD),
                description: "Slow-steeped, served over ice.".into(),
                tags: vec!["coffee".into(), "drink".into(), "cold".into()],
                image_url: None,
                available_qty: 40,
                variant: None,
            },
            Product {
                id: "espresso".into(),
                title: "Espresso".into(),
                price: Amount::new(350, Currency::USD),
                description: "A single, concentrated shot.".into(),
                tags: vec!["coffee".into(), "drink".into(), "hot".into()],
                image_url: None,
                available_qty: 60,
                variant: None,
            },
            Product {
                id: "latte".into(),
                title: "Caffè Latte".into(),
                price: Amount::new(480, Currency::USD),
                description: "Espresso with steamed milk.".into(),
                tags: vec!["coffee".into(), "drink".into(), "hot".into()],
                image_url: None,
                available_qty: 50,
                variant: None,
            },
            Product {
                id: "mug".into(),
                title: "Coffee Mug".into(),
                price: Amount::new(1200, Currency::USD),
                description: "Merch: a branded 12 oz mug.".into(),
                tags: vec!["merch".into()],
                image_url: None,
                available_qty: 15,
                variant: None,
            },
        ];
        products.sort_by(|a, b| a.id.cmp(&b.id));
        AppState {
            catalog: Arc::new(products),
        }
    }
}

/// Query parameters for the catalog feed.
#[derive(Debug, Default, Deserialize)]
struct FeedParams {
    /// Max number of products to return (default: all remaining).
    limit: Option<usize>,
    /// Number of products to skip (after any tag/search filtering).
    offset: Option<usize>,
    /// Filter to products carrying this tag (case-insensitive).
    tag: Option<String>,
    /// Free-text keyword search; results ranked by title match.
    search: Option<String>,
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(service_info))
        .route("/agentic/health", get(health))
        .route("/catalog/products", get(catalog_feed))
        .route("/catalog/products/{id}", get(product_lookup))
        .route("/llms.txt", get(llms_txt))
        .route("/.well-known/agent-card.json", get(agent_card))
        .with_state(state)
}

/// Service identity for any agent or client that discovers us.
async fn service_info() -> Json<Value> {
    Json(json!({
        "name": aiter_core::NAME,
        "version": aiter_core::VERSION,
        "repo": "https://github.com/DeathSurfing/aiter-commerce",
        "agentic": true,
    }))
}

/// Liveness + version probe.
async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": aiter_core::VERSION,
    }))
}

/// Catalog feed: stable, id-ordered JSON list of [`Product`] with optional
/// pagination (`?limit`, `?offset`), tag filter (`?tag`), and keyword search
/// (`?search`) that ranks results by title match.
async fn catalog_feed(
    State(state): State<AppState>,
    Query(params): Query<FeedParams>,
) -> Json<Vec<Product>> {
    let catalog = state.catalog.as_ref();

    let mut items: Vec<&Product> = catalog.iter().collect();

    // Tag filter (case-insensitive).
    if let Some(tag) = params.tag.as_deref() {
        let tag = tag.trim();
        if !tag.is_empty() {
            items.retain(|p| p.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)));
        }
    }

    // Keyword search: keep matches, rank by title over tags.
    let query = params
        .search
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);
    if let Some(q) = query {
        items.retain(|p| title(p).contains(&q) || tag_match(p, &q));
        items.sort_by(|a, b| rank(a, &q).cmp(&rank(b, &q)).then_with(|| a.id.cmp(&b.id)));
    }

    // Pagination over the (possibly filtered/ranked) stable list.
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(items.len());
    let page = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect();

    Json(page)
}

// --- search ranking helpers -------------------------------------------------

fn title(p: &Product) -> String {
    p.title.to_lowercase()
}

fn tag_match(p: &Product, q: &str) -> bool {
    p.tags.iter().any(|t| t.to_lowercase().contains(q))
}

/// Lower rank is better. Exact title match, then title prefix, then title
/// substring, then tag-only match.
fn rank(p: &Product, q: &str) -> usize {
    let t = title(p);
    if t == *q {
        0
    } else if t.starts_with(q) {
        1
    } else if t.contains(q) {
        2
    } else {
        3
    }
}

/// Single-product lookup; `404` for an unknown id.
async fn product_lookup(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Product>, StatusCode> {
    state
        .catalog
        .iter()
        .find(|p| p.id == id)
        .map(|p| Json(p.clone()))
        .ok_or(StatusCode::NOT_FOUND)
}

/// Deterministic, LLM-readable plain-text export of the catalog.
///
/// Format (stable, id-ordered, one product per line):
///   `## Products` header, then per line:
///   `- {id} | {title} | {units} {currency-code} | qty {available_qty} | tags: {tag, ...}`
/// All prices are integer minor units; `currency` is an ISO 4217 code.
async fn llms_txt(State(state): State<AppState>) -> impl IntoResponse {
    let mut out = String::new();
    out.push_str(&format!("# {} — product catalog\n\n", aiter_core::NAME));
    out.push_str("> Machine-generated, LLM-readable listing of this merchant's catalog.\n");
    out.push_str("> Source: GET /catalog/products (stable, id-ordered). Prices are integer\n");
    out.push_str("> minor units; currency is an ISO 4217 code. See README for the full\n");
    out.push_str("> /catalog/products JSON schema.\n\n");
    out.push_str("## Products\n");
    for p in state.catalog.iter() {
        let tags = if p.tags.is_empty() {
            "-".to_string()
        } else {
            p.tags.join(", ")
        };
        out.push_str(&format!(
            "- {} | {} | {} {} | qty {} | tags: {}\n",
            p.id,
            p.title,
            p.price.units,
            p.price.currency.code(),
            p.available_qty,
            tags
        ));
    }
    let headers = [(
        axum::http::header::CONTENT_TYPE,
        "text/plain; charset=utf-8",
    )];
    (headers, out)
}

/// A2A-style merchant discovery card. Advertises the endpoints this server
/// actually implements.
async fn agent_card() -> Json<Value> {
    Json(json!({
        "protocolVersion": "0.2",
        "name": aiter_core::NAME,
        "description": "AITER COMMERCE merchant agent card: catalog, search, discovery, llms.",
        "url": "http://localhost:8080",
        "capabilities": {
            "skills": ["catalog", "search", "discovery", "llms"],
            "endpoints": [
                {"path": "/catalog/products", "method": "GET",
                 "description": "Stable paginated catalog feed; ?limit, ?offset, ?tag, ?search."},
                {"path": "/catalog/products/{id}", "method": "GET",
                 "description": "Single product by id; 404 when unknown."},
                {"path": "/llms.txt", "method": "GET",
                 "description": "Deterministic plain-text catalog export for LLMs."},
                {"path": "/.well-known/agent-card.json", "method": "GET",
                 "description": "This discovery agent card."},
                {"path": "/agentic/health", "method": "GET",
                 "description": "Liveness and version probe."},
            ],
        },
    }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aiter_server=info,tower_http=info".into()),
        )
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let app = router(AppState::demo());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");
    tracing::info!("aiter-server listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value as JsonValue;
    use tower::ServiceExt;

    fn app() -> Router {
        router(AppState::demo())
    }

    async fn get(path: &str) -> (StatusCode, axum::body::Bytes) {
        let resp = app()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("call router");
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        (status, body)
    }

    async fn get_products(path: &str) -> Vec<Product> {
        let (status, body) = get(path).await;
        assert_eq!(status, StatusCode::OK, "expected 200 for {path}");
        serde_json::from_slice(&body).expect("parse product list")
    }

    #[tokio::test]
    async fn feed_returns_stable_id_ordered_list() {
        let all = get_products("/catalog/products").await;
        let ids: Vec<&str> = all.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["cold-brew", "espresso", "latte", "mug"]);
        // Must deserialize as real aiter-core Products with prices.
        assert!(all.iter().all(|p| p.validate().is_ok()));
    }

    #[tokio::test]
    async fn feed_pagination_and_tag_filter() {
        // Pagination.
        let page1 = get_products("/catalog/products?limit=2&offset=0").await;
        assert_eq!(
            page1.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["cold-brew", "espresso"]
        );
        let page2 = get_products("/catalog/products?limit=2&offset=2").await;
        assert_eq!(
            page2.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["latte", "mug"]
        );
        // Tag filter.
        let coffee = get_products("/catalog/products?tag=coffee").await;
        assert_eq!(
            coffee.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["cold-brew", "espresso", "latte"]
        );
        let merch = get_products("/catalog/products?tag=merch").await;
        assert_eq!(merch.len(), 1);
        assert_eq!(merch[0].id, "mug");
    }

    #[tokio::test]
    async fn lookup_returns_product_or_404() {
        let (status, body) = get("/catalog/products/espresso").await;
        assert_eq!(status, StatusCode::OK);
        let p: Product = serde_json::from_slice(&body).expect("parse product");
        assert_eq!(p.id, "espresso");
        assert_eq!(p.price.units, 350);

        let (status, _) = get("/catalog/products/does-not-exist").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn search_ranks_title_matches_above_tag_matches() {
        // "latte" matches only the latte title.
        let latte = get_products("/catalog/products?search=latte").await;
        assert_eq!(latte.len(), 1);
        assert_eq!(latte[0].id, "latte");
        // "coffee" is in the mug's title and the coffee tag of the drinks; the
        // title match ("Coffee Mug") must rank above every tag-only match.
        let coffee = get_products("/catalog/products?search=coffee").await;
        let ids: Vec<&str> = coffee.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids[0], "mug", "title match should rank first: {ids:?}");
        // Tag-only matches all follow, stable id-ordered.
        let tail = &ids[1..];
        let mut tail_sorted = tail.to_vec();
        tail_sorted.sort_unstable();
        assert_eq!(
            tail,
            tail_sorted.as_slice(),
            "tag matches id-ordered: {ids:?}"
        );
    }

    #[tokio::test]
    async fn llms_txt_is_deterministic_plain_text() {
        let (status, body) = get("/llms.txt").await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(body.to_vec()).expect("utf8 text");
        for id in ["cold-brew", "espresso", "latte", "mug"] {
            assert!(text.contains(id), "llms.txt missing {id}");
        }
        assert!(text.contains("## Products"));
        // Deterministic: a second request yields identical bytes.
        let (_, body2) = get("/llms.txt").await;
        assert_eq!(body, body2);
    }

    #[tokio::test]
    async fn agent_card_is_valid_json_with_capabilities() {
        let (status, body) = get("/.well-known/agent-card.json").await;
        assert_eq!(status, StatusCode::OK);
        let card: JsonValue = serde_json::from_slice(&body).expect("valid json");
        assert!(card["name"].as_str().is_some());
        let skills = card["capabilities"]["skills"]
            .as_array()
            .expect("capabilities.skills array");
        let names: Vec<&str> = skills.iter().filter_map(|s| s.as_str()).collect();
        for cap in ["catalog", "search", "discovery", "llms"] {
            assert!(names.contains(&cap), "agent card missing capability {cap}");
        }
        assert!(
            card["capabilities"]["endpoints"]
                .as_array()
                .map(|e| !e.is_empty())
                .unwrap_or(false),
            "agent card should advertise endpoints"
        );
    }
}
