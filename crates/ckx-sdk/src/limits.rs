//! The platform's bounds on a node: what its sandbox and its checks hold every call, snapshot
//! and module to. [`crate::testing`] applies the ones a native test can see.

pub use crate::node::{MAX_CHUNK_STATE, MAX_VOXEL_STATE, MAX_VOXEL_WRITES};
pub use crate::rt::{MAX_TIMERS, MAX_WATCHES, MIN_TIMER_MS};

/// The largest payload a node may send in one message, and the largest reply it may return. A
/// larger reply is answered `Internal`.
pub const MAX_PAYLOAD: usize = 4 << 20;
/// What one call into a node may send on, in all: messages (calls, sends, publishes, emits,
/// node API requests, subscriptions) and bytes of their payloads. Past either, calls and node
/// API requests answer `Busy` and publishes are dropped.
pub const OUT_MSGS_PER_CALL: u32 = 16_384;
pub const OUT_BYTES_PER_CALL: u64 = 32 << 20;
/// Log lines one call may write (the rest are dropped), and the bytes of a line that are kept.
pub const LOGS_PER_CALL: u32 = 32;
pub const MAX_LOG_BYTES: usize = 4 << 10;
/// The largest realtime inject a node may emit: a spatial event's header and its payload.
pub const MAX_EMIT_BYTES: usize = 70 + crate::emit::MAX_PAYLOAD;
/// The largest snapshot a hub may persist. A larger one fails to save, and the last one stands.
pub const MAX_SNAPSHOT_BYTES: usize = 8 << 20;
/// The most JSON a hub's [`crate::Hub::on_player_export`] may answer. A larger answer is an
/// error, and the export records the hub as failed.
pub const MAX_EXPORT_BYTES: usize = 1 << 20;
/// The largest module an app's node type may deploy.
pub const ARTIFACT_MAX_BYTES: usize = 16 << 20;

/// What a manifest may ask for a node type, and what it gets when it asks for nothing.
pub mod manifest {
    pub const MEMORY_DEFAULT_MB: u32 = 64;
    pub const MEMORY_MAX_MB: u32 = 512;
    pub const FUEL_DEFAULT: u64 = 50_000_000;
    pub const FUEL_MAX: u64 = 1_000_000_000;
    pub const DEADLINE_DEFAULT_MS: u32 = 2_000;
    pub const DEADLINE_MAX_MS: u32 = 10_000;
}

/// What the platform gives every mod, in place of a manifest's limits.
pub mod mods {
    /// A mod is the hub `("mod:<name>", "<grid id>")`.
    pub const TYPE_PREFIX: &str = "mod:";
    pub const MEMORY_MB: u32 = 32;
    pub const FUEL_PER_CALL: u64 = 20_000_000;
    pub const DEADLINE_MS: u32 = 2_000;
    /// Handler calls per second, per mod instance.
    pub const RATE_PER_SEC: u32 = 20;
    pub const ARTIFACT_MAX_BYTES: usize = 4 << 20;
    /// Mods one grid may run, and one owner may hold.
    pub const PER_GRID_MAX: usize = 8;
    pub const PER_OWNER_MAX: usize = 64;
    /// World events in one `$world` call, at most.
    pub const WORLD_BATCH_MAX: usize = 512;
    /// The node API scopes every mod holds, each confined to its grid.
    pub const SCOPES: [&str; 3] = ["grids.read", "world.read", "world.write"];
}
