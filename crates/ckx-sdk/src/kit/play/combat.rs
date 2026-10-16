//! The combat referee: the hub decides every hit, its damage and who dies; health never moves on
//! a client's say-so. Pieces a hub keeps in its state:
//!
//! - [`Health`] and [`Combatant`]: hit points, status effects that tick (poison, burning,
//!   regeneration), damage and healing with kill detection.
//! - [`Cooldowns`]: per-actor, per-action minimum intervals (an attack, a cast).
//! - [`in_reach`]: a target in range of the attacker, from positions the hub saw (`presence`).
//! - [`RespawnBook`]: kill credit and respawn timers.
//! - [`ContactBook`]: a hostile's touch on a player, once per cooldown (event 77).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Hit points with a maximum.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Health {
    pub current: i64,
    pub max: i64,
}

/// What damage did.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageOutcome {
    /// Health after it.
    pub health: i64,
    /// Damage actually taken (never more than the health it had).
    pub dealt: i64,
    pub killed: bool,
}

impl Health {
    pub fn new(max: i64) -> Self {
        Self { current: max.max(1), max: max.max(1) }
    }

    pub fn alive(&self) -> bool {
        self.current > 0
    }

    pub fn apply(&mut self, amount: i64) -> DamageOutcome {
        let before = self.current;
        self.current = (self.current - amount.max(0)).max(0);
        DamageOutcome { health: self.current, dealt: before - self.current, killed: before > 0 && self.current == 0 }
    }

    /// Heals up to the maximum; a dead one stays dead. Returns what was healed.
    pub fn heal(&mut self, amount: i64) -> i64 {
        if !self.alive() {
            return 0;
        }
        let before = self.current;
        self.current = (self.current + amount.max(0)).min(self.max);
        self.current - before
    }

    pub fn revive(&mut self) {
        self.current = self.max;
    }
}

/// A status effect: `per_tick` damage (negative heals) every `every_ms` until `until_ms`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Status {
    pub kind: String,
    pub until_ms: u64,
    pub per_tick: i64,
    pub every_ms: u64,
    pub next_ms: u64,
    /// Who applied it, for kill credit.
    #[serde(default)]
    pub source: Option<String>,
}

/// Something that fights: health and the effects on it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Combatant {
    pub health: Health,
    #[serde(default)]
    pub statuses: Vec<Status>,
    #[serde(default)]
    pub kills: u32,
    #[serde(default)]
    pub deaths: u32,
}

/// What a status tick did to a combatant.
#[derive(Clone, Debug, PartialEq)]
pub struct TickEffect {
    pub kind: String,
    pub outcome: DamageOutcome,
    pub source: Option<String>,
}

impl Combatant {
    pub fn new(max_hp: i64) -> Self {
        Self { health: Health::new(max_hp), ..Default::default() }
    }

    /// Applies `kind` (replacing one of the same kind), first ticking `every_ms` from `now`.
    pub fn afflict(&mut self, kind: &str, per_tick: i64, every_ms: u64, duration_ms: u64, source: Option<String>, now: u64) {
        self.statuses.retain(|s| s.kind != kind);
        let every_ms = every_ms.max(100);
        self.statuses.push(Status {
            kind: kind.to_owned(),
            until_ms: now + duration_ms,
            per_tick,
            every_ms,
            next_ms: now + every_ms,
            source,
        });
    }

    pub fn has(&self, kind: &str) -> bool {
        self.statuses.iter().any(|s| s.kind == kind)
    }

    /// Runs every tick due by `now`, drops expired effects, and returns what each did.
    pub fn tick(&mut self, now: u64) -> Vec<TickEffect> {
        let mut out = Vec::new();
        for s in &mut self.statuses {
            while s.next_ms <= now.min(s.until_ms) && self.health.alive() {
                s.next_ms += s.every_ms;
                let outcome = if s.per_tick >= 0 {
                    self.health.apply(s.per_tick)
                } else {
                    let healed = self.health.heal(-s.per_tick);
                    DamageOutcome { health: self.health.current, dealt: -healed, killed: false }
                };
                out.push(TickEffect { kind: s.kind.clone(), outcome, source: s.source.clone() });
            }
        }
        self.statuses.retain(|s| s.until_ms > now && s.next_ms <= s.until_ms);
        if out.iter().any(|e| e.outcome.killed) {
            self.deaths += 1;
            self.statuses.clear();
        }
        out
    }

    /// Damage from `amount`; counts the death.
    pub fn hit(&mut self, amount: i64) -> DamageOutcome {
        let outcome = self.health.apply(amount);
        if outcome.killed {
            self.deaths += 1;
            self.statuses.clear();
        }
        outcome
    }
}

/// Per-actor, per-action last-use times.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Cooldowns {
    #[serde(default)]
    pub last: BTreeMap<String, u64>,
}

impl Cooldowns {
    fn key(actor: &str, action: &str) -> String {
        format!("{actor}\u{0}{action}")
    }

    /// Whether `actor` may `action` at `now` with `cooldown_ms` between uses; when it may, the
    /// use is taken.
    pub fn take(&mut self, actor: &str, action: &str, cooldown_ms: u64, now: u64) -> bool {
        let k = Self::key(actor, action);
        if self.last.get(&k).is_some_and(|at| now < at + cooldown_ms) {
            return false;
        }
        self.last.insert(k, now);
        true
    }

    /// Milliseconds until `actor` may `action` again; 0 when it may now.
    pub fn remaining(&self, actor: &str, action: &str, cooldown_ms: u64, now: u64) -> u64 {
        self.last.get(&Self::key(actor, action)).map(|at| (at + cooldown_ms).saturating_sub(now)).unwrap_or(0)
    }

    /// Forgets uses older than `older_than_ms`.
    pub fn prune(&mut self, now: u64, older_than_ms: u64) {
        self.last.retain(|_, at| now.saturating_sub(*at) < older_than_ms);
    }
}

/// Whether `target` is within `range` chunks of `attacker` on the ground.
pub fn in_reach(attacker: (f64, f64), target: (f64, f64), range: f64) -> bool {
    super::super::presence::ground_distance(attacker, target) <= range
}

/// A kill: who did it, when, and when the victim comes back.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KillRecord {
    pub killer: String,
    pub at_ms: u64,
    pub respawn_at_ms: u64,
}

/// Kill credit and respawn timers, by victim id.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RespawnBook {
    #[serde(default)]
    pub kills: BTreeMap<String, KillRecord>,
}

impl RespawnBook {
    pub fn record_kill(&mut self, victim: &str, killer: &str, now: u64, respawn_ms: u64) {
        self.kills.insert(victim.to_owned(), KillRecord { killer: killer.to_owned(), at_ms: now, respawn_at_ms: now + respawn_ms });
    }

    /// Victims whose respawn came due, taken out of the book.
    pub fn due(&mut self, now: u64) -> Vec<String> {
        let ready: Vec<String> = self.kills.iter().filter(|(_, r)| now >= r.respawn_at_ms).map(|(id, _)| id.clone()).collect();
        for id in &ready {
            self.kills.remove(id);
        }
        ready
    }

    pub fn is_down(&self, victim: &str) -> bool {
        self.kills.contains_key(victim)
    }
}

/// A hostile's touch on a player, at most once per victim per cooldown.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ContactBook {
    #[serde(default)]
    pub at: BTreeMap<String, u64>,
}

impl ContactBook {
    /// Whether the hostile at `attacker` touches the victim at `victim` (within `range`) now;
    /// when it does, the victim's cooldown starts.
    pub fn touch(&mut self, victim: &str, victim_at: (f64, f64), attacker: (f64, f64), range: f64, cooldown_ms: u64, now: u64) -> bool {
        if self.at.get(victim).is_some_and(|t| now.saturating_sub(*t) < cooldown_ms) || !in_reach(attacker, victim_at, range) {
            return false;
        }
        self.at.insert(victim.to_owned(), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_kills_once_and_healing_stops_at_the_maximum() {
        let mut h = Health::new(10);
        assert_eq!(h.apply(4), DamageOutcome { health: 6, dealt: 4, killed: false });
        assert_eq!(h.heal(99), 4);
        assert_eq!(h.apply(30), DamageOutcome { health: 0, dealt: 10, killed: true });
        assert!(!h.apply(1).killed, "dead already");
        assert_eq!(h.heal(5), 0, "the dead stay dead");
    }

    #[test]
    fn statuses_tick_on_time_expire_and_credit_their_source() {
        let mut c = Combatant::new(20);
        c.afflict("poison", 3, 1_000, 3_500, Some("p7".into()), 0);
        assert!(c.tick(500).is_empty());
        let ticks = c.tick(3_600);
        assert_eq!(ticks.len(), 3);
        assert_eq!(c.health.current, 11);
        assert!(!c.has("poison"), "expired");
        c.afflict("regen", -5, 1_000, 10_000, None, 4_000);
        c.tick(6_000);
        assert_eq!(c.health.current, 20, "healing stops at the maximum");
        c.afflict("burn", 50, 500, 2_000, Some("p9".into()), 7_000);
        let burn = c.tick(7_600);
        assert!(burn.last().unwrap().outcome.killed && burn.last().unwrap().source.as_deref() == Some("p9"));
        assert_eq!((c.deaths, c.statuses.len()), (1, 0));
    }

    #[test]
    fn cooldowns_respawns_and_contact() {
        let mut cd = Cooldowns::default();
        assert!(cd.take("p1", "attack", 500, 0));
        assert!(!cd.take("p1", "attack", 500, 300));
        assert_eq!(cd.remaining("p1", "attack", 500, 300), 200);
        assert!(cd.take("p1", "cast", 500, 300), "another action");
        let mut rb = RespawnBook::default();
        rb.record_kill("mob-3", "p1", 0, 1_000);
        assert!(rb.is_down("mob-3"));
        assert_eq!(rb.due(1_000), vec!["mob-3".to_string()]);
        let mut cb = ContactBook::default();
        assert!(!cb.touch("p1", (0.0, 0.0), (3.0, 0.0), 1.5, 1_000, 0), "too far");
        assert!(cb.touch("p1", (0.0, 0.0), (1.0, 0.0), 1.5, 1_000, 0));
        assert!(!cb.touch("p1", (0.0, 0.0), (1.0, 0.0), 1.5, 1_000, 500), "cooldown");
    }
}
