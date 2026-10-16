//! World simulation a hub runs: [`daycycle`] (a clock everyone agrees on from the time alone),
//! [`weather`] (seeded fronts), [`nodes`] (resource nodes that deplete and regrow), [`waves`]
//! (timed spawn waves) and [`zones`] (rule boxes and shrinking circles). Deterministic over
//! `now_ms` and a seeded [`Rng`](super::Rng).

pub mod daycycle;
pub mod nodes;
pub mod waves;
pub mod weather;
pub mod zones;
