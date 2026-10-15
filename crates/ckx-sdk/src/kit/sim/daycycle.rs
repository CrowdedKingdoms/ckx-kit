//! The deterministic day/night clock: a pure function of wall-clock ms, so
//! hubs and clients agree without any synchronization message.
//! A 20-minute day is the
//! default; games pick their own length.

use serde::{Deserialize, Serialize};

pub const DEFAULT_DAY_LENGTH_MS: u64 = 1_200_000; // 20 minutes

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct DayCycle {
    pub day_length_ms: u64,
}

impl Default for DayCycle {
    fn default() -> Self {
        Self { day_length_ms: DEFAULT_DAY_LENGTH_MS }
    }
}

impl DayCycle {
    pub fn new(day_length_ms: u64) -> Self {
        Self { day_length_ms: day_length_ms.max(1) }
    }

    /// Phase in [0, 1): 0 = day start, 0.5 = day/night boundary region.
    pub fn phase(&self, now_ms: u64) -> f32 {
        (now_ms % self.day_length_ms) as f32 / self.day_length_ms as f32
    }

    /// Night when sin(phase*TAU) dips below -0.05.
    pub fn is_night(&self, now_ms: u64) -> bool {
        (self.phase(now_ms) * std::f32::consts::TAU).sin() < -0.05
    }

    /// Sun height in [-1, 1] (drive lighting / spawn weights).
    pub fn sun(&self, now_ms: u64) -> f32 {
        (self.phase(now_ms) * std::f32::consts::TAU).sin()
    }

    /// Day number since epoch (season math, daily resets).
    pub fn day_index(&self, now_ms: u64) -> u64 {
        now_ms / self.day_length_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_twenty_minute_day_turns_to_night_and_back() {
        let cycle = DayCycle::default();
        // First half of the cycle: sun up (sin > 0).
        assert!(!cycle.is_night(60_000));
        assert!(cycle.sun(300_000) > 0.99); // quarter cycle = noon
        // Second half: night.
        assert!(cycle.is_night(900_000)); // three-quarter = midnight
        assert_eq!(cycle.day_index(2_500_000), 2);
    }

    #[test]
    fn deterministic_across_instances() {
        let a = DayCycle::new(600_000);
        let b = DayCycle::new(600_000);
        for t in (0..3_000_000).step_by(90_000) {
            assert_eq!(a.is_night(t), b.is_night(t));
            assert_eq!(a.phase(t), b.phase(t));
        }
    }
}
