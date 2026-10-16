//! Loot tables with pity: each roll picks by weight, and a pity group the player has not hit in
//! `pity_within` rolls is guaranteed on the next. A player's [`LootBook`] for a table keeps the
//! counters and the last rolls, for a dispute.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::kit::Rng;

fn one() -> u64 {
    1
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct LootEntry {
    pub item: String,
    #[serde(default = "one")]
    pub count: u64,
    pub weight: u64,
    /// Entries in one pity group tick its counter together.
    #[serde(default)]
    pub pity_group: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct LootTable {
    pub entries: Vec<LootEntry>,
    /// A pity group, and the rolls within which it is guaranteed.
    pub pity_within: BTreeMap<String, u32>,
}

/// What a player's rolls on one table have been.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct LootBook {
    pub rolls: u64,
    pub pity: BTreeMap<String, u32>,
    /// The last rolls: `(roll number, item)`.
    pub recent: Vec<(u64, String)>,
}

/// How many of a book's rolls it keeps.
pub const RECENT_KEEP: usize = 32;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct LootRoll {
    pub item: String,
    pub count: u64,
    /// Pity picked it.
    pub pity: bool,
    pub roll: u64,
}

/// Rolls once; `None` for a table with no weight. A due pity group picks its rarest entry.
pub fn roll(table: &LootTable, book: &mut LootBook, rng: &mut Rng) -> Option<LootRoll> {
    let due = table.pity_within.iter().find(|(g, within)| book.pity.get(*g).copied().unwrap_or(0) + 1 >= **within).map(|(g, _)| g.clone());
    let picked = match &due {
        Some(g) => table.entries.iter().filter(|e| e.pity_group.as_deref() == Some(g)).min_by_key(|e| e.weight).cloned(),
        None => {
            let weights: Vec<u64> = table.entries.iter().map(|e| e.weight).collect();
            rng.weighted(&weights).map(|i| table.entries[i].clone())
        }
    }?;
    book.rolls += 1;
    for g in table.pity_within.keys() {
        let c = book.pity.entry(g.clone()).or_insert(0);
        *c = if picked.pity_group.as_deref() == Some(g) { 0 } else { c.saturating_add(1) };
    }
    book.recent.push((book.rolls, picked.item.clone()));
    if book.recent.len() > RECENT_KEEP {
        let drop = book.recent.len() - RECENT_KEEP;
        book.recent.drain(..drop);
    }
    Some(LootRoll { item: picked.item, count: picked.count, pity: due.is_some(), roll: book.rolls })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gacha() -> LootTable {
        LootTable {
            entries: vec![
                LootEntry { item: "common".into(), count: 1, weight: 990, pity_group: None },
                LootEntry { item: "legendary".into(), count: 1, weight: 10, pity_group: Some("rare".into()) },
            ],
            pity_within: BTreeMap::from([("rare".to_string(), 10)]),
        }
    }

    #[test]
    fn pity_guarantees_the_group_within_its_rolls() {
        let (t, mut book, mut rng) = (gacha(), LootBook::default(), Rng::seeded(7));
        let mut since = 0;
        for _ in 0..200 {
            let r = roll(&t, &mut book, &mut rng).unwrap();
            since = if r.item == "legendary" { 0 } else { since + 1 };
            assert!(since < 10, "pity fires within ten rolls");
        }
        assert_eq!((book.rolls, book.recent.len()), (200, RECENT_KEEP));
    }

    #[test]
    fn a_natural_hit_resets_the_counter_and_an_empty_table_rolls_nothing() {
        let sure = LootTable {
            entries: vec![LootEntry { item: "legendary".into(), count: 2, weight: 1, pity_group: Some("rare".into()) }],
            pity_within: BTreeMap::from([("rare".to_string(), 10)]),
        };
        let mut book = LootBook { pity: BTreeMap::from([("rare".to_string(), 5)]), ..LootBook::default() };
        let r = roll(&sure, &mut book, &mut Rng::seeded(1)).unwrap();
        assert_eq!((r.count, r.pity, book.pity["rare"]), (2, false, 0));
        assert!(roll(&LootTable::default(), &mut book, &mut Rng::seeded(1)).is_none());
    }
}
