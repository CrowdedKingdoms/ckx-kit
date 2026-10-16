//! Flow fields: one breadth-first distance field from the goal(s), then any
//! number of agents descend it with O(1) lookups per step — the tower-defense
//! creep mover (many agents, one goal, shared computation). Rebuild the field
//! only when the world changes (tower placed); agents just follow.

use super::path::CostProvider;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// A baked distance field over a bounded grid window. Serde-able so hubs
/// can persist it, but rebuilding from the provider is usually cheaper than
/// storing it durably.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FlowField {
    pub min_x: i32,
    pub min_z: i32,
    pub width: i32,
    pub height: i32,
    /// Distance to the nearest goal per cell; u32::MAX = unreachable/blocked.
    distances: Vec<u32>,
}

impl FlowField {
    /// Build the field inside the window from the goal cells outward,
    /// respecting the provider's blocked cells. Cost = uniform 1 per step
    /// (weighted providers still gate walkability; weighting the descent is
    /// deliberately v2). `max_cells` caps the BFS for fuel safety.
    pub fn build(
        provider: &impl CostProvider,
        window: (i32, i32, i32, i32), // min_x, min_z, width, height
        goals: &[(i32, i32)],
        max_cells: usize,
    ) -> Self {
        let (min_x, min_z, width, height) = window;
        let mut field = Self { min_x, min_z, width, height, distances: vec![u32::MAX; (width * height).max(0) as usize] };
        let mut queue: VecDeque<(i32, i32)> = VecDeque::new();
        for &(gx, gz) in goals {
            if let Some(index) = field.index(gx, gz)
                && provider.cost(gx, gz).is_some()
            {
                field.distances[index] = 0;
                queue.push_back((gx, gz));
            }
        }
        let mut visited = 0usize;
        while let Some((x, z)) = queue.pop_front() {
            visited += 1;
            if visited > max_cells {
                break;
            }
            let here = field.distance(x, z).unwrap_or(u32::MAX);
            for (dx, dz) in [(0, 1), (0, -1), (1, 0), (-1, 0)] {
                let (nx, nz) = (x + dx, z + dz);
                let Some(index) = field.index(nx, nz) else { continue };
                if provider.cost(nx, nz).is_none() {
                    continue;
                }
                if field.distances[index] > here.saturating_add(1) {
                    field.distances[index] = here + 1;
                    queue.push_back((nx, nz));
                }
            }
        }
        field
    }

    fn index(&self, x: i32, z: i32) -> Option<usize> {
        let (dx, dz) = (x - self.min_x, z - self.min_z);
        if dx < 0 || dz < 0 || dx >= self.width || dz >= self.height {
            return None;
        }
        Some((dz * self.width + dx) as usize)
    }

    /// Distance to the nearest goal (None outside the window or unreachable).
    pub fn distance(&self, x: i32, z: i32) -> Option<u32> {
        let d = self.distances[self.index(x, z)?];
        (d != u32::MAX).then_some(d)
    }

    /// The descending step from (x, z): the 4-neighbor with the smallest
    /// distance, when strictly smaller than here. None at goals/unreachable.
    pub fn step(&self, x: i32, z: i32) -> Option<(i32, i32)> {
        let here = self.distance(x, z)?;
        if here == 0 {
            return None; // at a goal
        }
        let mut best: Option<((i32, i32), u32)> = None;
        for (dx, dz) in [(0, 1), (0, -1), (1, 0), (-1, 0)] {
            let (nx, nz) = (x + dx, z + dz);
            if let Some(d) = self.distance(nx, nz)
                && d < here
                && best.is_none_or(|(_, bd)| d < bd)
            {
                best = Some(((dx, dz), d));
            }
        }
        best.map(|(step, _)| step)
    }

    /// Follow the field from (x, z) for up to `max_steps`, returning the
    /// walked cells (excluding the start). Agents usually take 1 per tick;
    /// this helper powers tests and catch-up moves.
    pub fn walk(&self, mut x: i32, mut z: i32, max_steps: usize) -> Vec<(i32, i32)> {
        let mut path = Vec::new();
        for _ in 0..max_steps {
            let Some((dx, dz)) = self.step(x, z) else { break };
            x += dx;
            z += dz;
            path.push((x, z));
        }
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::ai::path::GridProvider;

    fn provider<'a>(blocked: &'a dyn Fn(i32, i32) -> bool) -> GridProvider<'a> {
        GridProvider { width: 32, height: 32, blocked }
    }

    #[test]
    fn field_descends_to_goal() {
        let open = |_x: i32, _z: i32| false;
        let field = FlowField::build(&provider(&open), (0, 0, 32, 32), &[(31, 16)], 100_000);
        assert_eq!(field.distance(31, 16), Some(0));
        assert_eq!(field.distance(0, 16), Some(31));
        // Walking from anywhere reaches the goal in exactly distance steps.
        let path = field.walk(0, 16, 100);
        assert_eq!(path.len(), 31);
        assert_eq!(*path.last().unwrap(), (31, 16));
    }

    #[test]
    fn field_routes_around_walls_and_marks_unreachable() {
        // Wall at x=10 with a gap at z=0.
        let wall = |x: i32, z: i32| x == 10 && z != 0;
        let field = FlowField::build(&provider(&wall), (0, 0, 32, 32), &[(31, 16)], 100_000);
        let path = field.walk(0, 16, 200);
        assert_eq!(*path.last().unwrap(), (31, 16));
        assert!(path.contains(&(10, 0)), "routes through the gap");
        // Fully sealed column: left side unreachable.
        let sealed = |x: i32, _z: i32| x == 10;
        let field = FlowField::build(&provider(&sealed), (0, 0, 32, 32), &[(31, 16)], 100_000);
        assert_eq!(field.distance(0, 16), None);
        assert!(field.walk(0, 16, 10).is_empty());
    }

    #[test]
    fn multiple_goals_pick_nearest() {
        let open = |_x: i32, _z: i32| false;
        let field = FlowField::build(&provider(&open), (0, 0, 32, 32), &[(0, 0), (31, 31)], 100_000);
        assert_eq!(field.distance(2, 2), Some(4));
        assert_eq!(field.distance(29, 30), Some(3));
        // Descent from the middle goes to ONE of the goals.
        let path = field.walk(16, 16, 200);
        let end = *path.last().unwrap();
        assert!(end == (0, 0) || end == (31, 31));
    }
}
