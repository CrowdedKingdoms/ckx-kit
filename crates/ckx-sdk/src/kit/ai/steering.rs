//! Steering behaviors: pure functions from positions to a bounded step.
//! A hub calls one per agent per tick and add the result to the pose.

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec2 {
    pub x: f32,
    pub z: f32,
}

impl Vec2 {
    pub fn new(x: f32, z: f32) -> Self {
        Self { x, z }
    }

    pub fn len(&self) -> f32 {
        (self.x * self.x + self.z * self.z).sqrt()
    }

    pub fn to(self, other: Vec2) -> Vec2 {
        Vec2::new(other.x - self.x, other.z - self.z)
    }

    pub fn dist(&self, other: Vec2) -> f32 {
        self.to(other).len()
    }

    /// Clamp this displacement to at most `max` length.
    pub fn clamped(self, max: f32) -> Vec2 {
        let len = self.len();
        if len <= max || len == 0.0 { self } else { Vec2::new(self.x / len * max, self.z / len * max) }
    }

    pub fn yaw(&self) -> f32 {
        self.x.atan2(self.z)
    }
}

/// Step toward `target` at up to `speed*dt`, stopping within `stop_at`.
pub fn seek(pos: Vec2, target: Vec2, speed: f32, dt: f32, stop_at: f32) -> Vec2 {
    let delta = pos.to(target);
    if delta.len() <= stop_at {
        return Vec2::default();
    }
    delta.clamped(speed * dt)
}

/// Step directly away from `threat`.
pub fn flee(pos: Vec2, threat: Vec2, speed: f32, dt: f32) -> Vec2 {
    let away = threat.to(pos);
    if away.len() == 0.0 {
        return Vec2::new(speed * dt, 0.0);
    }
    away.clamped(speed * dt)
}

/// Seek that decelerates inside `slow_radius` (smooth stops).
pub fn arrive(pos: Vec2, target: Vec2, speed: f32, dt: f32, slow_radius: f32) -> Vec2 {
    let delta = pos.to(target);
    let dist = delta.len();
    if dist < 0.05 {
        return Vec2::default();
    }
    let eased = if dist < slow_radius { speed * (dist / slow_radius) } else { speed };
    delta.clamped(eased.max(0.2) * dt)
}

/// Push away from any neighbor closer than `min_dist` (crowd separation).
pub fn separate(pos: Vec2, neighbors: &[Vec2], min_dist: f32, speed: f32, dt: f32) -> Vec2 {
    let mut push = Vec2::default();
    for n in neighbors {
        let away = n.to(pos);
        let d = away.len();
        if d > 0.0 && d < min_dist {
            let w = (min_dist - d) / min_dist;
            push.x += away.x / d * w;
            push.z += away.z / d * w;
        }
    }
    push.clamped(speed * dt)
}

/// Return-home pull when beyond `leash_radius`; `None` inside the leash.
pub fn leash(pos: Vec2, home: Vec2, leash_radius: f32, speed: f32, dt: f32) -> Option<Vec2> {
    (pos.dist(home) > leash_radius).then(|| seek(pos, home, speed, dt, 0.5))
}

/// A wander target picker: a point within `range` of `anchor`, clamped to
/// `bounds` (half-extent box around origin). Pure — feed it your RNG rolls.
pub fn wander_target(anchor: Vec2, range: f32, roll_x: f32, roll_z: f32, bounds: f32) -> Vec2 {
    Vec2::new(
        (anchor.x + (roll_x - 0.5) * 2.0 * range).clamp(-bounds, bounds),
        (anchor.z + (roll_z - 0.5) * 2.0 * range).clamp(-bounds, bounds),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_stops_and_clamps() {
        let step = seek(Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0), 2.0, 0.25, 1.0);
        assert!((step.x - 0.5).abs() < 1e-6 && step.z == 0.0);
        assert_eq!(seek(Vec2::new(9.5, 0.0), Vec2::new(10.0, 0.0), 2.0, 0.25, 1.0), Vec2::default());
    }

    #[test]
    fn flee_moves_away() {
        let step = flee(Vec2::new(1.0, 1.0), Vec2::new(0.0, 0.0), 4.0, 0.25);
        assert!(step.x > 0.0 && step.z > 0.0);
        assert!((step.len() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn arrive_decelerates() {
        let far = arrive(Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0), 4.0, 1.0, 5.0);
        let near = arrive(Vec2::new(96.0, 0.0), Vec2::new(100.0, 0.0), 4.0, 1.0, 5.0);
        assert!(near.len() < far.len());
    }

    #[test]
    fn separation_and_leash() {
        let push = separate(Vec2::new(0.0, 0.0), &[Vec2::new(0.5, 0.0)], 2.0, 4.0, 0.25);
        assert!(push.x < 0.0); // pushed away from the neighbor
        assert!(leash(Vec2::new(3.0, 0.0), Vec2::default(), 10.0, 2.0, 0.25).is_none());
        let pull = leash(Vec2::new(30.0, 0.0), Vec2::default(), 10.0, 2.0, 0.25).unwrap();
        assert!(pull.x < 0.0);
    }

    #[test]
    fn wander_stays_in_bounds() {
        for roll in [0.0f32, 0.5, 1.0] {
            let t = wander_target(Vec2::new(39.0, -39.0), 12.0, roll, roll, 40.0);
            assert!(t.x.abs() <= 40.0 && t.z.abs() <= 40.0);
        }
    }
}
