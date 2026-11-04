//! Write ck-exec hubs and spokes.
//!
//! A **hub** owns state and runs one handler at a time; a **spoke** holds no authoritative
//! state and changes hubs only by calling their endpoints. Both are plain Rust types built for
//! `wasm32-unknown-unknown` and exported with [`export_hub!`] or [`export_spoke!`].
//!
//! ```ignore
//! use ckx_sdk::prelude::*;
//!
//! #[derive(Default, Serialize, Deserialize)]
//! struct Counter { total: i64 }
//!
//! impl Hub for Counter {
//!     fn spawn(_ctx: &Ctx, _seed: &[u8]) -> Result<Self> { Ok(Self::default()) }
//!     fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> { decode(snapshot) }
//!     fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> { encode(self) }
//!     fn handle(&mut self, _ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
//!         match call.method {
//!             "add" => { self.total += call.decode::<i64>()?; encode(&self.total) }
//!             other => Err(Error::unknown_method(other)),
//!         }
//!     }
//! }
//!
//! ckx_sdk::export_hub!(Counter);
//! ```
//!
//! Payloads are bytes. [`encode`] and [`decode`] use MessagePack with named fields, which
//! game clients decode into plain objects.
//!
//! Realtime events players around see at once go through [`Ctx::emit_spatial`],
//! [`Ctx::emit_actor`] and [`Ctx::emit_channel`]; see [`emit`].
//!
//! Platform data (world, grids, players, grid permissions, players' access to the app, safety
//! reports) is behind [`Ctx::world`], [`Ctx::grids`], [`Ctx::players`], [`Ctx::permissions`],
//! [`Ctx::access`] and [`Ctx::safety`], each needing a scope the node type declares in the
//! manifest; see [`node`].
//!
//! Hubs also get platform events, each a default no-op on [`Hub`]: timers they set
//! ([`Hub::on_timer`]), topics they watch ([`Hub::on_topic`]), players entering and leaving
//! the app ([`Hub::on_presence`], root hub only) and players' connections subscribing to them
//! ([`Hub::on_session`]). A timer can follow a cron schedule, the legacy automations' syntax:
//! see [`cron`] and [`Ctx::timer_cron`]. Two more concern what a hub holds about a player:
//! erasing it when their account is erased ([`Hub::on_player_erased`]) and exporting it
//! ([`Hub::on_player_export`]); a hub that does not implement them answers "not implemented".
//!
//! Nodes are unit tested natively with `cargo test`: `testing` runs one against a fake of the
//! platform (calls, timers, events, snapshots, the node API, a mod's grid). [`limits`] has the
//! platform's bounds.
//!
//! [`kit`] is the game kit's half here: the protocol kit hubs speak to the kit clients
//! (snapshots and deltas with sequence numbers, retry-safe actions, refusal codes), shard keys,
//! a seeded random generator, the realtime event registry, and the pieces games are made of
//! (`kit::play`, `kit::sim`, `kit::ai`, and the soft economy in `kit::econ`).

mod abi;
pub mod cron;
pub mod emit;
pub mod kit;
pub mod limits;
pub mod model;
pub mod node;
#[doc(hidden)]
pub mod rt;
#[cfg(not(target_arch = "wasm32"))]
pub mod testing;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
/// What [`Hub::on_player_export`] answers in, for a crate that does not depend on it itself.
pub use serde_json;

pub mod prelude {
    pub use crate::cron::Cron;
    pub use crate::emit::{Spatial, actor_id};
    pub use crate::model::{Model, PropKind, Schema, Viewer, Visibility};
    pub use crate::node::{AgeBracket, ChunkPos, GridPick, ReportReason, SafetyReport, VoxelWrite};
    pub use crate::{
        Call, CallError, Caller, Ctx, Error, Hub, Kind, Level, Presence, PresencePlayer, Result, Session, Spoke, Status, TopicMsg,
        WorldEvent, decode, encode,
    };
    pub use serde::{Deserialize, Serialize};
}

/// An error a handler returns. The caller receives its message with status `AppError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    pub fn unknown_method(method: &str) -> Self {
        Self(format!("unknown method `{method}`"))
    }

    /// What a hook the hub does not implement answers ([`Hub::on_player_erased`],
    /// [`Hub::on_player_export`]). The platform tells it from a failure: it records it and goes
    /// on.
    pub fn not_implemented(hook: &str) -> Self {
        Self(format!("not implemented: `{hook}`"))
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<CallError> for Error {
    fn from(e: CallError) -> Self {
        Self(format!("{:?}: {}", e.status, e.message))
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Encodes a value as MessagePack with named fields.
pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(value).map_err(|e| Error(format!("encode: {e}")))
}

/// Decodes MessagePack.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    rmp_serde::from_slice(bytes).map_err(|e| Error(format!("decode: {e}")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hub,
    Spoke,
}

/// Who is calling. The platform sets this; a handler can trust it for authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    Platform,
    Instance(u64),
    Player(u64),
    /// A developer of the app (the org's `manage_compute`), by user id, connected with
    /// `execConnectAsDeveloper`: studio tools, manual runs and admin endpoints. It may call any
    /// type, not only `client` ones, so check for it in handlers only developers should reach.
    Developer(u64),
}

/// One incoming call.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    pub caller: Caller,
    pub method: &'a str,
    pub payload: &'a [u8],
}

impl Call<'_> {
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        decode(self.payload)
    }

    /// The calling player, or an error when the caller is not a player.
    pub fn player(&self) -> Result<u64> {
        match self.caller {
            Caller::Player(p) => Ok(p),
            other => Err(Error(format!("`{}` is for players, not {other:?}", self.method))),
        }
    }

    /// The calling developer, or an error when the caller is not one: the guard for an admin
    /// endpoint.
    pub fn developer(&self) -> Result<u64> {
        match self.caller {
            Caller::Developer(u) => Ok(u),
            other => Err(Error(format!("`{}` is for the app's developers, not {other:?}", self.method))),
        }
    }
}

/// A message published on a topic this hub watches ([`Ctx::subscribe`]).
#[derive(Debug, Clone, Copy)]
pub struct TopicMsg<'a> {
    /// The publishing instance's type and key.
    pub node_type: &'a str,
    pub key: &'a str,
    pub topic: &'a str,
    pub payload: &'a [u8],
}

impl TopicMsg<'_> {
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        decode(self.payload)
    }
}

/// Players who entered or left the app since the last event, or a new count, delivered to the
/// root hub. Events can be lost when the root hub moves; `count` is always current.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    /// Players in the app now.
    pub count: u32,
    #[serde(default)]
    pub joined: Vec<PresencePlayer>,
    #[serde(default)]
    pub left: Vec<PresencePlayer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresencePlayer {
    pub player: u64,
    /// The player's avatar in the world, when they have one.
    #[serde(default)]
    pub actor: Option<String>,
}

/// Something that happened in a mod's grid, delivered to mods only. Events come in batches
/// and can be lost; each names the chunk it happened in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldEvent {
    pub grid: u64,
    /// [`WorldEvent::ACTORS`] or [`WorldEvent::VOXELS`].
    pub kind: String,
    pub chunk: [i64; 3],
}

impl WorldEvent {
    /// Actors moved into or out of the chunk.
    pub const ACTORS: &'static str = "actors";
    /// Voxels in the chunk changed.
    pub const VOXELS: &'static str = "voxels";
}

/// A player's connection subscribing to a hub's first topic, or leaving its last. A player
/// connected twice joins twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Joined,
    Left,
}

/// The platform's status for a call this instance made. Mirrors the wire status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    AppError,
    Busy,
    Moved,
    NotFound,
    DeadlineExceeded,
    Denied,
    RateLimited,
    Unavailable,
    Internal,
    Trapped,
    BadRequest,
    Unknown(u8),
}

impl Status {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Status::Ok,
            1 => Status::AppError,
            2 => Status::Busy,
            3 => Status::Moved,
            4 => Status::NotFound,
            5 => Status::DeadlineExceeded,
            6 => Status::Denied,
            7 => Status::RateLimited,
            8 => Status::Unavailable,
            9 => Status::Internal,
            10 => Status::Trapped,
            11 => Status::BadRequest,
            other => Status::Unknown(other),
        }
    }

    /// The wire status code: [`Status::from_u8`] reversed.
    pub fn as_u8(self) -> u8 {
        match self {
            Status::Ok => 0,
            Status::AppError => 1,
            Status::Busy => 2,
            Status::Moved => 3,
            Status::NotFound => 4,
            Status::DeadlineExceeded => 5,
            Status::Denied => 6,
            Status::RateLimited => 7,
            Status::Unavailable => 8,
            Status::Internal => 9,
            Status::Trapped => 10,
            Status::BadRequest => 11,
            Status::Unknown(other) => other,
        }
    }

    pub fn is_retryable(self) -> bool {
        matches!(self, Status::Busy | Status::Moved | Status::Unavailable | Status::RateLimited)
    }
}

/// A failed call made by this instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallError {
    pub status: Status,
    pub message: String,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.status, self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

/// This instance's identity and its way out: calls, messages, publishing and the node API.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub app: u64,
    pub instance: u64,
    pub epoch: u64,
    /// The deployed code version.
    pub version: u64,
    pub kind: Kind,
    pub node_type: String,
    pub key: String,
}

/// Default deadline for calls made without one.
pub const DEFAULT_DEADLINE_MS: u32 = 2_000;

fn request(node_type: &str, key: &str, method: &str, payload: &[u8]) -> Vec<u8> {
    let (t, k, m) = (
        &node_type.as_bytes()[..node_type.len().min(255)],
        &key.as_bytes()[..key.len().min(65535)],
        &method.as_bytes()[..method.len().min(255)],
    );
    let mut req = Vec::with_capacity(4 + t.len() + k.len() + m.len() + payload.len());
    req.push(t.len() as u8);
    req.extend_from_slice(t);
    req.extend_from_slice(&(k.len() as u16).to_le_bytes());
    req.extend_from_slice(k);
    req.push(m.len() as u8);
    req.extend_from_slice(m);
    req.extend_from_slice(payload);
    req
}

impl Ctx {
    /// Calls an endpoint on another instance of this app and waits for its reply. This
    /// instance handles nothing else until the reply arrives or the deadline passes.
    pub fn call(&self, node_type: &str, key: &str, method: &str, payload: &[u8]) -> Result<Vec<u8>, CallError> {
        self.call_with_deadline(node_type, key, method, payload, DEFAULT_DEADLINE_MS)
    }

    pub fn call_with_deadline(
        &self,
        node_type: &str,
        key: &str,
        method: &str,
        payload: &[u8],
        deadline_ms: u32,
    ) -> Result<Vec<u8>, CallError> {
        rt::import_result(abi::call(&request(node_type, key, method, payload), deadline_ms))
    }

    /// [`Ctx::call`] with MessagePack encoding on both sides.
    pub fn call_typed<Req: Serialize + ?Sized, Resp: DeserializeOwned>(
        &self,
        node_type: &str,
        key: &str,
        method: &str,
        req: &Req,
    ) -> Result<Resp> {
        let bytes = self.call(node_type, key, method, &encode(req)?)?;
        decode(&bytes)
    }

    /// Sends a one-way message. Delivery is attempted once; there is no reply.
    pub fn send(&self, node_type: &str, key: &str, method: &str, payload: &[u8]) -> Result<(), CallError> {
        match Status::from_u8(abi::send(&request(node_type, key, method, payload)) as u8) {
            Status::Ok => Ok(()),
            status => Err(CallError { status, message: "send refused".into() }),
        }
    }

    /// Pushes `payload` to every subscriber of `topic` on this instance.
    pub fn publish(&self, topic: &str, payload: &[u8]) {
        abi::publish(topic, payload);
    }

    /// A raw node API request: `op` and a JSON body, answered with JSON. The typed operations
    /// behind [`Ctx::world`], [`Ctx::grids`], [`Ctx::players`], [`Ctx::permissions`] and
    /// [`Ctx::access`] use it.
    pub fn node(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>, CallError> {
        rt::import_result(abi::node(op, payload, DEFAULT_DEADLINE_MS))
    }

    /// Sends a spatial event to the players within `distance` chunks of `at` (at most 8), at
    /// once, through the realtime tier. `actor` is the event's actor: the actor whose state it
    /// is, or the one an [`Spatial::ActorMessage`] is for. See [`emit`].
    pub fn emit_spatial(&self, at: node::ChunkPos, distance: u8, actor: &[u8; 32], event: emit::Spatial<'_>) -> Result<(), CallError> {
        emit::spatial(at, distance, actor, event)
    }

    /// Shows a server-driven actor (an NPC, a mob) with `state` to the players near `at`.
    pub fn emit_actor(&self, actor: &[u8; 32], at: node::ChunkPos, distance: u8, state: &[u8]) -> Result<(), CallError> {
        emit::spatial(at, distance, actor, emit::Spatial::Actor { state })
    }

    /// Sends `payload` to every member of one of the app's channels, as a channel message from
    /// `sender`.
    pub fn emit_channel(&self, channel: u64, sender: &[u8; 32], payload: &[u8]) -> Result<(), CallError> {
        emit::channel(channel, sender, payload)
    }

    /// Chunks, voxels and actors in this app's world (scopes `world.read`, `world.write`).
    pub fn world(&self) -> node::World<'_> {
        node::World(self)
    }

    /// This app's grids and their permissions (scopes `grids.read`, `grids.write`).
    pub fn grids(&self) -> node::Grids<'_> {
        node::Grids(self)
    }

    /// Players of this app: identity, tier features, group permissions and app state (scopes
    /// `players.read`, `players.write`).
    pub fn players(&self) -> node::Players<'_> {
        node::Players(self)
    }

    /// Grants and revokes grid permissions (scope `permissions.write`).
    pub fn permissions(&self) -> node::Permissions<'_> {
        node::Permissions(self)
    }

    /// Players' access to this app: read, grant, revoke, suspend (scopes `access.read`,
    /// `access.write`).
    pub fn access(&self) -> node::Access<'_> {
        node::Access(self)
    }

    /// Safety reports a player files through this hub (scope `safety.write`).
    pub fn safety(&self) -> node::Safety<'_> {
        node::Safety(self)
    }

    pub fn log(&self, level: Level, msg: &str) {
        abi::log(level as u32, msg);
    }

    pub fn now_ms(&self) -> u64 {
        abi::now_ms()
    }

    /// Runs [`Hub::on_timer`] with `name` once, `delay_ms` from now, replacing any timer of
    /// that name. Timers are kept in the snapshot, so they survive stops and moves; they run
    /// only while the hub runs, and one that came due meanwhile runs once when it is back.
    pub fn timer_after(&self, name: &str, delay_ms: u64) -> Result<()> {
        rt::set_timer(self, name, delay_ms, 0)
    }

    /// Runs [`Hub::on_timer`] with `name` every `period_ms` (at least 10), the first time one
    /// period from now. Replaces any timer of that name.
    pub fn timer_every(&self, name: &str, period_ms: u64) -> Result<()> {
        rt::set_timer(self, name, period_ms, period_ms)
    }

    /// Cancels a timer. Returns whether it was set.
    pub fn cancel_timer(&self, name: &str) -> bool {
        rt::cancel_timer(name)
    }

    /// Pending timers and when each is next due, in Unix milliseconds.
    pub fn timers(&self) -> Vec<(String, u64)> {
        rt::timers()
    }

    /// Subscribes this hub to `topic` on another instance of the app, if the manifest lets this
    /// type call that one. Messages arrive at [`Hub::on_topic`]. The subscription is kept in
    /// the snapshot and renewed whenever the hub starts again.
    pub fn subscribe(&self, node_type: &str, key: &str, topic: &str) -> Result<(), CallError> {
        rt::watch(self, node_type, key, topic, true)
    }

    pub fn unsubscribe(&self, node_type: &str, key: &str, topic: &str) -> Result<(), CallError> {
        rt::watch(self, node_type, key, topic, false)
    }

    /// Snapshots this hub soon after the current call returns (at most once a second), for a
    /// change that should not wait for the snapshot interval.
    pub fn persist_now(&self) {
        abi::persist_now();
    }
}

/// A stateful node: one instance per key, one handler at a time.
pub trait Hub: Sized + 'static {
    /// A new instance with no snapshot.
    fn spawn(ctx: &Ctx, seed: &[u8]) -> Result<Self>;
    /// An instance restored from its last snapshot, written by code version `from_version`.
    fn load(ctx: &Ctx, snapshot: &[u8], from_version: u64) -> Result<Self>;
    /// The state to snapshot. Called on the snapshot interval, before a move and before a stop.
    fn persist(&mut self, ctx: &Ctx) -> Result<Vec<u8>>;
    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>>;
    /// Called once before a graceful stop, after the last persist.
    fn evict(&mut self, _ctx: &Ctx) {}
    /// A timer set with [`Ctx::timer_after`] or [`Ctx::timer_every`] is due. An error is logged;
    /// the timer stays as scheduled.
    fn on_timer(&mut self, _ctx: &Ctx, _timer: &str) -> Result<()> {
        Ok(())
    }
    /// A message on a topic this hub watches.
    fn on_topic(&mut self, _ctx: &Ctx, _msg: TopicMsg<'_>) -> Result<()> {
        Ok(())
    }
    /// Root hub only: players entered or left the app.
    fn on_presence(&mut self, _ctx: &Ctx, _presence: &Presence) -> Result<()> {
        Ok(())
    }
    /// A player's connection subscribed to this hub, or left it.
    fn on_session(&mut self, _ctx: &Ctx, _player: u64, _session: Session) -> Result<()> {
        Ok(())
    }
    /// Mods only: things happened in the mod's grid. An actor arriving starts a mod that is
    /// not running, and this is its first call after `spawn` or `load`.
    fn on_world(&mut self, _ctx: &Ctx, _events: &[WorldEvent]) -> Result<()> {
        Ok(())
    }
    /// `player`'s account is being erased: delete or anonymize everything this hub holds about
    /// them. Every hub of the app that holds state hears it, a stopped one started for it,
    /// the player's own included (one keyed by their id, which the platform then deletes with
    /// its snapshot). The platform counts the hub done once a snapshot taken after this returns
    /// is stored; an error is recorded, and the erasure asks again later. Left out, the hub
    /// answers "not implemented", which is recorded and does not hold the erasure up.
    fn on_player_erased(&mut self, _ctx: &Ctx, _player: u64) -> Result<()> {
        Err(Error::not_implemented("on_player_erased"))
    }
    /// What this hub holds about `player`, for their data export: JSON, at most
    /// [`limits::MAX_EXPORT_BYTES`] of it, or `Value::Null` when it holds nothing. Every hub of
    /// the app that holds state is asked, a stopped one started for it. Left out, the hub
    /// answers "not implemented", which the export records.
    fn on_player_export(&mut self, _ctx: &Ctx, _player: u64) -> Result<serde_json::Value> {
        Err(Error::not_implemented("on_player_export"))
    }
}

/// A stateless node. Replicas may run anywhere; keep nothing here you cannot lose.
pub trait Spoke: Sized + 'static {
    fn start(ctx: &Ctx) -> Result<Self>;
    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>>;
}

/// Exports a [`Hub`] type as this module's node.
#[macro_export]
macro_rules! export_hub {
    ($t:ty) => {
        $crate::__export_node!($crate::rt::HubNode<$t>);
    };
}

/// Exports a [`Spoke`] type as this module's node.
#[macro_export]
macro_rules! export_spoke {
    ($t:ty) => {
        $crate::__export_node!($crate::rt::SpokeNode<$t>);
    };
}

// Only the module the sandbox loads needs the exports: a native build (a unit test, a crate
// that includes several nodes' sources) defines none, so nodes link side by side.
#[doc(hidden)]
#[macro_export]
macro_rules! __export_node {
    ($node:ty) => {
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn ckx_abi() -> u32 {
            $crate::rt::ABI_VERSION
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn ckx_alloc(len: u32) -> u32 {
            $crate::rt::alloc(len)
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn ckx_init(
            ctx_ptr: u32,
            ctx_len: u32,
            seed_ptr: u32,
            seed_len: u32,
            snap_ptr: u32,
            snap_len: u32,
            snap_version: u64,
            has_snap: u32,
        ) -> u64 {
            $crate::rt::init::<$node>(ctx_ptr, ctx_len, seed_ptr, seed_len, snap_ptr, snap_len, snap_version, has_snap)
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn ckx_handle(caller_kind: u32, caller_id: u64, buf_ptr: u32, method_len: u32, payload_len: u32) -> u64 {
            $crate::rt::handle(caller_kind, caller_id, buf_ptr, method_len, payload_len)
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn ckx_persist() -> u64 {
            $crate::rt::persist()
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn ckx_evict() {
            $crate::rt::evict()
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout_matches_the_abi() {
        let req = request("arena", "m1", "hit", b"\x01");
        assert_eq!(req, b"\x05arena\x02\x00m1\x03hit\x01");
    }

    #[test]
    fn encode_uses_named_fields() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Hit {
            damage: i64,
        }
        let bytes = encode(&Hit { damage: 5 }).unwrap();
        assert!(bytes.windows(6).any(|w| w == b"damage"));
        assert_eq!(decode::<Hit>(&bytes).unwrap(), Hit { damage: 5 });
    }
}
