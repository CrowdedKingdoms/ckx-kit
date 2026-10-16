//! Items: what a game's items are ([`Catalog`]), and an inventory of stacks with a number of
//! slots ([`Inventory`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::kit::{codes, error};

fn one() -> u64 {
    1
}

fn yes() -> bool {
    true
}

/// One kind of item.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ItemDef {
    pub id: String,
    /// What one slot holds; 1 for an item that does not stack.
    #[serde(default = "one")]
    pub max_stack: u64,
    /// Whether it may change hands (a market's order book, a trade).
    #[serde(default = "yes")]
    pub tradable: bool,
    #[serde(default)]
    pub name: String,
}

/// The game's items, by id.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(from = "Vec<ItemDef>", into = "Vec<ItemDef>")]
pub struct Catalog {
    items: BTreeMap<String, ItemDef>,
}

impl From<Vec<ItemDef>> for Catalog {
    fn from(items: Vec<ItemDef>) -> Self {
        Self { items: items.into_iter().map(|i| (i.id.clone(), i)).collect() }
    }
}

impl From<Catalog> for Vec<ItemDef> {
    fn from(c: Catalog) -> Self {
        c.items.into_values().collect()
    }
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&ItemDef> {
        self.items.get(id)
    }

    /// The item, or `not_found`.
    pub fn item(&self, id: &str) -> Result<&ItemDef> {
        self.items.get(id).ok_or_else(|| error(codes::NOT_FOUND, format!("there is no item `{id}`")))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ItemDef> {
        self.items.values()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Stack {
    pub item: String,
    pub count: u64,
}

/// Stacks of items in a number of slots. Adding fills the item's stacks before starting new
/// ones; a stack holds at most its item's `max_stack`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    pub slots: u32,
    pub stacks: Vec<Stack>,
}

impl Inventory {
    pub fn new(slots: u32) -> Self {
        Self { slots, stacks: Vec::new() }
    }

    pub fn count(&self, item: &str) -> u64 {
        self.stacks.iter().filter(|s| s.item == item).map(|s| s.count).sum()
    }

    /// How many of `item` still fit.
    pub fn room(&self, catalog: &Catalog, item: &str) -> u64 {
        let Some(def) = catalog.get(item) else { return 0 };
        let max = def.max_stack.max(1);
        let in_stacks: u64 = self.stacks.iter().filter(|s| s.item == item).map(|s| max.saturating_sub(s.count)).sum();
        let free = u64::from(self.slots).saturating_sub(self.stacks.len() as u64);
        in_stacks.saturating_add(free.saturating_mul(max))
    }

    /// Adds `count` of `item`, or nothing: `not_found` for an item the catalog lacks, `full`
    /// when they do not fit.
    pub fn add(&mut self, catalog: &Catalog, item: &str, count: u64) -> Result<()> {
        let max = catalog.item(item)?.max_stack.max(1);
        if count > self.room(catalog, item) {
            return Err(error(codes::FULL, format!("{count} {item} do not fit")));
        }
        let mut left = count;
        for s in self.stacks.iter_mut().filter(|s| s.item == item) {
            let put = left.min(max.saturating_sub(s.count));
            s.count += put;
            left -= put;
        }
        while left > 0 {
            let put = left.min(max);
            self.stacks.push(Stack { item: item.to_owned(), count: put });
            left -= put;
        }
        Ok(())
    }

    /// Adds what fits; answers how many did not.
    pub fn add_what_fits(&mut self, catalog: &Catalog, item: &str, count: u64) -> u64 {
        let fits = count.min(self.room(catalog, item));
        if fits > 0 && self.add(catalog, item, fits).is_err() {
            return count;
        }
        count - fits
    }

    /// Removes `count` of `item`, from its last stacks first, or nothing: `insufficient`.
    pub fn remove(&mut self, item: &str, count: u64) -> Result<()> {
        if self.count(item) < count {
            return Err(error(codes::INSUFFICIENT, format!("not {count} {item}")));
        }
        let mut left = count;
        for s in self.stacks.iter_mut().rev().filter(|s| s.item == item) {
            let take = left.min(s.count);
            s.count -= take;
            left -= take;
            if left == 0 {
                break;
            }
        }
        self.stacks.retain(|s| s.count > 0);
        Ok(())
    }

    /// Every item's total, by id.
    pub fn totals(&self) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for s in &self.stacks {
            *out.entry(s.item.clone()).or_insert(0) += s.count;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::from(vec![
            ItemDef { id: "wood".into(), max_stack: 10, tradable: true, name: String::new() },
            ItemDef { id: "sword".into(), max_stack: 1, tradable: true, name: String::new() },
        ])
    }

    #[test]
    fn stacks_fill_before_new_slots_and_a_full_inventory_takes_nothing() {
        let c = catalog();
        let mut inv = Inventory::new(2);
        inv.add(&c, "wood", 7).unwrap();
        inv.add(&c, "wood", 5).unwrap();
        assert_eq!(inv.stacks.iter().map(|s| s.count).collect::<Vec<_>>(), vec![10, 2]);
        assert_eq!(inv.room(&c, "wood"), 8);
        assert_eq!(inv.room(&c, "sword"), 0, "both slots are taken");
        let before = inv.clone();
        assert!(inv.add(&c, "wood", 9).is_err());
        assert_eq!(inv, before, "all or nothing");
        assert_eq!(inv.add_what_fits(&c, "wood", 9), 1);
        assert!(inv.add(&c, "stone", 1).is_err(), "not in the catalog");
    }

    #[test]
    fn removing_takes_from_the_last_stacks_and_frees_their_slots() {
        let c = catalog();
        let mut inv = Inventory::new(3);
        inv.add(&c, "wood", 14).unwrap();
        inv.remove("wood", 6).unwrap();
        assert_eq!(inv.stacks, vec![Stack { item: "wood".into(), count: 8 }]);
        assert!(inv.remove("wood", 9).is_err());
        assert_eq!(inv.count("wood"), 8);
    }

    #[test]
    fn a_catalog_reads_and_writes_as_a_list() {
        let json = r#"[{"id":"wood","max_stack":10},{"id":"sword"}]"#;
        let c: Catalog = serde_json::from_str(json).unwrap();
        assert_eq!(c.item("sword").unwrap().max_stack, 1);
        assert!(c.item("wood").unwrap().tradable);
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back.as_array().unwrap().len(), 2);
    }
}
