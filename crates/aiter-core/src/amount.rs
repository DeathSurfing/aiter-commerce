//! Money as integer minor units plus an ISO 4217 currency.
//!
//! There are **no floats** anywhere in money math. An [`Amount`] holds a whole
//! number of minor units (cents, paise, yen, ...) and a [`Currency`] that knows
//! how many minor units make up one major unit (its "minor-unit exponent").
//! Arithmetic is exact integer math with overflow guards and explicit refusal
//! to combine different currencies.

use serde::{Deserialize, Serialize};

/// An ISO 4217 currency with its minor-unit exponent (decimal places).
///
/// `minor_unit_exponent()` is the number of decimal places to the right of the
/// decimal point for a major unit (2 for USD, 0 for JPY).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Currency {
    USD,
    EUR,
    GBP,
    JPY,
    INR,
    CAD,
    AUD,
    CHF,
    CNY,
    KRW,
}

impl Currency {
    /// The ISO 4217 alphabetic code, e.g. `"USD"`.
    pub fn code(&self) -> &'static str {
        match self {
            Currency::USD => "USD",
            Currency::EUR => "EUR",
            Currency::GBP => "GBP",
            Currency::JPY => "JPY",
            Currency::INR => "INR",
            Currency::CAD => "CAD",
            Currency::AUD => "AUD",
            Currency::CHF => "CHF",
            Currency::CNY => "CNY",
            Currency::KRW => "KRW",
        }
    }

    /// Number of minor units per major unit (decimal places).
    pub fn minor_unit_exponent(&self) -> u32 {
        match self {
            Currency::JPY | Currency::KRW => 0,
            _ => 2,
        }
    }
}

/// A quantity of money expressed in integer minor units of a single currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Amount {
    pub units: i64,
    pub currency: Currency,
}

impl Amount {
    pub fn new(units: i64, currency: Currency) -> Self {
        Amount { units, currency }
    }

    /// The zero amount in the given currency.
    pub fn zero(currency: Currency) -> Self {
        Amount { units: 0, currency }
    }

    pub fn units(&self) -> i64 {
        self.units
    }

    pub fn currency(&self) -> Currency {
        self.currency
    }

    pub fn is_negative(&self) -> bool {
        self.units < 0
    }

    pub fn is_zero(&self) -> bool {
        self.units == 0
    }

    /// Add two amounts. Refuses cross-currency arithmetic and checks overflow.
    pub fn add(&self, other: &Amount) -> Result<Amount, AmountError> {
        self.require_same_currency(other)?;
        let units = self
            .units
            .checked_add(other.units)
            .ok_or(AmountError::Overflow)?;
        Ok(Amount {
            units,
            currency: self.currency,
        })
    }

    /// Subtract two amounts. Refuses cross-currency arithmetic and checks overflow.
    pub fn sub(&self, other: &Amount) -> Result<Amount, AmountError> {
        self.require_same_currency(other)?;
        let units = self
            .units
            .checked_sub(other.units)
            .ok_or(AmountError::Overflow)?;
        Ok(Amount {
            units,
            currency: self.currency,
        })
    }

    /// Multiply by a non-negative quantity using exact integer math (no floats).
    pub fn mul_by_quantity(&self, quantity: u32) -> Result<Amount, AmountError> {
        let units = i64::try_from(
            (self.units as i128)
                .checked_mul(quantity as i128)
                .ok_or(AmountError::Overflow)?,
        )
        .map_err(|_| AmountError::Overflow)?;
        Ok(Amount {
            units,
            currency: self.currency,
        })
    }

    fn require_same_currency(&self, other: &Amount) -> Result<(), AmountError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(AmountError::CrossCurrencyMismatch)
        }
    }
}

/// Errors raised by [`Amount`] arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountError {
    /// The computation would overflow `i64`.
    Overflow,
    /// Two amounts with different currencies were combined.
    CrossCurrencyMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_exponents_match_iso4217() {
        assert_eq!(Currency::USD.code(), "USD");
        assert_eq!(Currency::INR.code(), "INR");
        assert_eq!(Currency::USD.minor_unit_exponent(), 2);
        assert_eq!(Currency::JPY.minor_unit_exponent(), 0);
        assert_eq!(Currency::KRW.minor_unit_exponent(), 0);
    }

    #[test]
    fn addition_sums_units_in_same_currency() {
        let a = Amount::new(120, Currency::USD);
        let b = Amount::new(230, Currency::USD);
        assert_eq!(a.add(&b), Ok(Amount::new(350, Currency::USD)));
    }

    #[test]
    fn addition_refuses_cross_currency() {
        let a = Amount::new(1, Currency::USD);
        let b = Amount::new(1, Currency::EUR);
        assert_eq!(a.add(&b), Err(AmountError::CrossCurrencyMismatch));
        assert_eq!(b.sub(&a), Err(AmountError::CrossCurrencyMismatch));
    }

    #[test]
    fn addition_overflows_are_guarded() {
        let max = Amount::new(i64::MAX, Currency::USD);
        assert_eq!(
            max.add(&Amount::new(1, Currency::USD)),
            Err(AmountError::Overflow)
        );
    }

    #[test]
    fn subtraction_can_go_negative_but_not_overflow() {
        let a = Amount::new(120, Currency::USD);
        let b = Amount::new(300, Currency::USD);
        assert_eq!(a.sub(&b), Ok(Amount::new(-180, Currency::USD)));
        let min = Amount::new(i64::MIN, Currency::USD);
        assert_eq!(
            min.sub(&Amount::new(1, Currency::USD)),
            Err(AmountError::Overflow)
        );
    }

    #[test]
    fn multiply_by_quantity_is_exact() {
        let price = Amount::new(199, Currency::USD); // $1.99
        assert_eq!(
            price.mul_by_quantity(3),
            Ok(Amount::new(597, Currency::USD)) // $5.97
        );
        assert_eq!(price.mul_by_quantity(0), Ok(Amount::new(0, Currency::USD)));
    }

    #[test]
    fn multiply_by_quantity_guards_overflow() {
        let price = Amount::new(i64::MAX, Currency::USD);
        assert_eq!(price.mul_by_quantity(2), Err(AmountError::Overflow));
    }

    #[test]
    fn predicates_and_zero() {
        assert!(Amount::new(-1, Currency::USD).is_negative());
        assert!(!Amount::new(0, Currency::USD).is_negative());
        assert!(Amount::zero(Currency::EUR).is_zero());
    }
}
