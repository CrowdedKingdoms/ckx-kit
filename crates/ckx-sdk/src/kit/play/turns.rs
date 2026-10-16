//! Turns: initiative ordering, turn/round advance, per-turn timers
//! with timeout policies, and simultaneous reveal. Pure state machine over
//! `now_ms` — a hub keeps it in its state and drives it from
//! timers and calls; it never talks to the platform itself.

use serde::{Deserialize, Serialize};

/// What happens when a turn timer expires.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
pub enum TimeoutPolicy {
    /// The actor's turn is skipped (turn passes on).
    #[default]
    Skip,
    /// The actor forfeits and is removed from the order.
    Forfeit,
    /// Nothing happens automatically (a human host decides).
    Wait,
}

/// One participant in the order.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TurnActor {
    /// Stable id (a player id, a unit's id, ...).
    pub id: String,
    /// Higher acts first; ties keep insertion order (stable sort).
    pub initiative: i64,
    /// Forfeited/dead actors stay recorded but are skipped.
    pub active: bool,
}

/// An event the hub should announce (turn changed, round advanced, ...).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum TurnEvent {
    TurnStarted {
        actor_id: String,
        round: u32,
        turn_in_round: u32,
    },
    RoundStarted {
        round: u32,
    },
    ActorSkipped {
        actor_id: String,
    },
    ActorForfeited {
        actor_id: String,
    },
    /// One (or zero) active actors remain.
    OrderExhausted,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TurnOrder {
    pub actors: Vec<TurnActor>,
    /// Index into `actors` of the current turn holder.
    pub current: usize,
    pub round: u32,
    pub turn_in_round: u32,
    /// Current turn deadline (0 = no timer).
    pub deadline_ms: u64,
    pub turn_duration_ms: u64,
    pub timeout_policy: TimeoutPolicy,
    started: bool,
}

impl TurnOrder {
    /// Build an order from (id, initiative) pairs; sorts by initiative
    /// descending (stable — ties keep the given order).
    pub fn new(actors: impl IntoIterator<Item = (String, i64)>, turn_duration_ms: u64, timeout_policy: TimeoutPolicy) -> Self {
        let mut list: Vec<TurnActor> = actors.into_iter().map(|(id, initiative)| TurnActor { id, initiative, active: true }).collect();
        list.sort_by_key(|b| std::cmp::Reverse(b.initiative));
        Self { actors: list, current: 0, round: 0, turn_in_round: 0, deadline_ms: 0, turn_duration_ms, timeout_policy, started: false }
    }

    pub fn active_count(&self) -> usize {
        self.actors.iter().filter(|a| a.active).count()
    }

    /// The current turn holder (None before start / after exhaustion).
    pub fn current_actor(&self) -> Option<&TurnActor> {
        if !self.started {
            return None;
        }
        self.actors.get(self.current).filter(|a| a.active)
    }

    /// Begin round 1, turn 1. Returns the announce events.
    pub fn start(&mut self, now_ms: u64) -> Vec<TurnEvent> {
        if self.started || self.active_count() == 0 {
            return Vec::new();
        }
        self.started = true;
        self.round = 1;
        self.turn_in_round = 1;
        self.current = self.first_active_from(0);
        self.arm_timer(now_ms);
        vec![
            TurnEvent::RoundStarted { round: 1 },
            TurnEvent::TurnStarted { actor_id: self.actors[self.current].id.clone(), round: 1, turn_in_round: 1 },
        ]
    }

    /// Advance to the next active actor (call when the current turn ends).
    pub fn advance(&mut self, now_ms: u64) -> Vec<TurnEvent> {
        if !self.started {
            return Vec::new();
        }
        let mut events = Vec::new();
        if self.active_count() <= 1 {
            events.push(TurnEvent::OrderExhausted);
            return events;
        }
        let mut index = self.current;
        loop {
            index += 1;
            if index >= self.actors.len() {
                index = 0;
                self.round += 1;
                self.turn_in_round = 0;
                events.push(TurnEvent::RoundStarted { round: self.round });
            }
            if self.actors[index].active {
                break;
            }
        }
        self.current = index;
        self.turn_in_round += 1;
        self.arm_timer(now_ms);
        events.push(TurnEvent::TurnStarted {
            actor_id: self.actors[index].id.clone(),
            round: self.round,
            turn_in_round: self.turn_in_round,
        });
        events
    }

    /// Apply the timeout policy if the current turn's deadline passed.
    pub fn tick(&mut self, now_ms: u64) -> Vec<TurnEvent> {
        if !self.started || self.deadline_ms == 0 || now_ms < self.deadline_ms {
            return Vec::new();
        }
        let actor_id = match self.current_actor() {
            Some(actor) => actor.id.clone(),
            None => return Vec::new(),
        };
        match self.timeout_policy {
            TimeoutPolicy::Wait => Vec::new(),
            TimeoutPolicy::Skip => {
                let mut events = vec![TurnEvent::ActorSkipped { actor_id }];
                events.extend(self.advance(now_ms));
                events
            }
            TimeoutPolicy::Forfeit => {
                let mut events = vec![TurnEvent::ActorForfeited { actor_id: actor_id.clone() }];
                events.extend(self.forfeit(&actor_id, now_ms));
                events
            }
        }
    }

    /// Remove an actor from the running order (death, rage-quit). Advances
    /// the turn when it was theirs.
    pub fn forfeit(&mut self, actor_id: &str, now_ms: u64) -> Vec<TurnEvent> {
        let Some(index) = self.actors.iter().position(|a| a.id == actor_id && a.active) else {
            return Vec::new();
        };
        self.actors[index].active = false;
        if self.active_count() <= 1 {
            return vec![TurnEvent::OrderExhausted];
        }
        if index == self.current && self.started {
            // Their turn: pass it on. advance() skips inactive actors.
            return self.advance(now_ms);
        }
        Vec::new()
    }

    /// Milliseconds left on the current turn (None = no timer / not started).
    pub fn remaining_ms(&self, now_ms: u64) -> Option<u64> {
        (self.started && self.deadline_ms > 0).then(|| self.deadline_ms.saturating_sub(now_ms))
    }

    fn arm_timer(&mut self, now_ms: u64) {
        self.deadline_ms = if self.turn_duration_ms > 0 { now_ms + self.turn_duration_ms } else { 0 };
    }

    fn first_active_from(&self, start: usize) -> usize {
        (start..self.actors.len()).find(|&i| self.actors[i].active).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Simultaneous reveal: everyone commits a move, then all resolve at once.
// ---------------------------------------------------------------------------

/// Collects one hidden submission per actor, then resolves when everyone
/// (or the deadline) is in. Submissions stay in the hub until the reveal.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RevealRound {
    pub expected: Vec<String>,
    pub submissions: Vec<(String, serde_json::Value)>,
    pub deadline_ms: u64,
}

impl RevealRound {
    pub fn new(expected: Vec<String>, now_ms: u64, window_ms: u64) -> Self {
        Self { expected, submissions: Vec::new(), deadline_ms: if window_ms > 0 { now_ms + window_ms } else { 0 } }
    }

    /// Record a submission. Duplicates and strangers are rejected.
    pub fn submit(&mut self, actor_id: &str, payload: serde_json::Value) -> Result<(), &'static str> {
        if !self.expected.iter().any(|id| id == actor_id) {
            return Err("not a participant");
        }
        if self.submissions.iter().any(|(id, _)| id == actor_id) {
            return Err("already submitted");
        }
        self.submissions.push((actor_id.to_string(), payload));
        Ok(())
    }

    pub fn complete(&self) -> bool {
        self.submissions.len() == self.expected.len()
    }

    /// Ready to reveal: everyone submitted, or the window closed.
    pub fn ready(&self, now_ms: u64) -> bool {
        self.complete() || (self.deadline_ms > 0 && now_ms >= self.deadline_ms)
    }

    /// Consume and return all submissions (the reveal).
    pub fn reveal(self) -> Vec<(String, serde_json::Value)> {
        self.submissions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn order3() -> TurnOrder {
        TurnOrder::new([("slow".into(), 3), ("fast".into(), 9), ("mid".into(), 5)], 10_000, TimeoutPolicy::Skip)
    }

    #[test]
    fn initiative_orders_and_rounds_advance() {
        let mut order = order3();
        let events = order.start(1_000);
        assert!(matches!(&events[1], TurnEvent::TurnStarted { actor_id, round: 1, .. } if actor_id == "fast"));
        assert_eq!(order.current_actor().unwrap().id, "fast");

        order.advance(2_000);
        assert_eq!(order.current_actor().unwrap().id, "mid");
        order.advance(3_000);
        assert_eq!(order.current_actor().unwrap().id, "slow");
        // Wrap: new round.
        let events = order.advance(4_000);
        assert!(events.iter().any(|e| matches!(e, TurnEvent::RoundStarted { round: 2 })));
        assert_eq!(order.current_actor().unwrap().id, "fast");
        assert_eq!(order.round, 2);
    }

    #[test]
    fn timeout_skips_and_forfeit_policy_removes() {
        let mut order = order3();
        order.start(0);
        assert!(order.tick(9_999).is_empty(), "before the deadline");
        let events = order.tick(10_000);
        assert!(matches!(&events[0], TurnEvent::ActorSkipped { actor_id } if actor_id == "fast"));
        assert_eq!(order.current_actor().unwrap().id, "mid");

        let mut hard = TurnOrder::new([("a".into(), 2), ("b".into(), 1)], 5_000, TimeoutPolicy::Forfeit);
        hard.start(0);
        let events = hard.tick(5_000);
        assert!(matches!(&events[0], TurnEvent::ActorForfeited { actor_id } if actor_id == "a"));
        assert!(events.iter().any(|e| matches!(e, TurnEvent::OrderExhausted)));
        assert_eq!(hard.active_count(), 1);
    }

    #[test]
    fn forfeit_mid_order_passes_turn() {
        let mut order = order3();
        order.start(0);
        // "mid" (not current) forfeits: current turn unaffected.
        assert!(order.forfeit("mid", 1_000).is_empty());
        assert_eq!(order.current_actor().unwrap().id, "fast");
        // Current forfeits: turn passes to the next active ("slow").
        let events = order.forfeit("fast", 2_000);
        assert!(events.iter().any(|e| matches!(e, TurnEvent::OrderExhausted)));
    }

    #[test]
    fn reveal_round_collects_then_reveals() {
        let mut round = RevealRound::new(vec!["a".into(), "b".into()], 0, 30_000);
        assert_eq!(round.submit("x", json!(1)).unwrap_err(), "not a participant");
        round.submit("a", json!("rock")).unwrap();
        assert_eq!(round.submit("a", json!("paper")).unwrap_err(), "already submitted");
        assert!(!round.ready(1_000), "waiting on b");
        assert!(round.ready(30_000), "window closed");
        round.submit("b", json!("scissors")).unwrap();
        assert!(round.complete() && round.ready(1_000));
        let submissions = round.reveal();
        assert_eq!(submissions.len(), 2);
    }

    #[test]
    fn serde_roundtrip() {
        let mut order = order3();
        order.start(0);
        let json = serde_json::to_string(&order).unwrap();
        let restored: TurnOrder = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.current_actor().unwrap().id, "fast");
    }
}
