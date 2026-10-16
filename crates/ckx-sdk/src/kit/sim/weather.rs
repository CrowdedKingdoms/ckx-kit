//! Weather fronts: a seeded state machine that walks through weather kinds
//! with randomized durations, plus broadcast helpers. Clients receive
//! transitions as type-90 server events (see `EVENT_WEATHER`) and render
//! locally; late joiners read the current front from the hub.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::kit::Rng;
use crate::kit::wire;
use crate::node::ChunkPos;
use crate::{Ctx, Result};

/// The platform-reserved server-event type for weather/season transitions.
pub const EVENT_WEATHER: u16 = 90;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WeatherKind {
    pub id: String,
    /// Relative pick weight (0 disables).
    pub weight: u32,
    pub min_ms: u64,
    pub max_ms: u64,
}

pub fn default_kinds() -> Vec<WeatherKind> {
    vec![
        WeatherKind { id: "clear".into(), weight: 6, min_ms: 300_000, max_ms: 900_000 },
        WeatherKind { id: "cloudy".into(), weight: 3, min_ms: 180_000, max_ms: 600_000 },
        WeatherKind { id: "rain".into(), weight: 2, min_ms: 120_000, max_ms: 480_000 },
        WeatherKind { id: "storm".into(), weight: 1, min_ms: 60_000, max_ms: 240_000 },
    ]
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct WeatherState {
    pub current: String,
    pub since_ms: u64,
    pub until_ms: u64,
}

pub struct Transition {
    pub from: String,
    pub to: String,
    pub until_ms: u64,
}

/// Advance the front. Returns the transition when the front changed (emit it
/// to clients), `None` while the current front holds.
pub fn step(state: &mut WeatherState, kinds: &[WeatherKind], now_ms: u64, rng: &mut Rng) -> Option<Transition> {
    if !state.current.is_empty() && now_ms < state.until_ms {
        return None;
    }
    let total: u32 = kinds.iter().map(|k| k.weight).sum();
    if total == 0 {
        return None;
    }
    // Weighted pick, avoiding an immediate repeat when there's a choice.
    let mut pick = || {
        let mut roll = (rng.next_u64() % total as u64) as u32;
        for kind in kinds {
            if roll < kind.weight {
                return kind.clone();
            }
            roll -= kind.weight;
        }
        kinds[0].clone()
    };
    let mut next = pick();
    if next.id == state.current && kinds.iter().filter(|k| k.weight > 0).count() > 1 {
        for _ in 0..4 {
            next = pick();
            if next.id != state.current {
                break;
            }
        }
    }
    let duration = next.min_ms + rng.next_u64() % (next.max_ms.saturating_sub(next.min_ms).max(1));
    let from = std::mem::replace(&mut state.current, next.id.clone());
    state.since_ms = now_ms;
    state.until_ms = now_ms + duration;
    Some(Transition { from, to: next.id, until_ms: state.until_ms })
}

/// The client-facing JSON for a front (both the event body and `forecast`).
pub fn front_json(state: &WeatherState, now_ms: u64) -> Value {
    json!({
        "weather": state.current,
        "sinceMs": state.since_ms,
        "untilMs": state.until_ms,
        "remainingMs": state.until_ms.saturating_sub(now_ms),
    })
}

/// Sends a transition as a type-90 server event to the players within `distance` chunks of
/// `at`, from the hub's `actor`.
pub fn emit_transition(ctx: &Ctx, at: ChunkPos, distance: u8, actor: &[u8; 32], state: &WeatherState, now_ms: u64) -> Result<()> {
    wire::emit_event(ctx, at, distance, actor, EVENT_WEATHER, &front_json(state, now_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fronts_advance_and_hold() {
        let kinds = default_kinds();
        let mut state = WeatherState::default();
        let mut rng = Rng::seeded(7);
        let t = step(&mut state, &kinds, 1_000, &mut rng).expect("first step transitions");
        assert_eq!(t.from, "");
        assert!(!state.current.is_empty());
        assert!(state.until_ms > 1_000);
        // Holds until the front expires.
        let until = state.until_ms;
        assert!(step(&mut state, &kinds, until - 1, &mut rng).is_none());
        let prev = state.current.clone();
        let t2 = step(&mut state, &kinds, until, &mut rng).expect("expiry transitions");
        assert_eq!(t2.from, prev);
        assert_ne!(t2.to, prev, "avoids immediate repeats");
    }

    #[test]
    fn deterministic_for_a_seed() {
        let kinds = default_kinds();
        let mut a = (WeatherState::default(), Rng::seeded(42));
        let mut b = (WeatherState::default(), Rng::seeded(42));
        for t in (0..10_000_000u64).step_by(60_000) {
            let ta = step(&mut a.0, &kinds, t, &mut a.1).map(|x| x.to);
            let tb = step(&mut b.0, &kinds, t, &mut b.1).map(|x| x.to);
            assert_eq!(ta, tb);
        }
    }

    #[test]
    fn front_json_shape() {
        let state = WeatherState { current: "rain".into(), since_ms: 100, until_ms: 5_100 };
        let v = front_json(&state, 1_100);
        assert_eq!(v["weather"], "rain");
        assert_eq!(v["remainingMs"], 4_000);
    }
}
