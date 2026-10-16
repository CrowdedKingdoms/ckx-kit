//! Wave schedules: primitives for timed spawn waves (tower defense, horde
//! nights): the scheduling core a director hub drives.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WaveDef {
    /// Entries: (unit id, count).
    pub units: Vec<(String, u32)>,
    /// Delay after the previous wave clears (or starts, for timed mode).
    pub delay_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum WavePhase {
    /// Before wave `next` starts; starts at the embedded time.
    Countdown { next: usize, starts_at_ms: u64 },
    /// Wave `index` is live.
    Active { index: usize },
    /// All waves done.
    Complete,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WaveSchedule {
    pub phase: WavePhase,
}

impl Default for WaveSchedule {
    fn default() -> Self {
        Self { phase: WavePhase::Complete }
    }
}

pub enum WaveEvent {
    /// Spawn this wave's units now.
    Start(usize),
    Finished,
}

impl WaveSchedule {
    /// Arm the schedule from wave 0 with the first delay.
    pub fn start(waves: &[WaveDef], now_ms: u64) -> Self {
        match waves.first() {
            Some(first) => Self { phase: WavePhase::Countdown { next: 0, starts_at_ms: now_ms + first.delay_ms } },
            None => Self { phase: WavePhase::Complete },
        }
    }

    /// Advance. `active_cleared` = the live wave's units are all dead.
    pub fn step(&mut self, waves: &[WaveDef], now_ms: u64, active_cleared: bool) -> Option<WaveEvent> {
        match self.phase.clone() {
            WavePhase::Countdown { next, starts_at_ms } if now_ms >= starts_at_ms => {
                self.phase = WavePhase::Active { index: next };
                Some(WaveEvent::Start(next))
            }
            WavePhase::Active { index } if active_cleared => {
                let next = index + 1;
                match waves.get(next) {
                    Some(wave) => {
                        self.phase = WavePhase::Countdown { next, starts_at_ms: now_ms + wave.delay_ms };
                        None
                    }
                    None => {
                        self.phase = WavePhase::Complete;
                        Some(WaveEvent::Finished)
                    }
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_waves() -> Vec<WaveDef> {
        vec![
            WaveDef { units: vec![("slime".into(), 3)], delay_ms: 5_000 },
            WaveDef { units: vec![("skeleton".into(), 2)], delay_ms: 10_000 },
        ]
    }

    #[test]
    fn full_run() {
        let waves = two_waves();
        let mut schedule = WaveSchedule::start(&waves, 0);
        assert!(schedule.step(&waves, 4_999, false).is_none());
        assert!(matches!(schedule.step(&waves, 5_000, false), Some(WaveEvent::Start(0))));
        // Wave 0 live until cleared.
        assert!(schedule.step(&waves, 8_000, false).is_none());
        assert!(schedule.step(&waves, 9_000, true).is_none()); // countdown to wave 1
        assert!(matches!(schedule.step(&waves, 19_000, false), Some(WaveEvent::Start(1))));
        assert!(matches!(schedule.step(&waves, 25_000, true), Some(WaveEvent::Finished)));
        assert_eq!(schedule.phase, WavePhase::Complete);
    }

    #[test]
    fn empty_schedule_completes() {
        let schedule = WaveSchedule::start(&[], 0);
        assert_eq!(schedule.phase, WavePhase::Complete);
    }
}
