//! Crafting: a recipe takes its inputs (items, and currency for a fee) and gives its outputs,
//! all together or not at all.

use serde::{Deserialize, Serialize};

use super::holdings::{Bundle, Holdings};
use super::items::Catalog;
use crate::Result;
use crate::kit::{codes, error};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    pub id: String,
    pub inputs: Bundle,
    pub outputs: Bundle,
    /// The level a player needs; 0 for none.
    #[serde(default)]
    pub level: u32,
}

/// The most crafts one call makes.
pub const CRAFT_MAX: u64 = 100;

/// Crafts `recipe` `times` times into `holdings`; answers what it made. `insufficient` without
/// the inputs, `full` without room for the outputs (the inputs' room counts, since they go
/// first), `forbidden` below the recipe's level.
pub fn craft(holdings: &mut Holdings, catalog: &Catalog, recipe: &Recipe, times: u64, level: u32) -> Result<Bundle> {
    if times == 0 || times > CRAFT_MAX {
        return Err(error(codes::BAD_REQUEST, format!("1 to {CRAFT_MAX} at a time")));
    }
    if level < recipe.level {
        return Err(error(codes::FORBIDDEN, format!("`{}` needs level {}", recipe.id, recipe.level)));
    }
    let made = recipe.outputs.times(times)?;
    holdings.swap(catalog, &recipe.inputs.times(times)?, &made)?;
    Ok(made)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::econ::items::ItemDef;

    #[test]
    fn a_craft_takes_its_inputs_and_gives_its_outputs_or_nothing() {
        let c = Catalog::from(vec![
            ItemDef { id: "wood".into(), max_stack: 20, tradable: true, name: String::new() },
            ItemDef { id: "plank".into(), max_stack: 20, tradable: true, name: String::new() },
        ]);
        let plank = Recipe {
            id: "plank".into(),
            inputs: Bundle::item("wood", 2).plus(&Bundle::coins("gold", 1)).unwrap(),
            outputs: Bundle::item("plank", 1),
            level: 0,
        };
        let mut h = Holdings::new(2);
        h.give(&c, &Bundle::item("wood", 5).plus(&Bundle::coins("gold", 2)).unwrap()).unwrap();
        assert_eq!(craft(&mut h, &c, &plank, 2, 1).unwrap(), Bundle::item("plank", 2));
        assert_eq!((h.inventory.count("wood"), h.inventory.count("plank"), h.wallet.balance("gold")), (1, 2, 0));
        let before = h.clone();
        assert!(craft(&mut h, &c, &plank, 1, 1).is_err(), "out of wood and gold");
        assert_eq!(h, before);
        assert!(craft(&mut h, &c, &Recipe { level: 3, ..plank.clone() }, 1, 2).is_err());
    }
}
