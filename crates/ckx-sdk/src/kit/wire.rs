//! What kit hubs show players near them through the realtime tier ([`crate::emit`]): server
//! events, numbered by the registry below, and the state of server-driven actors.
//!
//! A server event's payload is `[event type, u16 little endian][JSON]`. The bodies here use
//! camelCase keys, the shape the kit clients' event parsers read. An app's own events take
//! numbers from [`APP_EVENTS`] up.
//!
//! A server-driven actor's state is [`ActorState`] as MessagePack: the kit clients decode it
//! into their actor stores like a player's.

use serde::{Deserialize, Serialize};

use crate::emit::{MAX_DISTANCE, MAX_PAYLOAD, Spatial};
use crate::node::ChunkPos;
use crate::{Ctx, Error, Result, encode};

/// A mob or a trap hit a player.
pub const CONTACT_DAMAGE: u16 = 77;
/// The weather or the time of day turned.
pub const WEATHER: u16 = 90;
/// A turn passed to the next player.
pub const TURN: u16 = 91;
/// Scores or a result.
pub const SCORE: u16 = 92;
/// A match was proposed to the players in it.
pub const PROPOSAL: u16 = 93;
/// An ability was cast, or landed.
pub const ABILITY: u16 = 94;
/// A movement the warden flagged.
pub const MOVEMENT_VIOLATION: u16 = 95;
/// A control point changed hands, or its capture moved.
pub const CONTROL_POINT: u16 = 96;
/// A racer started, passed a checkpoint, finished a lap or the race.
pub const RACE_TIMING: u16 = 97;
/// A play zone moved or shrank.
pub const ZONE_CHANGE: u16 = 98;
/// A wave began or ended, or a boss changed phase.
pub const ENCOUNTER: u16 = 99;
/// The contested object (a ball) changed hands, or a goal was scored.
pub const POSSESSION: u16 = 100;
/// An emote: a client event players send each other (the kit clients' convention); a hub may
/// send one too, for a server-driven actor.
pub const EMOTE: u16 = 101;
/// The first number an app's own events may use.
pub const APP_EVENTS: u16 = 1000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContactDamage {
    pub target_uuid: String,
    pub damage: i64,
    pub mob_id: String,
    pub mob_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WeatherChange {
    pub weather: String,
    pub since_ms: u64,
    pub until_ms: u64,
    pub night: bool,
    pub time_of_day: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnChange {
    /// Whose turn it is now: a player id, or a unit's id.
    pub actor_id: String,
    pub round: u32,
    pub turn_in_round: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Standing {
    pub actor_id: String,
    pub score: i64,
    pub rank: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScoreUpdate {
    pub standings: Vec<Standing>,
    pub winner_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProposalMade {
    pub proposal_id: String,
    pub mode: String,
    pub players: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AbilityUse {
    /// `cast` or `impact`.
    pub kind: String,
    pub ability_id: String,
    pub caster_id: String,
    pub victim_id: Option<String>,
    pub damage: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MovementViolation {
    /// `speed`, `teleport` or `bounds`.
    pub kind: String,
    pub user_id: String,
    pub detail: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ControlPointChange {
    pub point_id: String,
    pub owner: String,
    pub previous_owner: String,
    /// Capture progress toward the attacker, 0 to 1.
    pub progress: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RaceTiming {
    /// `started`, `checkpoint`, `lap` or `finished`.
    pub kind: String,
    pub course_id: String,
    pub user_id: String,
    pub checkpoint: u32,
    pub lap: u32,
    pub elapsed_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ZoneChange {
    /// `hold`, `shrink` or `closed`.
    pub kind: String,
    pub phase: Option<u32>,
    pub radius_now: f64,
    pub center_x: f64,
    pub center_z: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EncounterChange {
    /// `wave_started`, `wave_cleared`, `boss_phase` or `finished`.
    pub kind: String,
    pub encounter_id: String,
    pub wave: u32,
    pub phase: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PossessionChange {
    /// `claimed`, `passed`, `loose` or `goal`.
    pub kind: String,
    pub holder_id: Option<String>,
    pub team: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Emote {
    /// The emote's name, as the game knows it: `wave`, `cheer`, ...
    pub emote: String,
    /// The actor it is aimed at, if any.
    pub target_uuid: Option<String>,
}

/// A server event's payload: its type, then its body as JSON.
pub fn event_bytes<T: Serialize + ?Sized>(event_type: u16, body: &T) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(body).map_err(|e| Error::new(format!("an event body encodes as JSON: {e}")))?;
    let mut out = Vec::with_capacity(2 + json.len());
    out.extend_from_slice(&event_type.to_le_bytes());
    out.extend_from_slice(&json);
    if out.len() > MAX_PAYLOAD {
        return Err(Error::new(format!("a realtime payload is at most {MAX_PAYLOAD} bytes")));
    }
    Ok(out)
}

/// Sends a server event to the players within `distance` chunks of `at` (at most
/// [`MAX_DISTANCE`]). `actor` is the event's actor: the one it is about, or the hub's own.
pub fn emit_event<T: Serialize + ?Sized>(ctx: &Ctx, at: ChunkPos, distance: u8, actor: &[u8; 32], event_type: u16, body: &T) -> Result<()> {
    let bytes = event_bytes(event_type, body)?;
    ctx.emit_spatial(at, distance.min(MAX_DISTANCE), actor, Spatial::Event { event_type, state: &bytes[2..] })?;
    Ok(())
}

/// A server-driven actor as players near it see it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ActorState {
    /// What it is: `mob`, `npc`, `pet`, `ball`, `ghost`, a game's own word.
    pub kind: String,
    /// Its id within the hub that shows it.
    pub id: u64,
    /// Where it is, in chunks.
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    pub z: f64,
    /// Its health, when it has any.
    pub hp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hp: Option<i64>,
    /// What it is doing: `idle`, `chase`, `follow`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The player it belongs to (a pet, a summon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<u64>,
    /// Facing, in radians.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaw: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ActorState {
    pub fn new(kind: &str, id: u64, x: f64, z: f64) -> Self {
        Self { kind: kind.to_owned(), id, x, y: 0.0, z, hp: None, max_hp: None, mode: None, owner: None, yaw: None, name: None }
    }

    /// The chunk it stands in.
    pub fn chunk(&self) -> ChunkPos {
        ChunkPos::new(self.x.floor() as i64, self.y.floor() as i64, self.z.floor() as i64)
    }
}

/// Shows a server-driven actor to the players within `distance` chunks of where it stands.
pub fn emit_actor_state(ctx: &Ctx, actor: &[u8; 32], distance: u8, state: &ActorState) -> Result<()> {
    ctx.emit_actor(actor, state.chunk(), distance.min(MAX_DISTANCE), &encode(state)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_its_type_then_json() {
        let bytes = event_bytes(TURN, &TurnChange { actor_id: "7".into(), round: 2, turn_in_round: 1 }).unwrap();
        assert_eq!(&bytes[..2], &[91, 0]);
        let body: serde_json::Value = serde_json::from_slice(&bytes[2..]).unwrap();
        assert_eq!(body, serde_json::json!({ "actorId": "7", "round": 2, "turnInRound": 1 }));
        assert!(event_bytes(APP_EVENTS, &"x".repeat(MAX_PAYLOAD)).is_err());
    }

    #[test]
    fn an_actor_state_leaves_out_what_it_does_not_have() {
        let mut s = ActorState::new("mob", 3, 1.5, -2.25);
        s.hp = Some(40);
        let back: serde_json::Value = crate::decode(&encode(&s).unwrap()).unwrap();
        assert_eq!(back, serde_json::json!({ "kind": "mob", "id": 3, "x": 1.5, "y": 0.0, "z": -2.25, "hp": 40 }));
        assert_eq!(s.chunk(), ChunkPos::new(1, 0, -3));
    }
}
