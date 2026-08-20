//! Demo merchant seed catalog.
//!
//! Loads a small coffee-shop catalog from a JSON fixture (no random data) and
//! exposes it as an [`aiter_core::catalog::Catalog`].

use axum::http::StatusCode;
use axum::Json;

use aiter_core::catalog::Catalog;

/// The embedded demo catalog, loaded from `fixtures/catalog.json` at compile
/// time. Fails if the fixture ever stops being valid JSON for a `Catalog`.
pub fn demo_catalog() -> Result<Catalog, serde_json::Error> {
    serde_json::from_str(include_str!("../fixtures/catalog.json"))
}

/// `GET /seed/catalog` — export the demo merchant's seeded catalog.
pub async fn seed_catalog() -> Result<Json<Catalog>, StatusCode> {
    demo_catalog()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_at_least_eight_products() {
        let catalog = demo_catalog().expect("fixture should load");
        assert!(catalog.products.len() >= 8, "expected >= 8 products");
    }

    #[test]
    fn all_prices_are_non_negative() {
        let catalog = demo_catalog().expect("fixture should load");
        for product in &catalog.products {
            assert!(
                !product.price.is_negative(),
                "product {} has a negative price",
                product.id
            );
        }
    }

    #[test]
    fn all_product_ids_are_unique() {
        let catalog = demo_catalog().expect("fixture should load");
        let mut ids: Vec<&str> = catalog.products.iter().map(|p| p.id.as_str()).collect();
        ids.sort_unstable();
        let unique: std::collections::HashSet<_> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "duplicate product ids in fixture");
    }

    #[test]
    fn fixture_round_trips_through_serde() {
        let catalog = demo_catalog().expect("fixture should load");
        let json = serde_json::to_string(&catalog).expect("serialize");
        let back: Catalog = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(catalog, back);
    }
}
