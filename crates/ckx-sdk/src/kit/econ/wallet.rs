//! Soft currencies: balances that never go below zero.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::kit::{codes, error};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Wallet {
    pub balances: BTreeMap<String, i64>,
}

impl Wallet {
    pub fn balance(&self, currency: &str) -> i64 {
        self.balances.get(currency).copied().unwrap_or(0)
    }

    /// Adds `amount`; answers the balance. `bad_request` for a negative amount, `limit` past
    /// the largest balance.
    pub fn credit(&mut self, currency: &str, amount: i64) -> Result<i64> {
        if amount < 0 {
            return Err(error(codes::BAD_REQUEST, "an amount is not negative"));
        }
        let now = self.balance(currency).checked_add(amount).ok_or_else(|| error(codes::LIMIT, format!("{currency} would overflow")))?;
        if now > 0 {
            self.balances.insert(currency.to_owned(), now);
        }
        Ok(now)
    }

    /// Takes `amount`; answers the balance. `insufficient` when there is not that much.
    pub fn debit(&mut self, currency: &str, amount: i64) -> Result<i64> {
        if amount < 0 {
            return Err(error(codes::BAD_REQUEST, "an amount is not negative"));
        }
        let have = self.balance(currency);
        if have < amount {
            return Err(error(codes::INSUFFICIENT, format!("not {amount} {currency}")));
        }
        let now = have - amount;
        if now == 0 {
            self.balances.remove(currency);
        } else {
            self.balances.insert(currency.to_owned(), now);
        }
        Ok(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_balance_never_goes_below_zero() {
        let mut w = Wallet::default();
        assert_eq!(w.credit("gold", 30).unwrap(), 30);
        assert!(w.debit("gold", 31).is_err());
        assert_eq!(w.debit("gold", 30).unwrap(), 0);
        assert!(w.balances.is_empty(), "an empty balance is not kept");
        assert!(w.credit("gold", -1).is_err());
        w.credit("gold", i64::MAX).unwrap();
        assert!(w.credit("gold", 1).is_err(), "no overflow");
    }
}
