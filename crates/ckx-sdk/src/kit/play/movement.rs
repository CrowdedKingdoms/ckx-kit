//! The movement warden: checks the positions a player's client reports (how far since the last
//! one, how fast, inside the bounds) and says which position the hub keeps. A hub that takes
//! positions from clients (a race, a ball game, an arena) runs every report through it; one that
//! reads where the platform sees players ([`super::super::presence`]) need not.
//!
//! A report too fast is cut to the farthest the player could have gone; a jump past `teleport`
//! is refused (the player stays put); one outside the bounds is pulled back in. Each is a
//! [`Violation`] the hub may publish (the realtime registry's 95) and counts as a strike;
//! strikes clear after [`STRIKE_WINDOW_MS`] without one, so a hub can act on a run of them
//! (drop the player, report them) and forgive a lag spike.

use serde::{Deserialize, Serialize};

/// How long strikes are remembered after the last one.
pub const STRIKE_WINDOW_MS: u64 = 10_000;

/// What a player's movement may do, in the hub's own units.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct Rules {
    /// Units a second.
    pub max_speed: f64,
    /// A move this far or more at once is refused.
    pub teleport: f64,
    /// `(min_x, min_z, max_x, max_z)`, when there are any.
    pub bounds: Option<(f64, f64, f64, f64)>,
    /// Slack on `max_speed` for jitter in the reports' timing (1.25 lets a report be 25% fast).
    pub grace: f64,
}

impl Default for Rules {
    fn default() -> Self {
        Self { max_speed: 8.0, teleport: 30.0, bounds: None, grace: 1.25 }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    Speed,
    Teleport,
    Bounds,
}

impl ViolationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Speed => "speed",
            Self::Teleport => "teleport",
            Self::Bounds => "bounds",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Violation {
    pub kind: ViolationKind,
    pub detail: String,
}

/// The position the hub keeps for a report, and what was wrong with it.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub at: (f64, f64),
    pub violation: Option<Violation>,
}

/// One player's movement as the hub last accepted it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Warden {
    /// The position kept, and when.
    pub last: Option<(f64, f64, u64)>,
    pub strikes: u32,
    pub last_strike_ms: u64,
}

fn clamp_to(bounds: Option<(f64, f64, f64, f64)>, p: (f64, f64)) -> (f64, f64) {
    match bounds {
        Some((x0, z0, x1, z1)) => (p.0.clamp(x0.min(x1), x0.max(x1)), p.1.clamp(z0.min(z1), z0.max(z1))),
        None => p,
    }
}

impl Warden {
    /// A warden that starts from `at`: a spawn, a respawn, a place the hub moved the player to.
    pub fn at(at: (f64, f64), now: u64) -> Self {
        Self { last: Some((at.0, at.1, now)), ..Self::default() }
    }

    /// Moves the player where the hub says, with no check.
    pub fn place(&mut self, at: (f64, f64), now: u64) {
        self.last = Some((at.0, at.1, now));
    }

    /// The position kept.
    pub fn position(&self) -> Option<(f64, f64)> {
        self.last.map(|(x, z, _)| (x, z))
    }

    /// Strikes in the current window.
    pub fn strikes(&self, now: u64) -> u32 {
        if now.saturating_sub(self.last_strike_ms) >= STRIKE_WINDOW_MS { 0 } else { self.strikes }
    }

    /// Checks a reported position and keeps the one the rules allow.
    pub fn check(&mut self, rules: &Rules, claimed: (f64, f64), now: u64) -> Verdict {
        let mut violation = None;
        let mut at = claimed;
        if !at.0.is_finite() || !at.1.is_finite() {
            let kept = self.position().unwrap_or((0.0, 0.0));
            return self.strike(now, kept, ViolationKind::Teleport, "a position that is not a number".into());
        }
        let inside = clamp_to(rules.bounds, at);
        if inside != at {
            violation =
                Some(Violation { kind: ViolationKind::Bounds, detail: format!("({:.1}, {:.1}) is outside the bounds", at.0, at.1) });
            at = inside;
        }
        if let Some((x, z, t)) = self.last {
            let (dx, dz) = (at.0 - x, at.1 - z);
            let dist = (dx * dx + dz * dz).sqrt();
            let dt = now.saturating_sub(t) as f64 / 1000.0;
            let allowed = rules.max_speed * dt * rules.grace.max(1.0) + 0.5;
            if dist >= rules.teleport {
                return self.strike(now, (x, z), ViolationKind::Teleport, format!("{dist:.1} units at once"));
            }
            if dist > allowed {
                let k = allowed / dist;
                at = (x + dx * k, z + dz * k);
                violation = Some(Violation { kind: ViolationKind::Speed, detail: format!("{:.1} units a second", dist / dt.max(0.001)) });
            }
        }
        self.last = Some((at.0, at.1, now));
        match violation {
            Some(v) => self.strike(now, at, v.kind, v.detail),
            None => Verdict { at, violation: None },
        }
    }

    fn strike(&mut self, now: u64, at: (f64, f64), kind: ViolationKind, detail: String) -> Verdict {
        self.strikes = self.strikes(now) + 1;
        self.last_strike_ms = now;
        if kind == ViolationKind::Teleport {
            let t = self.last.map(|l| l.2).unwrap_or(now);
            self.last = Some((at.0, at.1, t));
        }
        Verdict { at, violation: Some(Violation { kind, detail }) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        Rules { max_speed: 10.0, teleport: 25.0, bounds: Some((0.0, 0.0, 100.0, 100.0)), grace: 1.0 }
    }

    #[test]
    fn a_fair_move_is_kept() {
        let mut w = Warden::at((10.0, 10.0), 0);
        let v = w.check(&rules(), (14.0, 10.0), 500);
        assert_eq!(v, Verdict { at: (14.0, 10.0), violation: None });
        assert_eq!(w.strikes(500), 0);
    }

    #[test]
    fn a_fast_move_is_cut_to_what_the_speed_allows() {
        let mut w = Warden::at((10.0, 10.0), 0);
        let v = w.check(&rules(), (20.0, 10.0), 500);
        assert_eq!(v.violation.map(|v| v.kind), Some(ViolationKind::Speed));
        assert!((v.at.0 - 15.5).abs() < 1e-9 && v.at.1 == 10.0, "{:?}", v.at);
        assert_eq!(w.position(), Some(v.at));
    }

    #[test]
    fn a_teleport_is_refused_and_the_player_stays_put() {
        let mut w = Warden::at((10.0, 10.0), 0);
        let v = w.check(&rules(), (60.0, 10.0), 10_000);
        assert_eq!((v.at, v.violation.map(|v| v.kind)), ((10.0, 10.0), Some(ViolationKind::Teleport)));
        let v = w.check(&rules(), (f64::NAN, 1.0), 10_100);
        assert_eq!((v.at, v.violation.map(|v| v.kind)), ((10.0, 10.0), Some(ViolationKind::Teleport)));
        assert_eq!(w.strikes(10_100), 2);
    }

    #[test]
    fn the_bounds_pull_a_move_back_in_and_strikes_clear_after_a_quiet_window() {
        let mut w = Warden::at((99.0, 50.0), 0);
        let v = w.check(&rules(), (101.0, 50.0), 1_000);
        assert_eq!((v.at, v.violation.map(|v| v.kind)), ((100.0, 50.0), Some(ViolationKind::Bounds)));
        assert_eq!(w.strikes(1_000), 1);
        assert_eq!(w.strikes(1_000 + STRIKE_WINDOW_MS), 0);
        w.place((5.0, 5.0), 20_000);
        assert_eq!(w.check(&rules(), (6.0, 5.0), 20_200).violation, None);
    }
}
