//! Idle production: producers that turn inputs into outputs at a rate, and catch up on any time
//! away in one step, bounded by their inputs and their outputs' storage.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Producer {
    pub id: String,
    /// What a cycle takes from the chain's stock.
    pub inputs: BTreeMap<String, u64>,
    /// What a cycle adds to it.
    pub outputs: BTreeMap<String, u64>,
    pub per_minute: f64,
    /// A cycle begun and not finished, from 0 to 1.
    pub progress: f64,
}

/// Producers in order (an upstream producer before the ones it feeds), their stock, and how
/// much of each resource the stock holds at most.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Chain {
    pub producers: Vec<Producer>,
    pub stock: BTreeMap<String, u64>,
    pub capacity: BTreeMap<String, u64>,
    /// When it last caught up; 0 before it starts.
    pub at: u64,
}

impl Chain {
    /// Starts the clock.
    pub fn start(&mut self, now: u64) {
        if self.at == 0 {
            self.at = now.max(1);
        }
    }

    /// Catches up to `now`; answers what was made. A producer short of inputs or out of storage
    /// runs as many whole cycles as it can and loses the rest of the time.
    pub fn advance(&mut self, now: u64) -> BTreeMap<String, u64> {
        let mut made = BTreeMap::new();
        if self.at == 0 || now <= self.at {
            self.start(now);
            return made;
        }
        let minutes = (now - self.at) as f64 / 60_000.0;
        self.at = now;
        for i in 0..self.producers.len() {
            let p = &self.producers[i];
            let potential = p.per_minute.max(0.0) * minutes + p.progress;
            let mut cycles = potential.floor();
            for (r, &n) in &p.inputs {
                if let Some(supported) = self.stock.get(r).copied().unwrap_or(0).checked_div(n) {
                    cycles = cycles.min(supported as f64);
                }
            }
            for (r, &n) in &p.outputs {
                if let Some(&cap) = self.capacity.get(r)
                    && let Some(fits) = cap.saturating_sub(self.stock.get(r).copied().unwrap_or(0)).checked_div(n)
                {
                    cycles = cycles.min(fits as f64);
                }
            }
            let runs = cycles.max(0.0) as u64;
            let bound = (runs as f64) < potential.floor();
            let p = &mut self.producers[i];
            p.progress = if bound { 0.0 } else { (potential - runs as f64).clamp(0.0, 0.999_999) };
            if runs == 0 {
                continue;
            }
            let (inputs, outputs) = (p.inputs.clone(), p.outputs.clone());
            for (r, n) in inputs {
                let s = self.stock.entry(r).or_insert(0);
                *s = s.saturating_sub(n.saturating_mul(runs));
            }
            for (r, n) in outputs {
                let add = n.saturating_mul(runs);
                *self.stock.entry(r.clone()).or_insert(0) += add;
                *made.entry(r).or_insert(0) += add;
            }
        }
        self.stock.retain(|_, n| *n > 0);
        made
    }

    /// Takes up to `count` of `resource` out of the stock; answers how many.
    pub fn take(&mut self, resource: &str, count: u64) -> u64 {
        let Some(have) = self.stock.get_mut(resource) else { return 0 };
        let taken = count.min(*have);
        *have -= taken;
        if *have == 0 {
            self.stock.remove(resource);
        }
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> Chain {
        Chain {
            producers: vec![
                Producer { id: "mine".into(), outputs: BTreeMap::from([("ore".into(), 1)]), per_minute: 6.0, ..Producer::default() },
                Producer {
                    id: "smelter".into(),
                    inputs: BTreeMap::from([("ore".into(), 2)]),
                    outputs: BTreeMap::from([("ingot".into(), 1)]),
                    per_minute: 2.0,
                    ..Producer::default()
                },
            ],
            ..Chain::default()
        }
    }

    #[test]
    fn a_night_away_catches_up_in_one_step() {
        let mut c = chain();
        c.start(1_000);
        let made = c.advance(1_000 + 10 * 60_000);
        assert_eq!(made["ore"], 60, "the mine runs six a minute");
        assert_eq!(made["ingot"], 20, "the smelter runs two a minute on the mine's ore");
        assert_eq!(c.stock["ore"], 20);
    }

    #[test]
    fn storage_and_inputs_bound_it_and_a_bound_producer_loses_the_rest_of_the_time() {
        let mut c = chain();
        c.capacity.insert("ore".into(), 10);
        c.producers.truncate(1);
        c.start(1);
        c.advance(1 + 60 * 60_000);
        assert_eq!(c.stock["ore"], 10, "the store is full");
        assert_eq!(c.producers[0].progress, 0.0);
        assert_eq!(c.take("ore", 4), 4);
        let made = c.advance(1 + 60 * 60_000 + 30_000);
        assert_eq!(made["ore"], 3, "half a minute at six a minute");
    }

    #[test]
    fn a_partial_cycle_carries_over() {
        let mut c = chain();
        c.producers.truncate(1);
        c.start(1);
        c.advance(1 + 5_000);
        assert!(c.stock.is_empty());
        let made = c.advance(1 + 10_000);
        assert_eq!(made["ore"], 1, "two halves make a cycle");
    }
}
