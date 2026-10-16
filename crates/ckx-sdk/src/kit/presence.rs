//! Where players are, for hubs that referee space (an arena, a race, a ball, a capture point, a
//! shrinking zone): the actors near a place, read with the node API (`world.read`), each
//! player's position as precise as their actor's state allows.
//!
//! Positions are in chunks, fractional. An actor whose state is an [`ActorState`] map or the
//! 48-byte engine pose (`f32` x, y, z in blocks first, 16 blocks a chunk) is placed exactly;
//! any other is placed at its chunk's centre, so a game that wants the hub to see more than
//! which chunk a player is in replicates its players with one of those two.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::wire::ActorState;
use crate::node::{Actor, ChunkPos};
use crate::{CallError, Ctx, decode};

/// Blocks in a chunk's edge, for the engine pose's world coordinates.
pub const CHUNK_BLOCKS: f64 = 16.0;
/// The engine pose's length.
pub const POSE_BYTES: usize = 48;

/// One player near the place, as last seen.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Seen {
    pub player: u64,
    pub uuid: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// Placed from the actor's state, not its chunk's centre.
    pub precise: bool,
}

/// Where an actor is, in chunks, and whether that came from its state.
pub fn position_of(actor: &Actor) -> (f64, f64, f64, bool) {
    if let Some(state) = actor.state.as_deref() {
        if let Ok(s) = decode::<ActorState>(state) {
            return (s.x, s.y, s.z, true);
        }
        if state.len() >= POSE_BYTES {
            let f = |i: usize| f32::from_le_bytes([state[i], state[i + 1], state[i + 2], state[i + 3]]) as f64;
            let (x, y, z) = (f(0), f(4), f(8));
            if x.is_finite() && y.is_finite() && z.is_finite() {
                return (x / CHUNK_BLOCKS, y / CHUNK_BLOCKS, z / CHUNK_BLOCKS, true);
            }
        }
    }
    (actor.chunk.x as f64 + 0.5, actor.chunk.y as f64 + 0.5, actor.chunk.z as f64 + 0.5, false)
}

/// Every player among `actors`, once each (the first actor a player has).
pub fn players_among(actors: &[Actor]) -> Vec<Seen> {
    let mut out: Vec<Seen> = Vec::new();
    for a in actors {
        let Some(player) = a.player else { continue };
        if out.iter().any(|s| s.player == player) {
            continue;
        }
        let (x, y, z, precise) = position_of(a);
        out.push(Seen { player, uuid: a.uuid.clone(), x, y, z, precise });
    }
    out
}

/// The players within `radius` chunks (at most 3 across, 1 up and down) of `center`.
pub fn nearby(ctx: &Ctx, center: ChunkPos, radius: u8) -> Result<Vec<Seen>, CallError> {
    Ok(players_among(&ctx.world().actors_near(center, radius, 1)?))
}

/// Distance on the ground (x and z), in chunks.
pub fn ground_distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Players' recent positions and speeds, from one scan to the next.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Tracker {
    #[serde(default)]
    pub last: BTreeMap<u64, Track>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Track {
    pub x: f64,
    pub z: f64,
    pub at: u64,
    /// Chunks a second between the last two scans; 0 for the first.
    pub speed: f64,
}

impl Tracker {
    /// Takes a scan at `now`; players not in it are forgotten. Returns each player's speed.
    pub fn update(&mut self, seen: &[Seen], now: u64) -> BTreeMap<u64, f64> {
        let mut next = BTreeMap::new();
        for s in seen {
            let speed = match self.last.get(&s.player) {
                Some(t) if now > t.at => ground_distance((t.x, t.z), (s.x, s.z)) / ((now - t.at) as f64 / 1_000.0),
                _ => 0.0,
            };
            next.insert(s.player, Track { x: s.x, z: s.z, at: now, speed });
        }
        self.last = next;
        self.last.iter().map(|(p, t)| (*p, t.speed)).collect()
    }

    pub fn position(&self, player: u64) -> Option<(f64, f64)> {
        self.last.get(&player).map(|t| (t.x, t.z))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode;

    fn actor(player: Option<u64>, chunk: (i64, i64, i64), state: Option<Vec<u8>>) -> Actor {
        Actor { uuid: format!("a{player:?}"), player, chunk: ChunkPos::new(chunk.0, chunk.1, chunk.2), state }
    }

    #[test]
    fn a_kit_state_or_an_engine_pose_places_a_player_exactly_and_anything_else_at_the_chunk_centre() {
        let kit = encode(&ActorState::new("player", 1, 2.25, -3.5)).unwrap();
        assert_eq!(position_of(&actor(Some(1), (2, 0, -4), Some(kit))), (2.25, 0.0, -3.5, true));
        let mut pose = vec![0u8; POSE_BYTES];
        pose[0..4].copy_from_slice(&40.0f32.to_le_bytes());
        pose[8..12].copy_from_slice(&(-8.0f32).to_le_bytes());
        assert_eq!(position_of(&actor(Some(2), (2, 0, -1), Some(pose))), (2.5, 0.0, -0.5, true));
        assert_eq!(position_of(&actor(Some(3), (1, 0, 1), Some(b"opaque".to_vec()))), (1.5, 0.5, 1.5, false));
        let seen = players_among(&[actor(None, (0, 0, 0), None), actor(Some(4), (0, 0, 0), None), actor(Some(4), (9, 0, 9), None)]);
        assert_eq!(seen.len(), 1, "an NPC is not a player, and a player counts once");
    }

    #[test]
    fn a_tracker_tells_each_players_speed() {
        let mut t = Tracker::default();
        let s = |x: f64| vec![Seen { player: 1, uuid: "u".into(), x, y: 0.0, z: 0.0, precise: true }];
        assert_eq!(t.update(&s(0.0), 1_000)[&1], 0.0);
        assert_eq!(t.update(&s(3.0), 2_000)[&1], 3.0);
        assert!(t.update(&[], 3_000).is_empty(), "gone from the scan, forgotten");
    }
}
