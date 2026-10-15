//! Rule zones: static axis-aligned boxes tagged with rule strings ("safe",
//! "pvp", "build-locked") plus **shrinking circles** — the
//! battle-royale zone: a phased schedule of (radius, hold, shrink) steps
//! with warning/shrink/settle events for the type-98 lane.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Zone {
    pub id: String,
    pub rule: String,
    pub min: (f32, f32, f32),
    pub max: (f32, f32, f32),
}

impl Zone {
    pub fn contains(&self, pos: (f32, f32, f32)) -> bool {
        pos.0 >= self.min.0
            && pos.0 <= self.max.0
            && pos.1 >= self.min.1
            && pos.1 <= self.max.1
            && pos.2 >= self.min.2
            && pos.2 <= self.max.2
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ZoneSet {
    pub zones: Vec<Zone>,
}

impl ZoneSet {
    /// All zones containing the position.
    pub fn at(&self, pos: (f32, f32, f32)) -> Vec<&Zone> {
        self.zones.iter().filter(|z| z.contains(pos)).collect()
    }

    /// Does any zone with this rule cover the position?
    pub fn has_rule(&self, pos: (f32, f32, f32), rule: &str) -> bool {
        self.zones.iter().any(|z| z.rule == rule && z.contains(pos))
    }
}

// ---------------------------------------------------------------------------
// Shrinking circles (BR zones)
// ---------------------------------------------------------------------------

/// One phase of a shrinking-zone schedule.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ShrinkPhase {
    /// The radius this phase shrinks TO.
    pub radius: f32,
    /// How long the zone HOLDS before shrinking (the warning window).
    pub hold_ms: u64,
    /// How long the shrink itself takes (radius interpolates linearly).
    pub shrink_ms: u64,
}

/// Announcements a shrinking zone produces as time advances.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ZoneEvent {
    /// A hold began: the zone will start shrinking to `to_radius` at `at_ms`.
    Warning { phase: usize, to_radius: f32, shrink_starts_at_ms: u64 },
    /// The shrink began.
    Shrinking { phase: usize, to_radius: f32 },
    /// The phase's target radius was reached.
    Settled { phase: usize, radius: f32 },
    /// Every phase is done (the final circle).
    Final { radius: f32 },
}

/// A battle-royale circle: fixed center, radius stepping down a schedule.
/// Pure clock math — feed it `now_ms`, read `radius()` and drain events.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ShrinkingZone {
    pub center: (f32, f32),
    pub initial_radius: f32,
    pub phases: Vec<ShrinkPhase>,
    pub started_at_ms: u64,
    /// Last phase-progress marker events were emitted for (phase, stage).
    announced: Vec<(usize, u8)>,
}

impl ShrinkingZone {
    pub fn start(center: (f32, f32), initial_radius: f32, phases: Vec<ShrinkPhase>, now_ms: u64) -> Self {
        Self { center, initial_radius, phases, started_at_ms: now_ms, announced: Vec::new() }
    }

    /// Phase timeline offsets: each phase = hold then shrink.
    fn phase_bounds(&self, phase: usize) -> (u64, u64, u64) {
        let mut offset = 0u64;
        for p in &self.phases[..phase] {
            offset += p.hold_ms + p.shrink_ms;
        }
        let p = &self.phases[phase];
        (offset, offset + p.hold_ms, offset + p.hold_ms + p.shrink_ms)
    }

    /// The circle's radius at `now_ms` (linear interpolation during shrinks).
    pub fn radius(&self, now_ms: u64) -> f32 {
        let elapsed = now_ms.saturating_sub(self.started_at_ms);
        let mut from = self.initial_radius;
        for (index, phase) in self.phases.iter().enumerate() {
            let (_, shrink_start, shrink_end) = self.phase_bounds(index);
            if elapsed < shrink_start {
                return from;
            }
            if elapsed < shrink_end {
                let t = (elapsed - shrink_start) as f32 / (shrink_end - shrink_start).max(1) as f32;
                return from + (phase.radius - from) * t;
            }
            from = phase.radius;
        }
        from
    }

    /// Is a position inside the live circle?
    pub fn contains(&self, pos: (f32, f32), now_ms: u64) -> bool {
        let (dx, dz) = (pos.0 - self.center.0, pos.1 - self.center.1);
        (dx * dx + dz * dz).sqrt() <= self.radius(now_ms)
    }

    /// Every phase fully settled?
    pub fn done(&self, now_ms: u64) -> bool {
        match self.phases.len() {
            0 => true,
            n => now_ms.saturating_sub(self.started_at_ms) >= self.phase_bounds(n - 1).2,
        }
    }

    /// Advance the announcer: returns the events newly due at `now_ms`
    /// (warning at hold start, shrinking at shrink start, settled at end,
    /// final once after the last phase).
    pub fn poll(&mut self, now_ms: u64) -> Vec<ZoneEvent> {
        let elapsed = now_ms.saturating_sub(self.started_at_ms);
        let mut events = Vec::new();
        for (index, phase) in self.phases.iter().enumerate() {
            let (hold_start, shrink_start, shrink_end) = self.phase_bounds(index);
            let stages: [(u8, u64); 3] = [(0, hold_start), (1, shrink_start), (2, shrink_end)];
            for (stage, at) in stages {
                if elapsed >= at && !self.announced.contains(&(index, stage)) {
                    self.announced.push((index, stage));
                    events.push(match stage {
                        0 => ZoneEvent::Warning {
                            phase: index,
                            to_radius: phase.radius,
                            shrink_starts_at_ms: self.started_at_ms + shrink_start,
                        },
                        1 => ZoneEvent::Shrinking { phase: index, to_radius: phase.radius },
                        _ => ZoneEvent::Settled { phase: index, radius: phase.radius },
                    });
                }
            }
        }
        if self.done(now_ms) && !self.announced.contains(&(usize::MAX, 3)) {
            self.announced.push((usize::MAX, 3));
            events.push(ZoneEvent::Final { radius: self.radius(now_ms) });
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinking_zone_interpolates_and_announces() {
        let mut zone = ShrinkingZone::start(
            (0.0, 0.0),
            100.0,
            vec![
                ShrinkPhase { radius: 50.0, hold_ms: 10_000, shrink_ms: 10_000 },
                ShrinkPhase { radius: 10.0, hold_ms: 5_000, shrink_ms: 5_000 },
            ],
            1_000,
        );
        // Hold: full radius; the warning fires immediately.
        let events = zone.poll(1_000);
        assert!(matches!(events[0], ZoneEvent::Warning { phase: 0, to_radius, .. } if to_radius == 50.0));
        assert_eq!(zone.radius(5_000), 100.0);
        // Mid-shrink: halfway between 100 and 50.
        let events = zone.poll(16_000);
        assert!(events.iter().any(|e| matches!(e, ZoneEvent::Shrinking { phase: 0, .. })));
        assert!((zone.radius(16_000) - 75.0).abs() < 0.01);
        // Settled at 50, phase 1 warning issued in the same poll window.
        let events = zone.poll(21_500);
        assert!(events.iter().any(|e| matches!(e, ZoneEvent::Settled { phase: 0, radius } if *radius == 50.0)));
        assert!(events.iter().any(|e| matches!(e, ZoneEvent::Warning { phase: 1, .. })));
        assert_eq!(zone.radius(24_000), 50.0);
        // Final circle.
        let events = zone.poll(31_000);
        assert!(events.iter().any(|e| matches!(e, ZoneEvent::Settled { phase: 1, radius } if *radius == 10.0)));
        assert!(events.iter().any(|e| matches!(e, ZoneEvent::Final { radius } if *radius == 10.0)));
        assert!(zone.done(31_000));
        assert!(zone.poll(40_000).is_empty(), "announce once");
        // Containment tracks the live radius.
        assert!(zone.contains((60.0, 0.0), 5_000));
        assert!(!zone.contains((60.0, 0.0), 31_000));
    }

    #[test]
    fn containment_and_rules() {
        let set = ZoneSet {
            zones: vec![
                Zone { id: "spawn".into(), rule: "safe".into(), min: (-10.0, 0.0, -10.0), max: (10.0, 32.0, 10.0) },
                Zone { id: "arena".into(), rule: "pvp".into(), min: (5.0, 0.0, 5.0), max: (30.0, 32.0, 30.0) },
            ],
        };
        assert!(set.has_rule((0.0, 16.0, 0.0), "safe"));
        assert!(!set.has_rule((0.0, 16.0, 0.0), "pvp"));
        // Overlap region carries both rules.
        assert_eq!(set.at((7.0, 16.0, 7.0)).len(), 2);
        // Boundaries are inclusive; outside is outside.
        assert!(set.has_rule((10.0, 32.0, 10.0), "safe"));
        assert!(set.at((50.0, 16.0, 50.0)).is_empty());
    }
}
