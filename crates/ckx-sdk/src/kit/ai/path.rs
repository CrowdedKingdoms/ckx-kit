//! Budget-capped A* over a pluggable cost provider. Under the platform's fuel
//! metering A* costs a little more than natively; the budget cap keeps a
//! pathological query from eating a call's fuel.

/// Walkability/cost source. Implement over your world representation
/// (obstacle grids, voxel columns, nav data in a hub's state).
pub trait CostProvider {
    /// Cost to ENTER (x, z); `None` = blocked. Uniform worlds return Some(1).
    fn cost(&self, x: i32, z: i32) -> Option<u32>;
}

/// A grid of booleans (true = blocked) — the simplest provider, also the
/// test fixture.
pub struct GridProvider<'a> {
    pub width: i32,
    pub height: i32,
    pub blocked: &'a dyn Fn(i32, i32) -> bool,
}

impl CostProvider for GridProvider<'_> {
    fn cost(&self, x: i32, z: i32) -> Option<u32> {
        if x < 0 || z < 0 || x >= self.width || z >= self.height || (self.blocked)(x, z) { None } else { Some(1) }
    }
}

pub struct PathConfig {
    /// Max nodes expanded before giving up (the fuel guard). ~4x the
    /// manhattan distance is plenty for sane worlds.
    pub max_expanded: usize,
}

impl Default for PathConfig {
    fn default() -> Self {
        Self { max_expanded: 4_096 }
    }
}

/// 4-connected A*. Returns the path INCLUDING both endpoints, or empty when
/// unreachable / budget-exhausted / endpoints blocked.
pub fn a_star(provider: &impl CostProvider, from: (i32, i32), to: (i32, i32), config: &PathConfig) -> Vec<(i32, i32)> {
    use std::collections::{BTreeMap, BinaryHeap};

    if provider.cost(from.0, from.1).is_none() || provider.cost(to.0, to.1).is_none() {
        return Vec::new();
    }
    if from == to {
        return vec![from];
    }

    #[derive(PartialEq, Eq)]
    struct Entry {
        f: u32,
        pos: (i32, i32),
    }
    impl Ord for Entry {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            other.f.cmp(&self.f) // min-heap
        }
    }
    impl PartialOrd for Entry {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    let h = |p: (i32, i32)| p.0.abs_diff(to.0) + p.1.abs_diff(to.1);
    let mut open = BinaryHeap::new();
    let mut g: BTreeMap<(i32, i32), u32> = BTreeMap::new();
    let mut came: BTreeMap<(i32, i32), (i32, i32)> = BTreeMap::new();
    open.push(Entry { f: h(from), pos: from });
    g.insert(from, 0);

    let mut expanded = 0usize;
    while let Some(Entry { pos, .. }) = open.pop() {
        expanded += 1;
        if expanded > config.max_expanded {
            return Vec::new();
        }
        if pos == to {
            let mut path = vec![to];
            let mut cur = to;
            while let Some(&prev) = came.get(&cur) {
                path.push(prev);
                cur = prev;
            }
            path.reverse();
            return path;
        }
        let g_here = g[&pos];
        for (dx, dz) in [(0, 1), (0, -1), (1, 0), (-1, 0)] {
            let next = (pos.0 + dx, pos.1 + dz);
            let Some(step) = provider.cost(next.0, next.1) else { continue };
            let tentative = g_here.saturating_add(step);
            if tentative < *g.get(&next).unwrap_or(&u32::MAX) {
                g.insert(next, tentative);
                came.insert(next, pos);
                open.push(Entry { f: tentative + h(next), pos: next });
            }
        }
    }
    Vec::new()
}

/// A route: from, then to.
type Route = ((i32, i32), (i32, i32));

/// A generation-keyed path cache: repeated queries between the same cells
/// reuse the stored path until the world changes (`invalidate()` on any
/// voxel/tower edit bumps the generation and lazily discards entries).
#[derive(Default)]
pub struct PathCache {
    generation: u64,
    entries: std::collections::BTreeMap<Route, (u64, Vec<(i32, i32)>)>,
}

impl PathCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every cached path (world changed).
    pub fn invalidate(&mut self) {
        self.generation += 1;
        // Lazy: stale entries are ignored on read and overwritten on write;
        // clear outright when the map has grown past a sane bound.
        if self.entries.len() > 1024 {
            self.entries.clear();
        }
    }

    /// Cached path or compute-and-store via [`a_star`].
    pub fn get(&mut self, provider: &impl CostProvider, from: (i32, i32), to: (i32, i32), config: &PathConfig) -> Vec<(i32, i32)> {
        if let Some((generation, path)) = self.entries.get(&(from, to))
            && *generation == self.generation
        {
            return path.clone();
        }
        let path = a_star(provider, from, to, config);
        self.entries.insert((from, to), (self.generation, path.clone()));
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(blocked: &'a dyn Fn(i32, i32) -> bool) -> GridProvider<'a> {
        GridProvider { width: 32, height: 32, blocked }
    }

    #[test]
    fn straight_line() {
        let open = |_x: i32, _z: i32| false;
        let path = a_star(&field(&open), (0, 0), (5, 0), &PathConfig::default());
        assert_eq!(path.len(), 6);
        assert_eq!(path[0], (0, 0));
        assert_eq!(path[5], (5, 0));
    }

    #[test]
    fn routes_around_walls() {
        // Vertical wall at x=5 with a gap at z=30.
        let wall = |x: i32, z: i32| x == 5 && z != 30;
        let path = a_star(&field(&wall), (0, 0), (10, 0), &PathConfig::default());
        assert!(!path.is_empty());
        assert!(path.contains(&(5, 30)), "must route through the gap");
        // No step lands on the wall.
        assert!(path.iter().all(|&(x, z)| !(x == 5 && z != 30)));
        // Consecutive steps are 4-connected.
        for pair in path.windows(2) {
            let d = (pair[0].0 - pair[1].0).abs() + (pair[0].1 - pair[1].1).abs();
            assert_eq!(d, 1);
        }
    }

    #[test]
    fn unreachable_and_budget() {
        let sealed = |x: i32, _z: i32| x == 5; // full wall
        assert!(a_star(&field(&sealed), (0, 0), (10, 0), &PathConfig::default()).is_empty());
        let open = |_x: i32, _z: i32| false;
        let tiny = PathConfig { max_expanded: 3 };
        assert!(a_star(&field(&open), (0, 0), (20, 20), &tiny).is_empty());
    }

    #[test]
    fn blocked_endpoints_and_identity() {
        let wall = |x: i32, z: i32| x == 0 && z == 0;
        assert!(a_star(&field(&wall), (0, 0), (5, 5), &PathConfig::default()).is_empty());
        let open = |_x: i32, _z: i32| false;
        assert_eq!(a_star(&field(&open), (3, 3), (3, 3), &PathConfig::default()), vec![(3, 3)]);
    }

    #[test]
    fn cache_reuses_until_invalidated() {
        use std::cell::Cell;
        let calls = Cell::new(0u32);
        let counting = |_x: i32, _z: i32| {
            calls.set(calls.get() + 1);
            false
        };
        let provider = field(&counting);
        let mut cache = PathCache::new();
        let config = PathConfig::default();
        let first = cache.get(&provider, (0, 0), (5, 0), &config);
        let after_first = calls.get();
        let second = cache.get(&provider, (0, 0), (5, 0), &config);
        assert_eq!(first, second);
        assert_eq!(calls.get(), after_first, "cache hit does no provider work");
        cache.invalidate();
        cache.get(&provider, (0, 0), (5, 0), &config);
        assert!(calls.get() > after_first, "invalidation recomputes");
    }
}
