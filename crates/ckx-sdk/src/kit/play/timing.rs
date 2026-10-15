//! Race timing: ordered checkpoint/gate evaluation from pose streams,
//! lap/split timing, best-lap books, and compact ghost recording (pose
//! track capture + interpolated replay). Pure state machines fed by the
//! race hub's tick samples — gates are proximity circles, so tick-rate
//! sampling with generous radii is the tolerance model (documented for
//! course designers).

use serde::{Deserialize, Serialize};

/// One ordered gate on a course.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Gate {
    pub x: f32,
    pub z: f32,
    pub radius: f32,
}

/// A course: ordered gates; a lap = passing every gate in order (gate 0 is
/// the start/finish line).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Course {
    pub gates: Vec<Gate>,
    pub laps: u32,
}

/// Events produced by feeding poses through a run timer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum TimingEvent {
    Started { at_ms: u64 },
    Checkpoint { gate: usize, split_ms: u64 },
    LapCompleted { lap: u32, lap_ms: u64, best: bool },
    Finished { total_ms: u64, best_lap_ms: u64 },
}

/// One racer's live run state.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RunTimer {
    pub next_gate: usize,
    pub lap: u32,
    pub started_at_ms: u64,
    pub lap_started_at_ms: u64,
    pub splits_ms: Vec<u64>,
    pub best_lap_ms: u64,
    pub finished: bool,
    pub total_ms: u64,
}

impl RunTimer {
    /// Feed one pose sample; returns the timing events it produced. The run
    /// starts when the racer first touches gate 0.
    pub fn feed(&mut self, course: &Course, pos: (f32, f32), now_ms: u64) -> Vec<TimingEvent> {
        if self.finished || course.gates.is_empty() {
            return Vec::new();
        }
        let mut events = Vec::new();
        let gate = course.gates[self.next_gate];
        let (dx, dz) = (pos.0 - gate.x, pos.1 - gate.z);
        if (dx * dx + dz * dz).sqrt() > gate.radius {
            return events;
        }

        if self.started_at_ms == 0 {
            // Touching the start line arms the run.
            if self.next_gate == 0 {
                self.started_at_ms = now_ms;
                self.lap_started_at_ms = now_ms;
                self.lap = 1;
                self.next_gate = 1 % course.gates.len();
                events.push(TimingEvent::Started { at_ms: now_ms });
            }
            return events;
        }

        let split = now_ms.saturating_sub(self.lap_started_at_ms);
        self.splits_ms.push(split);
        events.push(TimingEvent::Checkpoint { gate: self.next_gate, split_ms: split });
        self.next_gate = (self.next_gate + 1) % course.gates.len();

        // Wrapped back to the start line: lap complete.
        if self.next_gate == 1 % course.gates.len() && self.splits_ms.len().is_multiple_of(course.gates.len()) {
            let lap_ms = now_ms.saturating_sub(self.lap_started_at_ms);
            let best = self.best_lap_ms == 0 || lap_ms < self.best_lap_ms;
            if best {
                self.best_lap_ms = lap_ms;
            }
            events.push(TimingEvent::LapCompleted { lap: self.lap, lap_ms, best });
            if self.lap >= course.laps.max(1) {
                self.finished = true;
                self.total_ms = now_ms.saturating_sub(self.started_at_ms);
                events.push(TimingEvent::Finished { total_ms: self.total_ms, best_lap_ms: self.best_lap_ms });
            } else {
                self.lap += 1;
                self.lap_started_at_ms = now_ms;
            }
        }
        events
    }
}

// ---------------------------------------------------------------------------
// Ghosts
// ---------------------------------------------------------------------------

/// A compact recorded pose track: (offset ms, x, y, z, yaw). Sampled at the
/// recorder's cadence, replayed with linear interpolation.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct GhostTrack {
    pub frames: Vec<(u32, f32, f32, f32, f32)>,
    pub started_at_ms: u64,
}

/// Ghost tracks are capped so a long run cannot blow the state budget.
pub const GHOST_MAX_FRAMES: usize = 600;

impl GhostTrack {
    /// Record one frame (call per sample while the run is live).
    pub fn record(&mut self, now_ms: u64, x: f32, y: f32, z: f32, yaw: f32) {
        if self.started_at_ms == 0 {
            self.started_at_ms = now_ms;
        }
        if self.frames.len() >= GHOST_MAX_FRAMES {
            return;
        }
        let offset = now_ms.saturating_sub(self.started_at_ms) as u32;
        self.frames.push((offset, x, y, z, yaw));
    }

    pub fn duration_ms(&self) -> u32 {
        self.frames.last().map(|f| f.0).unwrap_or(0)
    }

    /// Interpolated pose at `offset_ms` into the replay (clamped to ends).
    pub fn frame_at(&self, offset_ms: u32) -> Option<(f32, f32, f32, f32)> {
        if self.frames.is_empty() {
            return None;
        }
        let last = *self.frames.last().unwrap();
        if offset_ms >= last.0 {
            return Some((last.1, last.2, last.3, last.4));
        }
        let after_index = self.frames.iter().position(|f| f.0 >= offset_ms)?;
        if after_index == 0 {
            let f = self.frames[0];
            return Some((f.1, f.2, f.3, f.4));
        }
        let before = self.frames[after_index - 1];
        let after = self.frames[after_index];
        let span = (after.0 - before.0).max(1) as f32;
        let t = (offset_ms - before.0) as f32 / span;
        Some((
            before.1 + (after.1 - before.1) * t,
            before.2 + (after.2 - before.2) * t,
            before.3 + (after.3 - before.3) * t,
            before.4 + (after.4 - before.4) * t,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle() -> Course {
        Course {
            gates: vec![Gate { x: 0.0, z: 0.0, radius: 2.0 }, Gate { x: 10.0, z: 0.0, radius: 2.0 }, Gate { x: 5.0, z: 8.0, radius: 2.0 }],
            laps: 2,
        }
    }

    #[test]
    fn ordered_gates_laps_and_finish() {
        let course = triangle();
        let mut timer = RunTimer::default();
        // Touching gate 1 before starting does nothing.
        assert!(timer.feed(&course, (10.0, 0.0), 500).is_empty());
        // Start at gate 0.
        let events = timer.feed(&course, (0.5, 0.0), 1_000);
        assert_eq!(events, vec![TimingEvent::Started { at_ms: 1_000 }]);
        // Skipping a gate is ignored (gate 2 while gate 1 is next).
        assert!(timer.feed(&course, (5.0, 8.0), 2_000).is_empty());
        // Gate 1 -> checkpoint split.
        let events = timer.feed(&course, (10.0, 0.5), 3_000);
        assert_eq!(events, vec![TimingEvent::Checkpoint { gate: 1, split_ms: 2_000 }]);
        timer.feed(&course, (5.0, 8.0), 5_000); // gate 2
        // Back to start: lap 1 done (best).
        let events = timer.feed(&course, (0.0, 0.5), 7_000);
        assert!(events.contains(&TimingEvent::LapCompleted { lap: 1, lap_ms: 6_000, best: true }));
        assert_eq!(timer.lap, 2);
        // Lap 2, faster: finish with best lap = lap 2.
        timer.feed(&course, (10.0, 0.0), 8_000);
        timer.feed(&course, (5.0, 8.0), 9_000);
        let events = timer.feed(&course, (0.0, 0.0), 11_000);
        assert!(events.contains(&TimingEvent::LapCompleted { lap: 2, lap_ms: 4_000, best: true }));
        assert!(events.contains(&TimingEvent::Finished { total_ms: 10_000, best_lap_ms: 4_000 }));
        assert!(timer.finished);
        assert!(timer.feed(&course, (10.0, 0.0), 12_000).is_empty(), "finished runs ignore input");
    }

    #[test]
    fn ghost_records_and_interpolates() {
        let mut ghost = GhostTrack::default();
        ghost.record(1_000, 0.0, 0.0, 0.0, 0.0);
        ghost.record(2_000, 10.0, 0.0, 0.0, 90.0);
        ghost.record(3_000, 10.0, 0.0, 10.0, 180.0);
        assert_eq!(ghost.duration_ms(), 2_000);
        let mid = ghost.frame_at(500).unwrap();
        assert!((mid.0 - 5.0).abs() < 1e-3);
        assert!((mid.3 - 45.0).abs() < 1e-3);
        let end = ghost.frame_at(9_999).unwrap();
        assert_eq!(end.2, 10.0);
        // Frame cap holds.
        let mut big = GhostTrack::default();
        for i in 0..(GHOST_MAX_FRAMES + 50) {
            big.record(i as u64 * 100, i as f32, 0.0, 0.0, 0.0);
        }
        assert_eq!(big.frames.len(), GHOST_MAX_FRAMES);
    }
}
