//! Catalog / product model.
//!
//! A [`Product`] is the merchant-facing item an agent can buy: identity, a
//! title, an [`Amount`] price, description, tags, an optional image URL, and
//! the quantity currently available. Variant-aware structure is carried in an
//! optional [`ProductVariant`] so a product may advertise e.g. size/colour
//! options without forcing every listing to have them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::amount::Amount;

/// A single sellable catalog item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Product {
    /// Stable identifier; must be non-empty (see [`Product::validate`]).
    pub id: String,
    pub title: String,
    pub price: Amount,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    /// Units currently available to sell.
    pub available_qty: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<ProductVariant>,
}

/// Optional variant structure for a product (size/colour/... options).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductVariant {
    /// Name of the variant dimension, e.g. `"size"`.
    pub name: String,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_adjustment: Option<Amount>,
}

/// Validation failures for a [`Product`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductError {
    /// The product id is missing/empty.
    MissingId,
    /// The price is negative.
    NegativePrice,
}

impl Product {
    /// Validate invariants: a required id and a non-negative price.
    pub fn validate(&self) -> Result<(), ProductError> {
        if self.id.is_empty() {
            return Err(ProductError::MissingId);
        }
        if self.price.is_negative() {
            return Err(ProductError::NegativePrice);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::{Amount, Currency};

    fn valid_product(id: &str) -> Product {
        Product {
            id: id.to_string(),
            title: "Widget".to_string(),
            price: Amount::new(100, Currency::USD),
            description: String::new(),
            tags: vec![],
            image_url: None,
            available_qty: 5,
            variant: None,
        }
    }

    #[test]
    fn validation_rejects_missing_id() {
        let p = valid_product("");
        assert_eq!(p.validate(), Err(ProductError::MissingId));
    }

    #[test]
    fn validation_rejects_negative_price() {
        let mut p = valid_product("p1");
        p.price = Amount::new(-1, Currency::USD);
        assert_eq!(p.validate(), Err(ProductError::NegativePrice));
    }

    #[test]
    fn validation_accepts_valid_product() {
        assert_eq!(valid_product("p1").validate(), Ok(()));
    }

    #[test]
    fn serde_round_trips_a_product() {
        let mut p = valid_product("p-42");
        p.description = "A nice widget".to_string();
        p.tags = vec!["tool".to_string(), "gadget".to_string()];
        p.image_url = Some("https://example.com/w.png".to_string());
        p.variant = Some(ProductVariant {
            name: "size".to_string(),
            options: BTreeMap::from([("S".to_string(), "0".to_string())]),
            price_adjustment: None,
        });

        let json = serde_json::to_string(&p).expect("serialize");
        let back: Product = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(p, back);
    }

    #[test]
    fn serde_tolerates_missing_optional_fields() {
        let json =
            r#"{"id":"p1","title":"T","price":{"units":100,"currency":"USD"},"available_qty":3}"#;
        let p: Product = serde_json::from_str(json).expect("deserialize");
        assert!(p.tags.is_empty());
        assert!(p.image_url.is_none());
        assert!(p.variant.is_none());
        assert_eq!(p.validate(), Ok(()));
    }
}
