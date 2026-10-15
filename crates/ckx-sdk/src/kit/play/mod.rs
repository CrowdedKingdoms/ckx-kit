//! Gameplay rules a hub keeps: [`turns`] (initiative, timeouts, simultaneous reveal),
//! [`score`] (score books, win conditions, standings), [`cards`] (decks, hands held as the
//! hub's secret, public piles), [`combat`] (the hit referee: health, statuses, cooldowns,
//! respawns), [`abilities`] (casts with cooldowns and resources, projectiles, area damage),
//! [`movement`] (the warden for positions clients report) and [`timing`] (checkpoints, laps,
//! splits, ghosts). Each is plain data over `now_ms` and a seeded [`Rng`](super::Rng): keep it
//! in the hub's state, and it carries on after a snapshot.

pub mod abilities;
pub mod cards;
pub mod combat;
pub mod movement;
pub mod score;
pub mod timing;
pub mod turns;
