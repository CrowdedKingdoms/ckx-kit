//! Levels from experience ([`Levels`], [`Progress`]); achievements and quests that count what a
//! player did ([`AchievementDef`], [`QuestDef`], [`Record`]).
//!
//! What a player did reaches the record as counters (`wins`, `kill:wolf`, `crafted:plank`).
//! A hub that decided it reports it; a counter a game lets the client report itself (a
//! tutorial step) is for things worth nothing.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::holdings::Bundle;
use crate::Result;
use crate::kit::{codes, error};

/// Experience to go from level `n` to `n + 1` is `base * growth^(n - 1)`, up to `max`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Levels {
    pub base: u64,
    pub growth: f64,
    pub max: u32,
}

impl Default for Levels {
    fn default() -> Self {
        Self { base: 100, growth: 1.5, max: 50 }
    }
}

impl Levels {
    /// What level `level` takes to finish; `None` at the top.
    pub fn to_next(&self, level: u32) -> Option<u64> {
        if level >= self.max {
            return None;
        }
        let need = self.base as f64 * self.growth.max(1.0).powi(level.saturating_sub(1) as i32);
        Some(need.min(u64::MAX as f64).round().max(1.0) as u64)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Progress {
    pub level: u32,
    /// Toward the next level.
    pub xp: u64,
    pub total_xp: u64,
}

impl Default for Progress {
    fn default() -> Self {
        Self { level: 1, xp: 0, total_xp: 0 }
    }
}

impl Progress {
    /// Adds experience; answers how many levels it gained.
    pub fn gain(&mut self, levels: &Levels, xp: u64) -> u32 {
        self.total_xp = self.total_xp.saturating_add(xp);
        self.xp = self.xp.saturating_add(xp);
        let mut gained = 0;
        while let Some(need) = levels.to_next(self.level) {
            if self.xp < need {
                break;
            }
            self.xp -= need;
            self.level += 1;
            gained += 1;
        }
        if levels.to_next(self.level).is_none() {
            self.xp = 0;
        }
        gained
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AchievementDef {
    pub id: String,
    pub counter: String,
    pub target: u64,
    #[serde(default)]
    pub reward: Bundle,
    #[serde(default)]
    pub xp: u64,
    /// Not shown until unlocked.
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Objective {
    pub counter: String,
    pub target: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct QuestDef {
    pub id: String,
    pub objectives: Vec<Objective>,
    #[serde(default)]
    pub reward: Bundle,
    #[serde(default)]
    pub xp: u64,
    /// Quests done first.
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub level: u32,
    /// A first-session tutorial: taken on as the player starts.
    #[serde(default)]
    pub tutorial: bool,
}

/// What a player did and what it earned them.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Record {
    pub counters: BTreeMap<String, u64>,
    /// Achievement to when it unlocked.
    pub unlocked: BTreeMap<String, u64>,
    /// Quest taken on, to each objective's count since.
    pub active: BTreeMap<String, Vec<u64>>,
    /// Done, waiting for its reward to be claimed.
    pub complete: BTreeSet<String>,
    /// Claimed, to when.
    pub done: BTreeMap<String, u64>,
}

/// What one count changed.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Unlocks {
    pub achievements: Vec<String>,
    pub quests: Vec<String>,
}

impl Record {
    /// Counts `n` of `counter`: the achievements it unlocks and the quests it completes.
    pub fn count(&mut self, achievements: &[AchievementDef], quests: &[QuestDef], counter: &str, n: u64, now: u64) -> Unlocks {
        let mut out = Unlocks::default();
        let total = self.counters.entry(counter.to_owned()).or_insert(0);
        *total = total.saturating_add(n);
        let total = *total;
        for a in achievements.iter().filter(|a| a.counter == counter && total >= a.target) {
            if !self.unlocked.contains_key(&a.id) {
                self.unlocked.insert(a.id.clone(), now);
                out.achievements.push(a.id.clone());
            }
        }
        for q in quests {
            let Some(counts) = self.active.get_mut(&q.id) else { continue };
            for (i, o) in q.objectives.iter().enumerate() {
                if o.counter == counter
                    && let Some(c) = counts.get_mut(i)
                {
                    *c = (*c).saturating_add(n).min(o.target);
                }
            }
            if q.objectives.iter().zip(counts.iter()).all(|(o, c)| *c >= o.target) {
                self.active.remove(&q.id);
                self.complete.insert(q.id.clone());
                out.quests.push(q.id.clone());
            }
        }
        out
    }

    /// Takes on quest `q`: `conflict` when it is taken, done or waits for another, `forbidden`
    /// below its level.
    pub fn accept(&mut self, q: &QuestDef, level: u32) -> Result<()> {
        if self.active.contains_key(&q.id) || self.complete.contains(&q.id) || self.done.contains_key(&q.id) {
            return Err(error(codes::CONFLICT, format!("`{}` is taken or done", q.id)));
        }
        if let Some(missing) = q.requires.iter().find(|r| !self.done.contains_key(*r)) {
            return Err(error(codes::CONFLICT, format!("`{}` comes after `{missing}`", q.id)));
        }
        if level < q.level {
            return Err(error(codes::FORBIDDEN, format!("`{}` needs level {}", q.id, q.level)));
        }
        self.active.insert(q.id.clone(), vec![0; q.objectives.len()]);
        Ok(())
    }

    /// Claims quest `id`'s reward: `not_ready` until it is complete.
    pub fn claim(&mut self, id: &str, now: u64) -> Result<()> {
        if !self.complete.remove(id) {
            return Err(error(codes::NOT_READY, format!("`{id}` is not complete")));
        }
        self.done.insert(id.to_owned(), now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experience_levels_up_along_the_curve_and_stops_at_the_top() {
        let levels = Levels { base: 100, growth: 2.0, max: 3 };
        let mut p = Progress::default();
        assert_eq!(p.gain(&levels, 250), 1);
        assert_eq!((p.level, p.xp), (2, 150));
        assert_eq!(levels.to_next(2), Some(200));
        assert_eq!(p.gain(&levels, 1_000), 1);
        assert_eq!((p.level, p.xp, p.total_xp), (3, 0, 1_250), "the top keeps no progress");
    }

    #[test]
    fn counts_unlock_achievements_once_and_complete_quests_taken_on() {
        let achievements = vec![AchievementDef {
            id: "lumberjack".into(),
            counter: "chop".into(),
            target: 3,
            reward: Bundle::default(),
            xp: 0,
            hidden: false,
        }];
        let quests = vec![
            QuestDef {
                id: "first".into(),
                objectives: vec![Objective { counter: "chop".into(), target: 2 }],
                reward: Bundle::coins("gold", 5),
                xp: 10,
                requires: vec![],
                level: 0,
                tutorial: true,
            },
            QuestDef {
                id: "second".into(),
                objectives: vec![Objective { counter: "chop".into(), target: 1 }],
                reward: Bundle::default(),
                xp: 0,
                requires: vec!["first".into()],
                level: 0,
                tutorial: false,
            },
        ];
        let mut r = Record::default();
        r.count(&achievements, &quests, "chop", 1, 0);
        r.accept(&quests[0], 1).unwrap();
        assert!(r.accept(&quests[1], 1).is_err(), "after the first");
        assert_eq!(r.count(&achievements, &quests, "chop", 1, 1).quests, Vec::<String>::new(), "counts from when it was taken on");
        let u = r.count(&achievements, &quests, "chop", 5, 2);
        assert_eq!((u.achievements, u.quests), (vec!["lumberjack".to_string()], vec!["first".to_string()]));
        assert!(r.count(&achievements, &quests, "chop", 1, 3).achievements.is_empty(), "unlocked once");
        assert!(r.claim("second", 4).is_err());
        r.claim("first", 4).unwrap();
        assert!(r.claim("first", 5).is_err(), "claimed once");
        r.accept(&quests[1], 1).unwrap();
    }
}
