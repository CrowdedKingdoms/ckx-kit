//! Data-driven agent behavior: a small behavior-tree interpreter whose trees
//! live as JSON (in a hub's state or its seed), so designers
//! iterate without recompiling Rust.
//!
//! Node shapes (JSON):
//!   { "type": "sequence", "children": [...] }
//!   { "type": "selector", "children": [...] }
//!   { "type": "condition", "if": "<sensor>", "then": {...} }   sensors below
//!   { "type": "idle" }
//!   { "type": "wander", "range": 12.0, "retarget_ms": 6000 }
//!   { "type": "follow", "target": "owner"|"nearest_player", "stop_at": 2.0 }
//!   { "type": "chase",  "range": 14.0, "stop_at": 1.1 }
//!   { "type": "flee",   "range": 8.0 }
//!   { "type": "patrol", "points": [[x,z], ...], "pause_ms": 2000 }
//!
//! Sensors for `condition.if`: "player_near" (within `range`, default 10),
//! "is_night", "is_day".
//!
//! The interpreter never moves the agent itself: it returns a [`Decision`]
//! the hub turns into steering and emits. Ticks are bounded (tree depth
//! caps at 8, first-decision wins), so fuel stays predictable.

use super::steering::{self, Vec2};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the agent decided to do this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Idle,
    /// Move by this bounded step.
    Step(Vec2),
    /// Nothing matched / tree invalid — caller may fall back.
    None,
}

/// Everything a tree evaluation may look at (the hub fills this in).
pub struct Perception {
    pub pos: Vec2,
    pub home: Vec2,
    /// Nearest player position, when one is visible.
    pub nearest_player: Option<Vec2>,
    /// Owner position (pets), when known.
    pub owner: Option<Vec2>,
    pub is_night: bool,
    pub speed: f32,
    pub dt: f32,
    pub now_ms: u64,
    /// Two uniform rolls in [0,1) for this tick (from the hub's seeded RNG).
    pub roll: (f32, f32),
    /// World half-extent for wander clamping.
    pub bounds: f32,
}

/// Per-agent mutable memory the tree reads/writes across ticks (serde-able
/// into the hub's state).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AgentMemory {
    pub target: Option<(f32, f32)>,
    pub retarget_at_ms: u64,
    pub patrol_index: usize,
    pub pause_until_ms: u64,
}

const MAX_DEPTH: usize = 8;

pub fn evaluate(tree: &Value, p: &Perception, memory: &mut AgentMemory) -> Decision {
    eval_node(tree, p, memory, 0)
}

fn eval_node(node: &Value, p: &Perception, m: &mut AgentMemory, depth: usize) -> Decision {
    if depth > MAX_DEPTH {
        return Decision::None;
    }
    match node["type"].as_str().unwrap_or("") {
        "sequence" | "selector" => {
            // First child returning a real decision wins (bounded work).
            for child in node["children"].as_array().unwrap_or(&Vec::new()) {
                match eval_node(child, p, m, depth + 1) {
                    Decision::None => continue,
                    decision => return decision,
                }
            }
            Decision::None
        }
        "condition" => {
            if sense(node, p) {
                eval_node(&node["then"], p, m, depth + 1)
            } else {
                Decision::None
            }
        }
        "idle" => Decision::Idle,
        "wander" => {
            let range = node["range"].as_f64().unwrap_or(12.0) as f32;
            let retarget_ms = node["retarget_ms"].as_u64().unwrap_or(6_000);
            if m.target.is_none() || p.now_ms >= m.retarget_at_ms {
                let t = steering::wander_target(p.home, range, p.roll.0, p.roll.1, p.bounds);
                m.target = Some((t.x, t.z));
                m.retarget_at_ms = p.now_ms + retarget_ms;
            }
            let (tx, tz) = m.target.unwrap();
            Decision::Step(steering::seek(p.pos, Vec2::new(tx, tz), p.speed, p.dt, 0.4))
        }
        "follow" => {
            let target = match node["target"].as_str().unwrap_or("nearest_player") {
                "owner" => p.owner,
                _ => p.nearest_player,
            };
            let stop_at = node["stop_at"].as_f64().unwrap_or(2.0) as f32;
            match target {
                Some(t) => Decision::Step(steering::arrive(p.pos, t, p.speed, p.dt, stop_at * 2.0)),
                None => Decision::None,
            }
        }
        "chase" => {
            let range = node["range"].as_f64().unwrap_or(14.0) as f32;
            let stop_at = node["stop_at"].as_f64().unwrap_or(1.1) as f32;
            match p.nearest_player {
                Some(t) if p.pos.dist(t) <= range => Decision::Step(steering::seek(p.pos, t, p.speed, p.dt, stop_at)),
                _ => Decision::None,
            }
        }
        "flee" => {
            let range = node["range"].as_f64().unwrap_or(8.0) as f32;
            match p.nearest_player {
                Some(t) if p.pos.dist(t) <= range => Decision::Step(steering::flee(p.pos, t, p.speed, p.dt)),
                _ => Decision::None,
            }
        }
        "patrol" => {
            let points: Vec<Vec2> = node["points"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .filter_map(|pt| Some(Vec2::new(pt[0].as_f64()? as f32, pt[1].as_f64()? as f32)))
                .collect();
            if points.is_empty() {
                return Decision::None;
            }
            if p.now_ms < m.pause_until_ms {
                return Decision::Idle;
            }
            let target = points[m.patrol_index % points.len()];
            if p.pos.dist(target) <= 0.5 {
                m.patrol_index = (m.patrol_index + 1) % points.len();
                m.pause_until_ms = p.now_ms + node["pause_ms"].as_u64().unwrap_or(2_000);
                return Decision::Idle;
            }
            Decision::Step(steering::seek(p.pos, target, p.speed, p.dt, 0.3))
        }
        _ => Decision::None,
    }
}

fn sense(node: &Value, p: &Perception) -> bool {
    match node["if"].as_str().unwrap_or("") {
        "player_near" => {
            let range = node["range"].as_f64().unwrap_or(10.0) as f32;
            p.nearest_player.map(|t| p.pos.dist(t) <= range).unwrap_or(false)
        }
        "is_night" => p.is_night,
        "is_day" => !p.is_night,
        _ => false,
    }
}

/// Canonical trees for the built-in behavior names a game's agent definitions use
/// (`wander_passive`, `wander_hostile`, `chase_hostile`).
pub fn builtin_tree(name: &str) -> Option<Value> {
    let tree = match name {
        "wander_passive" => serde_json::json!({
            "type": "selector",
            "children": [
                { "type": "condition", "if": "player_near", "range": 2.5,
                  "then": { "type": "flee", "range": 2.5 } },
                { "type": "wander", "range": 12.0 }
            ]
        }),
        "wander_hostile" | "chase_hostile" => serde_json::json!({
            "type": "selector",
            "children": [
                { "type": "chase", "range": 14.0, "stop_at": 1.1 },
                { "type": "wander", "range": 12.0 }
            ]
        }),
        "wander" => serde_json::json!({ "type": "wander", "range": 12.0 }),
        "follow_owner" => serde_json::json!({
            "type": "selector",
            "children": [
                { "type": "follow", "target": "owner", "stop_at": 2.0 },
                { "type": "wander", "range": 4.0 }
            ]
        }),
        _ => return None,
    };
    Some(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perception(pos: (f32, f32), player: Option<(f32, f32)>) -> Perception {
        Perception {
            pos: Vec2::new(pos.0, pos.1),
            home: Vec2::default(),
            nearest_player: player.map(|(x, z)| Vec2::new(x, z)),
            owner: None,
            is_night: false,
            speed: 2.0,
            dt: 0.25,
            now_ms: 10_000,
            roll: (0.9, 0.1),
            bounds: 40.0,
        }
    }

    #[test]
    fn hostile_chases_then_wanders() {
        let tree = builtin_tree("chase_hostile").unwrap();
        let mut memory = AgentMemory::default();
        // Player in aggro range: chase step points at the player.
        let d = evaluate(&tree, &perception((0.0, 0.0), Some((5.0, 0.0))), &mut memory);
        match d {
            Decision::Step(step) => assert!(step.x > 0.0),
            other => panic!("expected chase step, got {other:?}"),
        }
        // No player: falls through to wander (sets a target).
        let d = evaluate(&tree, &perception((0.0, 0.0), None), &mut memory);
        assert!(matches!(d, Decision::Step(_)));
        assert!(memory.target.is_some());
    }

    #[test]
    fn passive_flees_close_players() {
        let tree = builtin_tree("wander_passive").unwrap();
        let mut memory = AgentMemory::default();
        let d = evaluate(&tree, &perception((0.0, 0.0), Some((1.0, 0.0))), &mut memory);
        match d {
            Decision::Step(step) => assert!(step.x < 0.0, "flees away"),
            other => panic!("expected flee, got {other:?}"),
        }
    }

    #[test]
    fn patrol_cycles_points() {
        let tree = serde_json::json!({
            "type": "patrol", "points": [[0.0, 0.0], [10.0, 0.0]], "pause_ms": 1000
        });
        let mut memory = AgentMemory::default();
        // Standing on point 0: advances the index and pauses.
        let d = evaluate(&tree, &perception((0.0, 0.0), None), &mut memory);
        assert_eq!(d, Decision::Idle);
        assert_eq!(memory.patrol_index, 1);
        // After the pause: walks toward point 1.
        let mut p = perception((0.0, 0.0), None);
        p.now_ms = 20_000;
        match evaluate(&tree, &p, &mut memory) {
            Decision::Step(step) => assert!(step.x > 0.0),
            other => panic!("expected step, got {other:?}"),
        }
    }

    #[test]
    fn depth_cap_and_unknown_nodes() {
        // Deeply nested selector chain past the cap yields None.
        let mut node = serde_json::json!({ "type": "idle" });
        for _ in 0..12 {
            node = serde_json::json!({ "type": "selector", "children": [node] });
        }
        let mut memory = AgentMemory::default();
        assert_eq!(evaluate(&node, &perception((0.0, 0.0), None), &mut memory), Decision::None);
        assert_eq!(evaluate(&serde_json::json!({"type": "nope"}), &perception((0.0, 0.0), None), &mut memory), Decision::None);
    }

    #[test]
    fn follow_owner_tree() {
        let tree = builtin_tree("follow_owner").unwrap();
        let mut memory = AgentMemory::default();
        let mut p = perception((0.0, 0.0), None);
        p.owner = Some(Vec2::new(8.0, 0.0));
        match evaluate(&tree, &p, &mut memory) {
            Decision::Step(step) => assert!(step.x > 0.0),
            other => panic!("expected follow step, got {other:?}"),
        }
    }
}
