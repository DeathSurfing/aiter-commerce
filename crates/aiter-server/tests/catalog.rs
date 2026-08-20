//! Integration tests for the catalog REST surface (issues #8–#11, #42–#46).
//!
//! Exercises the real axum router end-to-end via `tower::ServiceExt::oneshot`
//! without binding a socket: catalog feed, lookup, search, discovery card and
//! llms.txt export.

use aiter_core::catalog::{Catalog, Product};
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::Router;
use serde_json::Value;
use tower::ServiceExt;

use aiter_server::catalog::{seed_catalog, AppState};

fn app() -> Router {
    aiter_server::router(AppState::new(seed_catalog()))
}

/// Perform a GET against the app and return (status, body string).
async fn get(path: &str) -> (StatusCode, String) {
    let response = app()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// Perform a GET with an explicit `Host` header (used by the agent-card test).
async fn get_with_host(path: &str, host: &str) -> (StatusCode, String) {
    let response = app()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("host", HeaderValue::from_str(host).unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// Parse a `/catalog/products` response envelope into its `items`.
fn items(body: &str) -> Vec<Product> {
    let v: Value = serde_json::from_str(body).unwrap();
    serde_json::from_value(v["items"].clone()).unwrap()
}

// --- Issue #8: catalog feed -------------------------------------------------

#[tokio::test]
async fn catalog_feed_returns_stable_product_list() {
    let (status, body) = get("/catalog/products").await;
    assert_eq!(status, StatusCode::OK);
    let products = items(&body);
    assert!(!products.is_empty(), "feed should not be empty");
    let ids: Vec<&str> = products.iter().map(|p| p.id.as_str()).collect();
    assert!(ids.contains(&"p-latte"));
    assert!(ids.contains(&"p-espresso"));
    assert!(ids.contains(&"p-coldbrew"));
}

#[tokio::test]
async fn catalog_feed_envelope_reports_totals() {
    let (_, body) = get("/catalog/products").await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["total"], 3);
    assert_eq!(v["offset"], 0);
    assert_eq!(v["has_more"], false);
    assert!(v["limit"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn pagination_limits_and_offsets_deterministically() {
    // Default order is sorted by id, deterministic.
    let (_, body) = get("/catalog/products?limit=2").await;
    let products = items(&body);
    assert_eq!(products.len(), 2);
    assert_eq!(products[0].id, "p-coldbrew");
    assert_eq!(products[1].id, "p-espresso");
    // A partial page and more items exist -> has_more.
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["total"], 3);
    assert_eq!(v["has_more"], true);

    let (_, body) = get("/catalog/products?offset=2").await;
    let products = items(&body);
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].id, "p-latte");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["has_more"], false);
}

#[tokio::test]
async fn tag_filter_returns_only_matching_products() {
    let (_, body) = get("/catalog/products?tag=cold").await;
    let products = items(&body);
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].id, "p-coldbrew");
}

#[tokio::test]
async fn tag_filter_is_case_insensitive() {
    // "Cold" (mixed case) and "COFFEE" (upper) both match, like ?search= (#42).
    let (_, body) = get("/catalog/products?tag=Cold").await;
    let products = items(&body);
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].id, "p-coldbrew");

    let (_, body) = get("/catalog/products?tag=COFFEE").await;
    let products = items(&body);
    assert_eq!(products.len(), 3, "all three products share a `coffee` tag");
}

// --- Issue #9: lookup + search ----------------------------------------------

#[tokio::test]
async fn product_lookup_known_id_is_200_unknown_is_404() {
    let (status, _) = get("/catalog/products/p-latte").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = get("/catalog/products/nope-nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.is_empty());
}

#[tokio::test]
async fn search_ranks_title_matches() {
    let (status, body) = get("/catalog/products?search=latte").await;
    assert_eq!(status, StatusCode::OK);
    let products = items(&body);
    assert!(!products.is_empty(), "search should return matches");
    // Exact title match must rank first.
    assert_eq!(products[0].id, "p-latte");
}

#[tokio::test]
async fn search_matches_description_only() {
    // "steamed" appears only in p-latte's description (#44).
    let (_, body) = get("/catalog/products?search=steamed").await;
    let products = items(&body);
    let ids: Vec<&str> = products.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["p-latte"]);
}

#[tokio::test]
async fn search_ranks_title_above_tag_above_description() {
    // "espresso" matches p-espresso by title AND p-latte by description;
    // the title match must rank first though both appear (#44).
    let (_, body) = get("/catalog/products?search=espresso").await;
    let products = items(&body);
    assert!(!products.is_empty());
    assert_eq!(products[0].id, "p-espresso");

    // "coffee" is a tag on all three; all rank by tag, stable id order.
    let (_, body) = get("/catalog/products?search=coffee").await;
    let products = items(&body);
    let ids: Vec<&str> = products.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["p-coldbrew", "p-espresso", "p-latte"]);
}

// --- Issue #10: merchant discovery profile ----------------------------------

#[tokio::test]
async fn agent_card_is_valid_json_with_capabilities() {
    let (status, body) = get("/.well-known/agent-card.json").await;
    assert_eq!(status, StatusCode::OK);
    let card: Value = serde_json::from_str(&body).unwrap();
    assert!(card["agent"]["name"].is_string());
    assert!(card["capabilities"].is_array());
    let caps: Vec<&str> = card["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    for cap in ["catalog", "search", "discovery", "llms"] {
        assert!(caps.contains(&cap), "missing capability {cap}");
    }
}

#[tokio::test]
async fn agent_card_resolves_absolute_endpoints_from_host() {
    // A fresh agent must be able to call the advertised endpoints without
    // external context: they must be absolute against a resolvable base URL (#46).
    let (status, body) = get_with_host("/.well-known/agent-card.json", "api.example.com").await;
    assert_eq!(status, StatusCode::OK);
    let card: Value = serde_json::from_str(&body).unwrap();
    let service = card["service"].as_str().unwrap();
    assert!(service.starts_with("http"));
    assert!(service.contains("api.example.com"));
    assert!(card["endpoints"]["catalog"]
        .as_str()
        .unwrap()
        .starts_with(service));
    assert!(card["endpoints"]["product_lookup"]
        .as_str()
        .unwrap()
        .starts_with(service));
    // A marketable full URL for the catalog feed.
    let full = card["endpoints"]["catalog"].as_str().unwrap();
    assert!(full.ends_with("/catalog/products"));
    assert!(full.starts_with("http://api.example.com"));
}

// --- Issue #11: llms.txt export ---------------------------------------------

#[tokio::test]
async fn llms_txt_is_deterministic_plain_text() {
    let (s1, b1) = get("/llms.txt").await;
    let (s2, b2) = get("/llms.txt").await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert!(!b1.is_empty());
    // Deterministic: identical on repeat calls.
    assert_eq!(b1, b2);
    // Lists every seeded product with a stable ordering.
    for id in ["p-latte", "p-espresso", "p-coldbrew"] {
        assert!(b1.contains(id), "missing {id} in llms.txt");
    }
    assert!(b1.contains("Latte"));
    assert!(b1.contains("# AITER COMMERCE catalog"));
}

#[tokio::test]
async fn llms_txt_is_spec_shaped() {
    let (_, body) = get("/llms.txt").await;
    // llms.txt convention: a `#` title, a `>` blockquote intro, and markdown
    // links in section(s) (#45).
    assert!(body.starts_with("# AITER COMMERCE catalog\n"));
    assert!(body.contains("> Machine-readable catalog"));
    assert!(body.contains("## Products"));
    assert!(body.contains("- [Caffè Latte](/catalog/products/p-latte): Espresso"));
}

// --- Issue #12: demo seed export -------------------------------------------

#[tokio::test]
async fn seed_catalog_endpoint_serves_full_demo_catalog() {
    let (status, body) = get("/seed/catalog").await;
    assert_eq!(status, StatusCode::OK);
    let catalog: Catalog = serde_json::from_str(&body).unwrap();
    assert!(
        catalog.products.len() >= 8,
        "expected >= 8 products, got {}",
        catalog.products.len()
    );
}
