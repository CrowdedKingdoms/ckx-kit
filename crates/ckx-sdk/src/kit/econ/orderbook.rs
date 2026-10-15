//! An order book for one item: limit orders matched at price-time priority, at the resting
//! order's price. Placing an order answers the [`Fill`]s whoever runs the book settles (the
//! buyer pays `price * count`, the seller gives `count`); escrow is the runner's ([`super::holds`]).

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::kit::{codes, error};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub id: u64,
    pub player: u64,
    pub side: Side,
    /// The limit, per unit.
    pub price: i64,
    /// What is still open.
    pub count: u64,
    pub at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    pub buyer: u64,
    pub seller: u64,
    pub buy_order: u64,
    pub sell_order: u64,
    pub price: i64,
    pub count: u64,
}

/// Bids best (highest) first, asks best (lowest) first, earlier first at a price.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct OrderBook {
    pub bids: Vec<Order>,
    pub asks: Vec<Order>,
}

impl OrderBook {
    /// Places order `id` and matches it at once; whatever is left rests on the book.
    pub fn place(&mut self, id: u64, player: u64, side: Side, price: i64, count: u64, now: u64) -> Result<Vec<Fill>> {
        if price <= 0 || count == 0 {
            return Err(error(codes::BAD_REQUEST, "a price and a count are positive"));
        }
        let mut left = count;
        let mut fills = Vec::new();
        let (far, near) = match side {
            Side::Buy => (&mut self.asks, &mut self.bids),
            Side::Sell => (&mut self.bids, &mut self.asks),
        };
        while left > 0 {
            let Some(best) = far.first_mut() else { break };
            let crosses = match side {
                Side::Buy => best.price <= price,
                Side::Sell => best.price >= price,
            };
            if !crosses {
                break;
            }
            let n = left.min(best.count);
            let (buyer, seller, buy_order, sell_order) = match side {
                Side::Buy => (player, best.player, id, best.id),
                Side::Sell => (best.player, player, best.id, id),
            };
            fills.push(Fill { buyer, seller, buy_order, sell_order, price: best.price, count: n });
            best.count -= n;
            left -= n;
            if best.count == 0 {
                far.remove(0);
            }
        }
        if left > 0 {
            let at = near
                .iter()
                .position(|o| match side {
                    Side::Buy => o.price < price,
                    Side::Sell => o.price > price,
                })
                .unwrap_or(near.len());
            near.insert(at, Order { id, player, side, price, count: left, at: now });
        }
        Ok(fills)
    }

    /// Takes `player`'s order `id` off the book.
    pub fn cancel(&mut self, id: u64, player: u64) -> Option<Order> {
        for side in [&mut self.bids, &mut self.asks] {
            if let Some(i) = side.iter().position(|o| o.id == id && o.player == player) {
                return Some(side.remove(i));
            }
        }
        None
    }

    pub fn order(&self, id: u64) -> Option<&Order> {
        self.bids.iter().chain(self.asks.iter()).find(|o| o.id == id)
    }

    /// `(price, count)` a level, best first.
    pub fn depth(&self, side: Side, levels: usize) -> Vec<(i64, u64)> {
        let orders = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        let mut out: Vec<(i64, u64)> = Vec::new();
        for o in orders {
            if let Some((p, n)) = out.last_mut()
                && *p == o.price
            {
                *n += o.count;
                continue;
            }
            if out.len() == levels {
                break;
            }
            out.push((o.price, o.count));
        }
        out
    }

    pub fn of(&self, player: u64) -> Vec<Order> {
        self.bids.iter().chain(self.asks.iter()).filter(|o| o.player == player).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_match_at_the_resting_price_in_price_time_order() {
        let mut b = OrderBook::default();
        b.place(1, 10, Side::Sell, 12, 5, 0).unwrap();
        b.place(2, 11, Side::Sell, 10, 3, 1).unwrap();
        b.place(3, 12, Side::Sell, 10, 4, 2).unwrap();
        let fills = b.place(4, 20, Side::Buy, 11, 5, 3).unwrap();
        assert_eq!(fills.iter().map(|f| (f.seller, f.price, f.count)).collect::<Vec<_>>(), vec![(11, 10, 3), (12, 10, 2)]);
        assert_eq!(b.depth(Side::Sell, 5), vec![(10, 2), (12, 5)]);
        assert!(b.bids.is_empty(), "the buy filled whole");
        let rest = b.place(5, 21, Side::Buy, 9, 4, 4).unwrap();
        assert!(rest.is_empty());
        assert_eq!(b.depth(Side::Buy, 5), vec![(9, 4)]);
        assert_eq!(b.cancel(5, 99), None, "only its owner cancels it");
        assert_eq!(b.cancel(5, 21).unwrap().count, 4);
        assert!(b.place(6, 1, Side::Buy, 0, 1, 5).is_err());
    }
}
