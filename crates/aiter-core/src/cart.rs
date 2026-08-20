//! Shopping cart model.
//!
//! A [`Cart`] is a pure in-memory set of [`LineItem`]s (product id + quantity)
//! scoped to a single [`Currency`]. It supports add / update / remove and
//! round-trips through serde. Totals are computed separately in [`crate::pricing`]
//! because line items carry no price; prices come from the catalog at pricing time.

use serde::{Deserialize, Serialize};

use crate::amount::Currency;

/// One product plus the quantity requested in a cart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineItem {
    pub product_id: String,
    pub quantity: u32,
}

/// A set of [`LineItem`]s in a single currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cart {
    pub currency: Currency,
    pub items: Vec<LineItem>,
}

/// Errors from cart mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CartError {
    /// Quantity must be non-zero on `add`.
    InvalidQuantity,
    /// Adding would overflow `u32` quantity for a line.
    QuantityOverflow,
}

impl Cart {
    pub fn new(currency: Currency) -> Self {
        Cart {
            currency,
            items: Vec::new(),
        }
    }

    /// Add `quantity` to a line, creating it if absent and merging if present.
    pub fn add(&mut self, product_id: impl Into<String>, quantity: u32) -> Result<(), CartError> {
        if quantity == 0 {
            return Err(CartError::InvalidQuantity);
        }
        let product_id = product_id.into();
        if let Some(item) = self.items.iter_mut().find(|li| li.product_id == product_id) {
            let next = (item.quantity as u64)
                .checked_add(quantity as u64)
                .ok_or(CartError::QuantityOverflow)?;
            item.quantity = u32::try_from(next).map_err(|_| CartError::QuantityOverflow)?;
        } else {
            self.items.push(LineItem {
                product_id,
                quantity,
            });
        }
        Ok(())
    }

    /// Set a quantity explicitly; zero removes the line.
    pub fn update(&mut self, product_id: &str, quantity: u32) {
        if quantity == 0 {
            self.items.retain(|li| li.product_id != product_id);
            return;
        }
        if let Some(item) = self.items.iter_mut().find(|li| li.product_id == product_id) {
            item.quantity = quantity;
        } else {
            self.items.push(LineItem {
                product_id: product_id.to_string(),
                quantity,
            });
        }
    }

    /// Remove a product from the cart entirely.
    pub fn remove(&mut self, product_id: &str) {
        self.items.retain(|li| li.product_id != product_id);
    }

    /// Current quantity of a product (0 if not present).
    pub fn quantity_of(&self, product_id: &str) -> u32 {
        self.items
            .iter()
            .find(|li| li.product_id == product_id)
            .map(|li| li.quantity)
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn line_items(&self) -> &[LineItem] {
        &self.items
    }

    pub fn currency(&self) -> Currency {
        self.currency
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::Currency;

    #[test]
    fn fresh_cart_is_empty_in_its_currency() {
        let cart = Cart::new(Currency::EUR);
        assert!(cart.is_empty());
        assert_eq!(cart.currency(), Currency::EUR);
    }

    #[test]
    fn add_creates_and_merges_lines() {
        let mut cart = Cart::new(Currency::USD);
        cart.add("p1", 2).unwrap();
        cart.add("p2", 1).unwrap();
        assert_eq!(cart.quantity_of("p1"), 2);
        cart.add("p1", 3).unwrap();
        assert_eq!(cart.quantity_of("p1"), 5);
        assert_eq!(cart.line_items().len(), 2);
    }

    #[test]
    fn add_rejects_zero_and_guards_overflow() {
        let mut cart = Cart::new(Currency::USD);
        assert_eq!(cart.add("p1", 0), Err(CartError::InvalidQuantity));
        cart.add("p1", u32::MAX).unwrap();
        assert_eq!(cart.add("p1", 1), Err(CartError::QuantityOverflow));
    }

    #[test]
    fn update_sets_or_removes_quantity() {
        let mut cart = Cart::new(Currency::USD);
        cart.add("p1", 1).unwrap();
        cart.update("p1", 7);
        assert_eq!(cart.quantity_of("p1"), 7);
        cart.update("p1", 0);
        assert_eq!(cart.quantity_of("p1"), 0);
        assert!(cart.is_empty());
        // update on an absent product inserts it
        cart.update("p2", 4);
        assert_eq!(cart.quantity_of("p2"), 4);
    }

    #[test]
    fn remove_deletes_only_the_named_line() {
        let mut cart = Cart::new(Currency::USD);
        cart.add("p1", 1).unwrap();
        cart.add("p2", 1).unwrap();
        cart.remove("p1");
        assert_eq!(cart.quantity_of("p1"), 0);
        assert_eq!(cart.quantity_of("p2"), 1);
        assert_eq!(cart.line_items().len(), 1);
        // removing an absent product is a no-op
        cart.remove("nope");
        assert_eq!(cart.line_items().len(), 1);
    }

    #[test]
    fn serde_round_trips_a_cart() {
        let mut cart = Cart::new(Currency::GBP);
        cart.add("p1", 2).unwrap();
        cart.add("p2", 5).unwrap();
        let json = serde_json::to_string(&cart).expect("serialize");
        let back: Cart = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cart, back);
    }
}
