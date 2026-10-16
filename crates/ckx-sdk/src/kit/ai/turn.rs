//! Budgeted heuristic movers for enemy phases in turn-based games:
//! enumerate reachable cells under a movement budget, score
//! each candidate with a game-supplied heuristic, pick the best — all capped
//! by an expansion budget so fuel stays predictable. Depth-1 "minimax" is
//! the heuristic itself; deeper search is deliberately out of scope v1.

use super::path::CostProvider;
use std::collections::{BTreeMap, VecDeque};

/// One candidate destination for a turn move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    pub x: i32,
    pub z: i32,
    /// Movement cost spent to reach it.
    pub cost: u32,
}

/// Enumerate the cells reachable from `from` with at most `move_points`
/// total entry cost (BFS/Dijkstra-lite over the provider, 4-connected),
/// including standing still. Expansion stops at `max_expanded` cells.
pub fn reachable(provider: &impl CostProvider, from: (i32, i32), move_points: u32, max_expanded: usize) -> Vec<Candidate> {
    let mut best_cost: BTreeMap<(i32, i32), u32> = BTreeMap::new();
    let mut queue: VecDeque<((i32, i32), u32)> = VecDeque::new();
    best_cost.insert(from, 0);
    queue.push_back((from, 0));
    let mut expanded = 0usize;

    while let Some((pos, cost)) = queue.pop_front() {
        expanded += 1;
        if expanded > max_expanded {
            break;
        }
        for (dx, dz) in [(0, 1), (0, -1), (1, 0), (-1, 0)] {
            let next = (pos.0 + dx, pos.1 + dz);
            let Some(step) = provider.cost(next.0, next.1) else { continue };
            let total = cost.saturating_add(step);
            if total > move_points {
                continue;
            }
            if total < *best_cost.get(&next).unwrap_or(&u32::MAX) {
                best_cost.insert(next, total);
                queue.push_back((next, total));
            }
        }
    }
    best_cost.into_iter().map(|((x, z), cost)| Candidate { x, z, cost }).collect()
}

/// The chosen move for a turn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnMove {
    pub to: (i32, i32),
    pub score: f32,
}

/// Pick the best reachable cell under `move_points` by `score` (higher wins;
/// ties prefer cheaper movement, then stable coordinate order). Returns
/// None only when even standing still is impossible (blocked origin).
pub fn best_move(
    provider: &impl CostProvider,
    from: (i32, i32),
    move_points: u32,
    max_expanded: usize,
    mut score: impl FnMut(&Candidate) -> f32,
) -> Option<TurnMove> {
    let candidates = reachable(provider, from, move_points, max_expanded);
    let mut best: Option<(f32, u32, (i32, i32))> = None;
    for candidate in &candidates {
        let s = score(candidate);
        let key = (s, candidate.cost, (candidate.x, candidate.z));
        let better = match &best {
            None => true,
            Some((bs, bc, bp)) => {
                s > *bs || (s == *bs && candidate.cost < *bc) || (s == *bs && candidate.cost == *bc && (candidate.x, candidate.z) < *bp)
            }
        };
        if better {
            best = Some((key.0, key.1, key.2));
        }
    }
    best.map(|(s, _, to)| TurnMove { to, score: s })
}

/// The classic melee-enemy heuristic: get adjacent to the closest target,
/// preferring cells that minimize distance to ANY target. Returns a score
/// function for [`best_move`].
pub fn approach_scorer(targets: Vec<(i32, i32)>) -> impl FnMut(&Candidate) -> f32 {
    move |candidate: &Candidate| {
        let d = targets.iter().map(|t| (t.0 - candidate.x).abs() + (t.1 - candidate.z).abs()).min().unwrap_or(0);
        -(d as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::ai::path::GridProvider;

    fn open_field<'a>(blocked: &'a dyn Fn(i32, i32) -> bool) -> GridProvider<'a> {
        GridProvider { width: 16, height: 16, blocked }
    }

    #[test]
    fn reachable_respects_budget_and_walls() {
        let open = |_x: i32, _z: i32| false;
        let cells = reachable(&open_field(&open), (8, 8), 2, 10_000);
        // Manhattan diamond radius 2 = 13 cells (incl. origin).
        assert_eq!(cells.len(), 13);
        assert!(cells.iter().any(|c| c.x == 8 && c.z == 8 && c.cost == 0));

        let wall = |x: i32, _z: i32| x == 9; // wall right of origin
        let cells = reachable(&open_field(&wall), (8, 8), 2, 10_000);
        assert!(!cells.iter().any(|c| c.x == 9));
        assert!(!cells.iter().any(|c| c.x == 10), "cannot pass through the wall with 2 mp");
    }

    #[test]
    fn best_move_approaches_target() {
        let open = |_x: i32, _z: i32| false;
        let provider = open_field(&open);
        let chosen = best_move(&provider, (2, 2), 3, 10_000, approach_scorer(vec![(10, 2)])).unwrap();
        assert_eq!(chosen.to, (5, 2), "walks straight toward the target");
        // Already adjacent: stays adjacent (distance 1 beats distance 0? No —
        // approach minimizes distance, landing ON the target cell if open).
        let chosen = best_move(&provider, (9, 2), 1, 10_000, approach_scorer(vec![(10, 2)])).unwrap();
        assert_eq!(chosen.to, (10, 2));
    }

    #[test]
    fn best_move_prefers_cheaper_on_ties_and_survives_blocked_budget() {
        let open = |_x: i32, _z: i32| false;
        let provider = open_field(&open);
        // Constant score: standing still (cost 0) wins the tie.
        let chosen = best_move(&provider, (4, 4), 3, 10_000, |_| 1.0).unwrap();
        assert_eq!(chosen.to, (4, 4));
        // Zero move points: only standing still is reachable.
        let chosen = best_move(&provider, (4, 4), 0, 10_000, approach_scorer(vec![(9, 9)])).unwrap();
        assert_eq!(chosen.to, (4, 4));
    }
}
