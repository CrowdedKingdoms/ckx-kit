//! What a player owns: their inventory, their wallet, and the mail that holds items that did not
//! fit ([`Holdings`]); and items and currency moved together ([`Bundle`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::check_id;
use super::items::{Catalog, Inventory};
use super::wallet::Wallet;
use crate::Result;
use crate::kit::{codes, error};

/// Items and currency moved together: a price, a reward, a hold.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Bundle {
    /// Item id to how many.
    pub items: BTreeMap<String, u64>,
    /// Currency to how much.
    pub currency: BTreeMap<String, i64>,
}

impl Bundle {
    pub fn item(id: &str, count: u64) -> Self {
        Self { items: BTreeMap::from([(id.to_owned(), count)]), currency: BTreeMap::new() }
    }

    pub fn coins(currency: &str, amount: i64) -> Self {
        Self { items: BTreeMap::new(), currency: BTreeMap::from([(currency.to_owned(), amount)]) }
    }

    pub fn is_empty(&self) -> bool {
        self.items.values().all(|&n| n == 0) && self.currency.values().all(|&n| n == 0)
    }

    /// `bad_request` for an id the economy does not take or a negative amount.
    pub fn check(&self) -> Result<()> {
        for id in self.items.keys() {
            check_id("item id", id)?;
        }
        for (id, &amount) in &self.currency {
            check_id("currency", id)?;
            if amount < 0 {
                return Err(error(codes::BAD_REQUEST, "an amount is not negative"));
            }
        }
        Ok(())
    }

    pub fn plus(&self, other: &Bundle) -> Result<Bundle> {
        let mut out = self.clone();
        for (id, &n) in &other.items {
            let e = out.items.entry(id.clone()).or_insert(0);
            *e = e.checked_add(n).ok_or_else(|| error(codes::LIMIT, "too many"))?;
        }
        for (id, &n) in &other.currency {
            let e = out.currency.entry(id.clone()).or_insert(0);
            *e = e.checked_add(n).ok_or_else(|| error(codes::LIMIT, "too much"))?;
        }
        Ok(out.trimmed())
    }

    /// What is left of `self` once `part` is taken out of it; `insufficient` when it holds less.
    pub fn minus(&self, part: &Bundle) -> Result<Bundle> {
        let mut out = self.clone();
        for (id, &n) in &part.items {
            let e = out.items.entry(id.clone()).or_insert(0);
            *e = e.checked_sub(n).ok_or_else(|| error(codes::INSUFFICIENT, format!("not {n} {id}")))?;
        }
        for (id, &n) in &part.currency {
            let e = out.currency.entry(id.clone()).or_insert(0);
            if *e < n {
                return Err(error(codes::INSUFFICIENT, format!("not {n} {id}")));
            }
            *e -= n;
        }
        Ok(out.trimmed())
    }

    /// `n` of it.
    pub fn times(&self, n: u64) -> Result<Bundle> {
        let k = i64::try_from(n).map_err(|_| error(codes::LIMIT, "too many"))?;
        let mut out = Bundle::default();
        for (id, &c) in &self.items {
            out.items.insert(id.clone(), c.checked_mul(n).ok_or_else(|| error(codes::LIMIT, "too many"))?);
        }
        for (id, &c) in &self.currency {
            out.currency.insert(id.clone(), c.checked_mul(k).ok_or_else(|| error(codes::LIMIT, "too much"))?);
        }
        Ok(out.trimmed())
    }

    /// Without its zero entries.
    pub fn trimmed(mut self) -> Bundle {
        self.items.retain(|_, n| *n > 0);
        self.currency.retain(|_, n| *n != 0);
        self
    }
}

/// A player's inventory and wallet, and their mail: items given to them that did not fit,
/// kept until they make room ([`Holdings::take_mail`]).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Holdings {
    pub inventory: Inventory,
    pub wallet: Wallet,
    pub mail: BTreeMap<String, u64>,
}

impl Holdings {
    pub fn new(slots: u32) -> Self {
        Self { inventory: Inventory::new(slots), ..Self::default() }
    }

    /// Whether `b` could be taken.
    pub fn has(&self, b: &Bundle) -> bool {
        b.items.iter().all(|(id, &n)| self.inventory.count(id) >= n) && b.currency.iter().all(|(id, &n)| self.wallet.balance(id) >= n)
    }

    /// Takes `b`, or nothing: `insufficient`.
    pub fn take(&mut self, b: &Bundle) -> Result<()> {
        b.check()?;
        if !self.has(b) {
            return Err(error(codes::INSUFFICIENT, "not enough"));
        }
        for (id, &n) in &b.items {
            self.inventory.remove(id, n)?;
        }
        for (id, &n) in &b.currency {
            self.wallet.debit(id, n)?;
        }
        Ok(())
    }

    /// Gives `b`, or nothing: `full` when its items do not fit, `not_found` for an item the
    /// catalog lacks.
    pub fn give(&mut self, catalog: &Catalog, b: &Bundle) -> Result<()> {
        b.check()?;
        let mut next = self.clone();
        for (id, &n) in &b.items {
            next.inventory.add(catalog, id, n)?;
        }
        for (id, &n) in &b.currency {
            next.wallet.credit(id, n)?;
        }
        *self = next;
        Ok(())
    }

    /// Gives `b`, its items as far as they fit and the rest to the mail. It fails only for a
    /// bundle the economy does not take: what another hub delivers always arrives.
    pub fn give_or_mail(&mut self, catalog: &Catalog, b: &Bundle) -> Result<()> {
        b.check()?;
        for id in b.items.keys() {
            catalog.item(id)?;
        }
        let mut next = self.clone();
        for (id, &n) in &b.currency {
            next.wallet.credit(id, n)?;
        }
        for (id, &n) in &b.items {
            let left = next.inventory.add_what_fits(catalog, id, n);
            if left > 0 {
                *next.mail.entry(id.clone()).or_insert(0) += left;
            }
        }
        *self = next;
        Ok(())
    }

    /// Takes `pay` and gives `get` together, or neither.
    pub fn swap(&mut self, catalog: &Catalog, pay: &Bundle, get: &Bundle) -> Result<()> {
        let mut next = self.clone();
        next.take(pay)?;
        next.give(catalog, get)?;
        *self = next;
        Ok(())
    }

    /// Moves what fits from the mail into the inventory; answers what moved.
    pub fn take_mail(&mut self, catalog: &Catalog) -> Bundle {
        let mut moved = Bundle::default();
        let mail = std::mem::take(&mut self.mail);
        for (id, n) in mail {
            let left = self.inventory.add_what_fits(catalog, &id, n);
            if n > left {
                moved.items.insert(id.clone(), n - left);
            }
            if left > 0 {
                self.mail.insert(id, left);
            }
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::econ::items::ItemDef;

    fn catalog() -> Catalog {
        Catalog::from(vec![ItemDef { id: "ore".into(), max_stack: 10, tradable: true, name: String::new() }])
    }

    #[test]
    fn a_swap_is_all_or_nothing() {
        let c = catalog();
        let mut h = Holdings::new(1);
        h.give(&c, &Bundle::coins("gold", 50)).unwrap();
        let before = h.clone();
        assert!(h.swap(&c, &Bundle::coins("gold", 20), &Bundle::item("ore", 11)).is_err(), "11 ore do not fit one slot");
        assert_eq!(h, before);
        h.swap(&c, &Bundle::coins("gold", 20), &Bundle::item("ore", 10)).unwrap();
        assert_eq!((h.wallet.balance("gold"), h.inventory.count("ore")), (30, 10));
    }

    #[test]
    fn what_does_not_fit_waits_in_the_mail() {
        let c = catalog();
        let mut h = Holdings::new(1);
        h.give_or_mail(&c, &Bundle::item("ore", 14).plus(&Bundle::coins("gold", 3)).unwrap()).unwrap();
        assert_eq!((h.inventory.count("ore"), h.mail["ore"], h.wallet.balance("gold")), (10, 4, 3));
        h.take(&Bundle::item("ore", 6)).unwrap();
        assert_eq!(h.take_mail(&c), Bundle::item("ore", 4));
        assert!(h.mail.is_empty());
    }

    #[test]
    fn bundles_add_take_and_multiply_without_overflow() {
        let price = Bundle::coins("gold", 7);
        assert_eq!(price.times(3).unwrap(), Bundle::coins("gold", 21));
        assert!(price.times(u64::MAX).is_err());
        let both = price.plus(&Bundle::item("ore", 2)).unwrap();
        assert_eq!(both.minus(&Bundle::item("ore", 2)).unwrap(), price);
        assert!(both.minus(&Bundle::coins("gold", 8)).is_err());
        assert!(Bundle::coins("gold", -1).check().is_err());
    }
}
