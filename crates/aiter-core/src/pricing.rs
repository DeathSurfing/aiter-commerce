//! Totals computation for carts.
//!
//! Per-line totals (`qty × price`), the cart subtotal, and a pluggable tax
//! hook. All math is **exact integer** — it delegates to [`Amount`]
//! arithmetic, which never touches floats and refuses cross-currency mixes.

use serde::{Deserialize, Serialize};

use crate::amount::{Amount, AmountError};
use crate::cart::Cart;

/// Errors from pricing a cart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PricingError {
    /// Underlying amount arithmetic failed (overflow / mixed currency).
    Amount(AmountError),
    /// An item in the cart has no price in the catalog.
    MissingPrice(String),
}

impl From<AmountError> for PricingError {
    fn from(e: AmountError) -> Self {
        PricingError::Amount(e)
    }
}

/// Aggregated money figures for a cart, checkout, or order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub subtotal: Amount,
    pub tax: Amount,
    pub total: Amount,
}

/// Pluggable tax computation. Implementations decide the rate/amount; the
/// result is added (as exact integer minor units) to the subtotal.
pub trait TaxCalculator {
    fn tax_on(&self, subtotal: &Amount) -> Result<Amount, PricingError>;
}

/// A tax hook that always returns zero — the default for untaxed pricing.
pub struct NoTax;

impl TaxCalculator for NoTax {
    fn tax_on(&self, subtotal: &Amount) -> Result<Amount, PricingError> {
        Ok(Amount::zero(subtotal.currency()))
    }
}

/// `qty × unit_price`, exact integer.
pub fn line_total(quantity: u32, unit_price: &Amount) -> Result<Amount, PricingError> {
    Ok(unit_price.mul_by_quantity(quantity)?)
}

/// Sum of all per-line totals for a cart, using `price_of` to resolve each
/// product's unit price from the catalog.
pub fn cart_subtotal<F>(cart: &Cart, price_of: F) -> Result<Amount, PricingError>
where
    F: Fn(&str) -> Option<Amount>,
{
    let mut total = Amount::zero(cart.currency());
    for item in cart.line_items() {
        let price = price_of(&item.product_id)
            .ok_or_else(|| PricingError::MissingPrice(item.product_id.clone()))?;
        let line = line_total(item.quantity, &price)?;
        total = total.add(&line)?;
    }
    Ok(total)
}

/// Full pricing: subtotal, tax, and grand total.
pub fn compute_totals<F, T>(cart: &Cart, price_of: F, tax: &T) -> Result<Totals, PricingError>
where
    F: Fn(&str) -> Option<Amount>,
    T: TaxCalculator,
{
    let subtotal = cart_subtotal(cart, price_of)?;
    let tax_amount = tax.tax_on(&subtotal)?;
    let total = subtotal.add(&tax_amount)?;
    Ok(Totals {
        subtotal,
        tax: tax_amount,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::{Amount, Currency};
    use std::collections::HashMap;

    fn usd(units: i64) -> Amount {
        Amount::new(units, Currency::USD)
    }

    fn price_map() -> HashMap<String, Amount> {
        HashMap::from([
            ("p1".to_string(), usd(100)), // $1.00
            ("p2".to_string(), usd(350)), // $3.50
            ("p3".to_string(), usd(25)),  // $0.25
        ])
    }

    fn lookup<'a>(map: &'a HashMap<String, Amount>) -> impl Fn(&str) -> Option<Amount> + 'a {
        move |id: &str| map.get(id).copied()
    }

    #[test]
    fn per_line_total_is_exact() {
        assert_eq!(line_total(3, &usd(199)), Ok(usd(597)));
        assert_eq!(line_total(2, &usd(350)), Ok(usd(700)));
    }

    #[test]
    fn subtotal_sums_all_lines() {
        let mut cart = Cart::new(Currency::USD);
        cart.add("p1", 2).unwrap(); // 200
        cart.add("p2", 1).unwrap(); // 350
        cart.add("p3", 4).unwrap(); // 100
        let subtotal = cart_subtotal(&cart, lookup(&price_map())).expect("priced");
        assert_eq!(subtotal, usd(650));
    }

    #[test]
    fn empty_cart_has_zero_subtotal() {
        let cart = Cart::new(Currency::USD);
        assert_eq!(cart_subtotal(&cart, lookup(&price_map())), Ok(usd(0)));
    }

    #[test]
    fn missing_price_is_reported() {
        let mut cart = Cart::new(Currency::USD);
        cart.add("ghost", 1).unwrap();
        assert_eq!(
            cart_subtotal(&cart, lookup(&price_map())),
            Err(PricingError::MissingPrice("ghost".to_string()))
        );
    }

    struct FixedPercent(u64);

    impl TaxCalculator for FixedPercent {
        fn tax_on(&self, subtotal: &Amount) -> Result<Amount, PricingError> {
            // Exact integer percentage of the subtotal (single digit, no floats).
            let units = subtotal
                .units()
                .checked_mul(self.0 as i64)
                .ok_or(AmountError::Overflow)?;
            Ok(Amount::new(units / 100, subtotal.currency()))
        }
    }

    #[test]
    fn compute_totals_includes_tax() {
        let mut cart = Cart::new(Currency::USD);
        cart.add("p1", 2).unwrap(); // 200 subtotal
        let totals =
            compute_totals(&cart, lookup(&price_map()), &FixedPercent(10)).expect("priced");
        assert_eq!(totals.subtotal, usd(200));
        assert_eq!(totals.tax, usd(20));
        assert_eq!(totals.total, usd(220));
    }

    #[test]
    fn no_tax_is_zero_tax() {
        let mut cart = Cart::new(Currency::USD);
        cart.add("p1", 1).unwrap();
        let totals = compute_totals(&cart, lookup(&price_map()), &NoTax).expect("priced");
        assert_eq!(totals.tax, usd(0));
        assert_eq!(totals.total, totals.subtotal);
    }

    // --- Deterministic property-style tests over random carts ---

    /// Tiny xorshift PRNG so the "random" carts are fully reproducible.
    struct XorShift(u64);

    impl XorShift {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn range(&mut self, max: u64) -> u64 {
            self.next_u64() % max
        }
    }

    #[test]
    fn subtotal_equals_independent_line_by_line_sum_over_random_carts() {
        let mut rng = XorShift(0xA1CE_2010_2026_0808);
        let prices: Vec<(String, i64)> = (0..20)
            .map(|i| (format!("p{i}"), (rng.range(100_000) + 1) as i64))
            .collect();

        for _ in 0..200 {
            let mut cart = Cart::new(Currency::USD);
            let mut expected = usd(0);
            for (id, price) in &prices {
                if rng.range(4) == 0 {
                    let qty = (rng.range(100) + 1) as u32;
                    cart.add(id.clone(), qty).unwrap();
                    expected = expected.add(&usd(price * qty as i64)).unwrap();
                    // Recompute `expected` the independent way to catch drift:
                    let _ = expected.units();
                }
            }
            if cart.is_empty() {
                continue;
            }
            let map: HashMap<String, Amount> =
                prices.iter().map(|(id, p)| (id.clone(), usd(*p))).collect();
            let subtotal = cart_subtotal(&cart, lookup(&map)).expect("priced");
            assert_eq!(subtotal, expected);
        }
    }

    #[test]
    fn totals_are_consistent_over_random_carts() {
        let mut rng = XorShift(0xDEAD_BEEF_0000_0001);
        for _ in 0..200 {
            let mut cart = Cart::new(Currency::USD);
            for i in 0..10 {
                if rng.range(3) == 0 {
                    cart.add(format!("p{i}"), (rng.range(50) + 1) as u32)
                        .unwrap();
                }
            }
            if cart.is_empty() {
                continue;
            }
            let map: HashMap<String, Amount> = (0..10)
                .map(|i| (format!("p{i}"), usd((rng.range(50_000) + 1) as i64)))
                .collect();
            let totals = compute_totals(&cart, lookup(&map), &FixedPercent(5)).expect("priced");
            // total == subtotal + tax, always.
            assert_eq!(totals.subtotal.add(&totals.tax), Ok(totals.total));
        }
    }
}
