//! Realtime events: what players near a chunk, or in a channel, see at once. The host sends
//! them straight to the realtime tier (the servers players' clients are connected to), not
//! through the game API, and players' clients receive them as the same messages other players'
//! actions produce. Use [`Ctx::publish`](crate::Ctx::publish) topics for game state a client
//! subscribes to through the gateway; use these for the world everyone around sees.
//!
//! Limits: a payload is at most [`MAX_PAYLOAD`] bytes, replication distance at most 8 chunks.
//! Each app has a rate on each host; beyond it events are dropped and counted, not queued.

use crate::node::ChunkPos;
use crate::{CallError, Status, abi};

pub const MAX_PAYLOAD: usize = 1024;
pub const MAX_DISTANCE: u8 = 8;

/// A 32-byte actor id, as the realtime tier addresses actors. Players' actors use their
/// 32-character uuid; a server-driven actor (an NPC, a mob) can use any 32 bytes.
pub fn actor_id(uuid: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    let b = uuid.as_bytes();
    out[..b.len().min(32)].copy_from_slice(&b[..b.len().min(32)]);
    out
}

/// What a spatial event is to the players who receive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spatial<'a> {
    /// An actor's state: a server-driven actor appears and moves like a player's.
    Actor { state: &'a [u8] },
    /// A voxel edit at `(x, y, z)` within the chunk, folded into the live mesh. The durable
    /// write is `ctx.world().set_voxels`, which already shows it; this is for effects.
    Voxel { x: i16, y: i16, z: i16, voxel_type: i16, state: &'a [u8] },
    /// A server event (`event_type`, then `state`), as games route server events.
    Event { event_type: u16, state: &'a [u8] },
    /// An app-defined payload.
    Generic { payload: &'a [u8] },
    /// A message to the one actor named as the event's actor.
    ActorMessage { payload: &'a [u8] },
}

impl Spatial<'_> {
    fn opcode(&self) -> u8 {
        match self {
            Spatial::Actor { .. } => 130,
            Spatial::Voxel { .. } => 133,
            Spatial::Event { .. } => 139,
            Spatial::Generic { .. } => 140,
            Spatial::ActorMessage { .. } => 142,
        }
    }

    fn payload(&self) -> Vec<u8> {
        match *self {
            Spatial::Actor { state } => state.to_vec(),
            Spatial::Voxel { x, y, z, voxel_type, state } => {
                let mut p = Vec::with_capacity(10 + state.len());
                for v in [x, y, z, voxel_type] {
                    p.extend_from_slice(&v.to_le_bytes());
                }
                p.extend_from_slice(&(state.len() as u16).to_le_bytes());
                p.extend_from_slice(state);
                p
            }
            Spatial::Event { event_type, state } => {
                let mut p = event_type.to_le_bytes().to_vec();
                p.extend_from_slice(state);
                p
            }
            Spatial::Generic { payload } | Spatial::ActorMessage { payload } => payload.to_vec(),
        }
    }
}

fn refused(message: &str) -> CallError {
    CallError { status: Status::BadRequest, message: message.into() }
}

fn send(inject: &[u8]) -> Result<(), CallError> {
    match Status::from_u8(abi::emit(inject) as u8) {
        Status::Ok => Ok(()),
        status => Err(CallError { status, message: "realtime event refused".into() }),
    }
}

/// The inject for a spatial event (the host writes the app in).
pub(crate) fn spatial(at: ChunkPos, distance: u8, actor: &[u8; 32], event: Spatial<'_>) -> Result<(), CallError> {
    let payload = event.payload();
    if payload.len() > MAX_PAYLOAD {
        return Err(refused("a realtime payload is at most 1024 bytes"));
    }
    if distance > MAX_DISTANCE {
        return Err(refused("replication distance is at most 8 chunks"));
    }
    let mut m = Vec::with_capacity(70 + payload.len());
    m.push(23);
    m.push(event.opcode());
    m.extend_from_slice(&0u64.to_le_bytes());
    for v in [at.x, at.y, at.z] {
        m.extend_from_slice(&v.to_le_bytes());
    }
    m.push(distance);
    m.push(0);
    m.extend_from_slice(actor);
    m.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    m.extend_from_slice(&payload);
    send(&m)
}

/// The inject for a channel message.
pub(crate) fn channel(channel: u64, sender: &[u8; 32], payload: &[u8]) -> Result<(), CallError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(refused("a realtime payload is at most 1024 bytes"));
    }
    let mut m = Vec::with_capacity(51 + payload.len());
    m.push(24);
    m.extend_from_slice(&0u64.to_le_bytes());
    m.extend_from_slice(&channel.to_le_bytes());
    m.extend_from_slice(sender);
    m.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    m.extend_from_slice(payload);
    send(&m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_are_framed_as_clients_decode_them() {
        let v = Spatial::Voxel { x: 1, y: -2, z: 3, voxel_type: 54, state: &[9] };
        assert_eq!(v.payload(), vec![1, 0, 0xfe, 0xff, 3, 0, 54, 0, 1, 0, 9]);
        let v = Spatial::Voxel { x: 0, y: 0, z: 0, voxel_type: -2, state: &[] };
        assert_eq!(v.payload(), vec![0, 0, 0, 0, 0, 0, 0xfe, 0xff, 0, 0], "an app's voxel type is signed");
        assert_eq!(Spatial::Event { event_type: 7, state: b"x" }.payload(), vec![7, 0, b'x']);
        assert_eq!(actor_id("ab")[..3], [b'a', b'b', 0]);
        assert!(spatial(ChunkPos::new(0, 0, 0), 9, &[0; 32], Spatial::Generic { payload: b"" }).is_err());
        assert!(channel(1, &[0; 32], &[0; MAX_PAYLOAD + 1]).is_err());
        assert!(spatial(ChunkPos::new(0, 0, 0), 2, &[0; 32], Spatial::Actor { state: b"pose" }).is_ok());
    }
}
