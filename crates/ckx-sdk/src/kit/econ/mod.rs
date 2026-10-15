//! The soft economy a game keeps in its hubs: items in stacks ([`items`]), soft currencies
//! ([`wallet`]), both together with a mailbox for what does not fit ([`holdings`]), escrow holds
//! and the references a hub answers once ([`holds`]), recipes ([`recipes`]), loot with pity
//! ([`loot`]), idle production that catches up ([`production`]), vendors that restock
//! ([`vendor`]), an order book ([`orderbook`]), and levels, achievements and quests
//! ([`progression`]).
//!
//! **It is not a ledger.** A hub's items and balances are its own state, persisted with its
//! snapshots like everything else it keeps. A transfer between hubs is calls each answered once
//! by its reference ([`holds::Applied`]), not a transaction: there is no double-entry book, no
//! audit trail beyond the hubs' logs, and no real money. Escrow is a hold in the owner's own hub
//! ([`holds::Holds`]), so goods in flight are never in two places, and whatever drives a
//! transfer (a market) retries each step until its hub answers.

pub mod holdings;
pub mod holds;
pub mod items;
pub mod loot;
pub mod orderbook;
pub mod production;
pub mod progression;
pub mod recipes;
pub mod vendor;
pub mod wallet;

pub use holdings::{Bundle, Holdings};
pub use holds::{Applied, Holds};
pub use items::{Catalog, Inventory, ItemDef, Stack};
pub use wallet::Wallet;

/// The longest item id, currency or reference the economy takes.
pub const ID_MAX: usize = 64;

pub(crate) fn check_id(what: &str, id: &str) -> crate::Result<()> {
    if id.is_empty() || id.len() > ID_MAX {
        return Err(super::error(super::codes::BAD_REQUEST, format!("a {what} is 1 to {ID_MAX} bytes")));
    }
    Ok(())
}
