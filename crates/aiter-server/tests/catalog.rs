//! Integration tests for the catalog REST surface (issues #8–#11).
//!
//! Exercises the real axum router end-to-end via `tower::ServiceExt::oneshot`
//! without binding a socket: catalog feed, lookup, search, discovery card and
//! llms.txt export.

use aiter_core::catalog::Product;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
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

// --- Issue #8: catalog feed -------------------------------------------------

#[tokio::test]
async fn catalog_feed_returns_stable_product_list() {
    let (status, body) = get("/catalog/products").await;
    assert_eq!(status, StatusCode::OK);
    let products: Vec<Product> = serde_json::from_str(&body).unwrap();
    assert!(!products.is_empty(), "feed should not be empty");
    let ids: Vec<&str> = products.iter().map(|p| p.id.as_str()).collect();
    assert!(ids.contains(&"p-latte"));
    assert!(ids.contains(&"p-espresso"));
    assert!(ids.contains(&"p-coldbrew"));
}

#[tokio::test]
async fn pagination_limits_and_offsets_deterministically() {
    // Default order is sorted by id, deterministic.
    let (_, body) = get("/catalog/products?limit=2").await;
    let products: Vec<Product> = serde_json::from_str(&body).unwrap();
    assert_eq!(products.len(), 2);
    assert_eq!(products[0].id, "p-coldbrew");
    assert_eq!(products[1].id, "p-espresso");

    let (_, body) = get("/catalog/products?offset=2").await;
    let products: Vec<Product> = serde_json::from_str(&body).unwrap();
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].id, "p-latte");
}

#[tokio::test]
async fn tag_filter_returns_only_matching_products() {
    let (_, body) = get("/catalog/products?tag=cold").await;
    let products: Vec<Product> = serde_json::from_str(&body).unwrap();
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].id, "p-coldbrew");
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
    let products: Vec<Product> = serde_json::from_str(&body).unwrap();
    assert!(!products.is_empty(), "search should return matches");
    // Exact title match must rank first.
    assert_eq!(products[0].id, "p-latte");
}

// --- Issue #10: merchant discovery profile ----------------------------------

#[tokio::test]
async fn agent_card_is_valid_json_with_capabilities() {
    let (status, body) = get("/.well-known/agent-card.json").await;
    assert_eq!(status, StatusCode::OK);
    let card: serde_json::Value = serde_json::from_str(&body).unwrap();
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
