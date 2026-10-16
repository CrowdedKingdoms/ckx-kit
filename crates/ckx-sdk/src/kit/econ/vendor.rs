//! Vendors: offers at fixed prices, each with stock that restocks and a limit per player.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::holdings::Bundle;
use crate::Result;
use crate::kit::{codes, error};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub item: String,
    /// What one costs.
    pub price: Bundle,
    /// How many a restock puts on the shelf; none for no limit.
    #[serde(default)]
    pub stock: Option<u64>,
    /// How many one player may buy between restocks; none for no limit.
    #[serde(default)]
    pub per_player: Option<u64>,
    /// What the vendor pays for one; none when it does not buy them.
    #[serde(default)]
    pub buys_for: Option<Bundle>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Vendor {
    pub id: String,
    pub offers: Vec<Offer>,
    /// When it restocks (cron), or never.
    #[serde(default)]
    pub restock: Option<String>,
}

/// A vendor's shelf and who bought what since the last restock.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Shelf {
    pub stock: BTreeMap<String, u64>,
    /// `"<player>:<item>"` to how many.
    pub bought: BTreeMap<String, u64>,
    pub restocked_at: u64,
}

impl Vendor {
    pub fn offer(&self, item: &str) -> Result<&Offer> {
        self.offers.iter().find(|o| o.item == item).ok_or_else(|| error(codes::NOT_FOUND, format!("`{}` sells no `{item}`", self.id)))
    }
}

impl Shelf {
    pub fn restock(&mut self, vendor: &Vendor, now: u64) {
        self.stock = vendor.offers.iter().filter_map(|o| o.stock.map(|s| (o.item.clone(), s))).collect();
        self.bought.clear();
        self.restocked_at = now;
    }

    /// Whether `player` may buy `count` of `offer` now: `insufficient` (stock), `limit`.
    pub fn check(&self, offer: &Offer, player: u64, count: u64) -> Result<()> {
        if count == 0 {
            return Err(error(codes::BAD_REQUEST, "buy at least one"));
        }
        if offer.stock.is_some() && self.stock.get(&offer.item).copied().unwrap_or(0) < count {
            return Err(error(codes::INSUFFICIENT, format!("not {count} {} in stock", offer.item)));
        }
        if let Some(cap) = offer.per_player
            && self.bought.get(&format!("{player}:{}", offer.item)).copied().unwrap_or(0) + count > cap
        {
            return Err(error(codes::LIMIT, format!("{cap} {} a player until the restock", offer.item)));
        }
        Ok(())
    }

    /// Records a sale `check` allowed.
    pub fn sold(&mut self, offer: &Offer, player: u64, count: u64) {
        if offer.stock.is_some()
            && let Some(s) = self.stock.get_mut(&offer.item)
        {
            *s = s.saturating_sub(count);
        }
        *self.bought.entry(format!("{player}:{}", offer.item)).or_insert(0) += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_and_a_per_player_limit_hold_until_the_restock() {
        let v = Vendor {
            id: "smith".into(),
            offers: vec![Offer {
                item: "sword".into(),
                price: Bundle::coins("gold", 50),
                stock: Some(2),
                per_player: Some(1),
                buys_for: None,
            }],
            restock: None,
        };
        let mut shelf = Shelf::default();
        shelf.restock(&v, 1);
        let o = v.offer("sword").unwrap();
        shelf.check(o, 1, 1).unwrap();
        shelf.sold(o, 1, 1);
        assert!(shelf.check(o, 1, 1).is_err(), "one a player");
        shelf.check(o, 2, 1).unwrap();
        shelf.sold(o, 2, 1);
        assert!(shelf.check(o, 3, 1).is_err(), "sold out");
        shelf.restock(&v, 2);
        shelf.check(o, 1, 1).unwrap();
        assert!(v.offer("shield").is_err());
    }
}
