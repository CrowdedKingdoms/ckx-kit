//! Typed node API operations: the platform's world, grid, player, permission and access data,
//! always within this instance's app. Each needs a scope its node type declares in the manifest
//! (`"scopes": ["world.read", ...]`); without it the call fails with [`Status::Denied`].
//!
//! | Scope | Operations |
//! |---|---|
//! | `players.read` | [`Players::get`], [`Players::features`], [`Players::check_permission`], [`Players::state`], [`Players::avatar_state`] |
//! | `players.write` | [`Players::set_state`] |
//! | `world.read` | [`World::chunk`], [`World::voxels`], [`World::actors`], [`World::actors_near`] |
//! | `world.write` | [`World::set_voxels`], [`World::set_chunk_state`] |
//! | `grids.read` | [`Grids::get`], [`Grids::at`], [`Grids::check_permission`], [`Grids::settings`] |
//! | `grids.write` | [`Grids::create`], [`Grids::delete`], [`Grids::assign_group`], [`Grids::unassign_group`], [`Grids::set_open_permissions`] |
//! | `permissions.write` | [`Permissions::grant`], [`Permissions::revoke`] |
//! | `access.read` | [`Access::get`] |
//! | `access.write` | [`Access::grant`], [`Access::revoke`], [`Access::suspend`], [`Access::unsuspend`] |
//!
//! The game API rate limits each app (reads and writes separately); a refusal is
//! [`Status::RateLimited`].

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::de::{DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};

use crate::{CallError, Ctx, Status};

/// Voxel writes in one [`World::set_voxels`] call.
pub const MAX_VOXEL_WRITES: usize = 16;
/// A voxel's state bytes.
pub const MAX_VOXEL_STATE: usize = 1024;
/// A chunk's state bytes ([`World::set_chunk_state`]).
pub const MAX_CHUNK_STATE: usize = 65_536;

/// A chunk's coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ChunkPos {
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

impl ChunkPos {
    pub const fn new(x: i64, y: i64, z: i64) -> Self {
        Self { x, y, z }
    }

    fn json(self) -> Value {
        json!({ "x": self.x.to_string(), "y": self.y.to_string(), "z": self.z.to_string() })
    }
}

/// A chunk as stored: the dense voxel grid and the game's own metadata blob.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub id: u64,
    pub at: ChunkPos,
    pub voxels: Option<Vec<u8>>,
    pub chunk_state: Option<Vec<u8>>,
}

/// A voxel written into a chunk (the edits on top of its dense grid), with who wrote it and
/// when.
#[derive(Debug, Clone, PartialEq)]
pub struct Voxel {
    pub x: i16,
    pub y: i16,
    pub z: i16,
    /// What a type means is up to the app.
    pub voxel_type: i16,
    pub state: Option<Vec<u8>>,
    /// Who wrote it: a user id in decimal, `"0"` when the app's own code did. `None` from a
    /// platform that does not say.
    pub created_by: Option<String>,
    /// When it was written (RFC 3339).
    pub updated_at: Option<String>,
}

/// One voxel to write: `voxel` is its position within `chunk`.
#[derive(Debug, Clone, PartialEq)]
pub struct VoxelWrite {
    pub chunk: ChunkPos,
    /// `(x, y, z)` within the chunk. Positions are the app's to define; an app on the 16×16×16
    /// dense grid keeps to 0 to 15 on each axis.
    pub voxel: (i16, i16, i16),
    /// What a type means is up to the app.
    pub voxel_type: i16,
    pub state: Option<Vec<u8>>,
}

/// A player's actor in the world, from live presence.
#[derive(Debug, Clone, PartialEq)]
pub struct Actor {
    /// The 32-character actor id.
    pub uuid: String,
    pub player: Option<u64>,
    pub chunk: ChunkPos,
    /// The actor's persisted public state, where the game keeps one.
    pub state: Option<Vec<u8>>,
}

/// A grid: a box of chunks with its own permissions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    pub id: u64,
    pub low: ChunkPos,
    pub high: ChunkPos,
    /// Who holds it, where the platform records an owner.
    pub owner: Option<GridOwner>,
    /// Permission keys every player holds on it, granted or not.
    pub open_permissions: Vec<String>,
}

impl Grid {
    pub fn contains(&self, at: ChunkPos) -> bool {
        (self.low.x..=self.high.x).contains(&at.x)
            && (self.low.y..=self.high.y).contains(&at.y)
            && (self.low.z..=self.high.z).contains(&at.z)
    }
}

/// A grid's owner: what kind of party it is, and which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridOwner {
    pub kind: String,
    #[serde(rename = "ref")]
    pub ref_: String,
}

/// The app's grid rules, from [`Grids::settings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GridSettings {
    /// Players may write voxels outside every grid.
    pub wilderness_writes_open: bool,
}

/// Which grid [`Grids::at`] returns where grids overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridPick {
    /// The lowest grid id: what the realtime tier enforces.
    #[default]
    First,
    Smallest,
    Largest,
}

impl GridPick {
    fn as_str(self) -> &'static str {
        match self {
            GridPick::First => "first",
            GridPick::Smallest => "smallest",
            GridPick::Largest => "largest",
        }
    }
}

/// A player's public identity.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Player {
    #[serde(deserialize_with = "de_u64")]
    pub player: u64,
    pub gamertag: String,
    /// What tells players with the same gamertag apart; most players have none.
    #[serde(default)]
    pub disambiguation: Option<String>,
}

/// A player's access to the app, from [`Access::get`] and [`Access::grant`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerAccess {
    /// `active`, `revoked`, or another state the platform names.
    pub status: String,
    /// The access tier, if any.
    #[serde(default, rename = "tierId", deserialize_with = "de_opt_u64")]
    pub tier: Option<u64>,
    /// When it ends (RFC 3339), if it does.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Until when the player is suspended (RFC 3339), while they are.
    #[serde(default)]
    pub suspended_until: Option<String>,
}

fn bad_reply(op: &str, e: impl std::fmt::Display) -> CallError {
    CallError { status: Status::Internal, message: format!("unexpected {op} reply: {e}") }
}

fn bad_request(message: impl Into<String>) -> CallError {
    CallError { status: Status::BadRequest, message: message.into() }
}

fn op<R: DeserializeOwned>(ctx: &Ctx, name: &str, body: Value) -> Result<R, CallError> {
    let body = serde_json::to_vec(&body).map_err(|e| bad_request(e.to_string()))?;
    let bytes = ctx.node(name, &body)?;
    serde_json::from_slice(&bytes).map_err(|e| bad_reply(name, e))
}

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

// ---- wire shapes ----

#[derive(Deserialize)]
#[serde(untagged)]
enum Num {
    Text(String),
    Int(i64),
}

fn de_i64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    match Num::deserialize(d)? {
        Num::Text(s) => s.parse().map_err(serde::de::Error::custom),
        Num::Int(i) => Ok(i),
    }
}

fn de_u64<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = de_i64(d)?;
    u64::try_from(v).map_err(serde::de::Error::custom)
}

fn de_opt_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    match Option::<Num>::deserialize(d)? {
        None => Ok(None),
        Some(Num::Text(s)) => s.parse().map(Some).map_err(serde::de::Error::custom),
        Some(Num::Int(i)) => u64::try_from(i).map(Some).map_err(serde::de::Error::custom),
    }
}

/// An id or a time the platform may send as text or as a number, kept as text.
fn de_opt_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(other) => Err(serde::de::Error::custom(format!("expected text or a number, got {other}"))),
    }
}

fn de_b64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(s) => B64.decode(s).map(Some).map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize)]
struct WirePos {
    #[serde(deserialize_with = "de_i64")]
    x: i64,
    #[serde(deserialize_with = "de_i64")]
    y: i64,
    #[serde(deserialize_with = "de_i64")]
    z: i64,
}

impl From<WirePos> for ChunkPos {
    fn from(p: WirePos) -> Self {
        ChunkPos { x: p.x, y: p.y, z: p.z }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireChunk {
    #[serde(deserialize_with = "de_u64")]
    chunk_id: u64,
    #[serde(flatten)]
    at: WirePos,
    #[serde(default, deserialize_with = "de_b64")]
    state_base64: Option<Vec<u8>>,
    #[serde(default, deserialize_with = "de_b64")]
    chunk_state_base64: Option<Vec<u8>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireVoxel {
    voxel_x: i16,
    voxel_y: i16,
    voxel_z: i16,
    voxel_type: i16,
    #[serde(default, deserialize_with = "de_b64")]
    state_base64: Option<Vec<u8>>,
    #[serde(default, deserialize_with = "de_opt_text")]
    created_by: Option<String>,
    #[serde(default, deserialize_with = "de_opt_text")]
    updated_at: Option<String>,
}

impl From<WireVoxel> for Voxel {
    fn from(v: WireVoxel) -> Self {
        Voxel {
            x: v.voxel_x,
            y: v.voxel_y,
            z: v.voxel_z,
            voxel_type: v.voxel_type,
            state: v.state_base64,
            created_by: v.created_by,
            updated_at: v.updated_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireActor {
    uuid: String,
    #[serde(default, deserialize_with = "de_opt_u64")]
    user_id: Option<u64>,
    #[serde(deserialize_with = "de_i64")]
    chunk_x: i64,
    #[serde(deserialize_with = "de_i64")]
    chunk_y: i64,
    #[serde(deserialize_with = "de_i64")]
    chunk_z: i64,
    #[serde(default, deserialize_with = "de_b64")]
    state_base64: Option<Vec<u8>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireGrid {
    #[serde(deserialize_with = "de_u64")]
    grid_id: u64,
    low: WirePos,
    high: WirePos,
    #[serde(default)]
    owner: Option<GridOwner>,
    #[serde(default)]
    open_permissions: Vec<String>,
}

impl From<WireGrid> for Grid {
    fn from(g: WireGrid) -> Self {
        Grid { id: g.grid_id, low: g.low.into(), high: g.high.into(), owner: g.owner, open_permissions: g.open_permissions }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireState {
    #[serde(default, deserialize_with = "de_b64")]
    state_base64: Option<Vec<u8>>,
}

fn ser_id<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

#[derive(Serialize)]
struct PlayerReq {
    #[serde(serialize_with = "ser_id")]
    player: u64,
}

// ---- the operations ----

/// World reads and writes: [`Ctx::world`].
pub struct World<'a>(pub(crate) &'a Ctx);

impl World<'_> {
    /// The chunk at `at`, or `None` when the app never stored it. Scope `world.read`.
    pub fn chunk(&self, at: ChunkPos) -> Result<Option<Chunk>, CallError> {
        #[derive(Deserialize)]
        struct R {
            chunk: Option<WireChunk>,
        }
        let r: R = op(self.0, "world.chunk", at.json())?;
        Ok(r.chunk.map(|c| Chunk { id: c.chunk_id, at: c.at.into(), voxels: c.state_base64, chunk_state: c.chunk_state_base64 }))
    }

    /// The voxels written into a chunk, at most 2,048. Scope `world.read`.
    pub fn voxels(&self, at: ChunkPos) -> Result<Vec<Voxel>, CallError> {
        #[derive(Deserialize)]
        struct R {
            voxels: Vec<WireVoxel>,
        }
        let r: R = op(self.0, "world.voxels", at.json())?;
        Ok(r.voxels.into_iter().map(Voxel::from).collect())
    }

    /// Writes up to [`MAX_VOXEL_WRITES`] voxels in one transaction, as the app, and shows them
    /// to nearby players. Returns the voxel update ids. Scope `world.write`.
    pub fn set_voxels(&self, writes: &[VoxelWrite]) -> Result<Vec<u64>, CallError> {
        if writes.is_empty() || writes.len() > MAX_VOXEL_WRITES {
            return Err(bad_request(format!("write 1 to {MAX_VOXEL_WRITES} voxels per call")));
        }
        let mut list = Vec::with_capacity(writes.len());
        for w in writes {
            if w.state.as_ref().is_some_and(|s| s.len() > MAX_VOXEL_STATE) {
                return Err(bad_request(format!("a voxel's state is at most {MAX_VOXEL_STATE} bytes")));
            }
            list.push(json!({
                "chunk": w.chunk.json(),
                "voxel": { "x": w.voxel.0, "y": w.voxel.1, "z": w.voxel.2 },
                "voxelType": w.voxel_type,
                "stateBase64": w.state.as_deref().map(b64),
            }));
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct R {
            voxel_update_ids: Vec<String>,
        }
        let r: R = op(self.0, "world.set_voxels", json!({ "writes": list }))?;
        r.voxel_update_ids.iter().map(|id| id.parse().map_err(|e| bad_reply("world.set_voxels", e))).collect()
    }

    /// Replaces the chunk's own state blob, the one [`Chunk::chunk_state`] reads (at most
    /// [`MAX_CHUNK_STATE`] bytes; `None` clears it). Scope `world.write`.
    pub fn set_chunk_state(&self, chunk: ChunkPos, state: Option<&[u8]>) -> Result<(), CallError> {
        if state.is_some_and(|s| s.len() > MAX_CHUNK_STATE) {
            return Err(bad_request(format!("a chunk's state is at most {MAX_CHUNK_STATE} bytes")));
        }
        let _: Value = op(self.0, "world.set_chunk_state", json!({ "chunk": chunk.json(), "stateBase64": state.map(b64) }))?;
        Ok(())
    }

    /// The actors in one chunk. Scope `world.read`.
    pub fn actors(&self, at: ChunkPos) -> Result<Vec<Actor>, CallError> {
        self.actors_op("world.actors", at.json())
    }

    /// The actors within `radius_xz` chunks across and `radius_y` up and down (at most 3 and 1).
    /// Scope `world.read`.
    pub fn actors_near(&self, center: ChunkPos, radius_xz: u8, radius_y: u8) -> Result<Vec<Actor>, CallError> {
        let mut body = center.json();
        body["radiusXz"] = json!(radius_xz.min(3));
        body["radiusY"] = json!(radius_y.min(1));
        self.actors_op("world.actors_radius", body)
    }

    fn actors_op(&self, name: &str, body: Value) -> Result<Vec<Actor>, CallError> {
        #[derive(Deserialize)]
        struct R {
            actors: Vec<WireActor>,
        }
        let r: R = op(self.0, name, body)?;
        Ok(r.actors
            .into_iter()
            .map(|a| Actor {
                uuid: a.uuid,
                player: a.user_id,
                chunk: ChunkPos::new(a.chunk_x, a.chunk_y, a.chunk_z),
                state: a.state_base64,
            })
            .collect())
    }
}

/// Grids and their permissions: [`Ctx::grids`].
pub struct Grids<'a>(pub(crate) &'a Ctx);

impl Grids<'_> {
    /// One of this app's grids. Scope `grids.read`.
    pub fn get(&self, grid: u64) -> Result<Option<Grid>, CallError> {
        self.grid_op(json!({ "grid": grid.to_string() }))
    }

    /// The grid covering chunk `at`, if any. Scope `grids.read`.
    pub fn at(&self, at: ChunkPos, pick: GridPick) -> Result<Option<Grid>, CallError> {
        self.grid_op(json!({ "at": at.json(), "pick": pick.as_str() }))
    }

    /// Whether `player` holds `permission` on `grid` (granted directly or through a group, and
    /// not expired). Scope `grids.read`.
    pub fn check_permission(&self, grid: u64, player: u64, permission: &str) -> Result<bool, CallError> {
        #[derive(Deserialize)]
        struct R {
            allowed: bool,
        }
        let r: R = op(
            self.0,
            "grids.check_permission",
            json!({ "grid": grid.to_string(), "player": player.to_string(), "permission": permission }),
        )?;
        Ok(r.allowed)
    }

    /// The app's grid rules. Scope `grids.read`.
    pub fn settings(&self) -> Result<GridSettings, CallError> {
        op(self.0, "grids.settings", json!({}))
    }

    /// A new grid of the app from chunk `low` to chunk `high`, both included. Scope
    /// `grids.write`.
    pub fn create(&self, low: ChunkPos, high: ChunkPos) -> Result<Grid, CallError> {
        #[derive(Deserialize)]
        struct R {
            grid: WireGrid,
        }
        let r: R = op(self.0, "grids.create", json!({ "low": low.json(), "high": high.json() }))?;
        Ok(r.grid.into())
    }

    /// Removes one of the app's grids. Scope `grids.write`.
    pub fn delete(&self, grid: u64) -> Result<(), CallError> {
        let _: Value = op(self.0, "grids.delete", json!({ "grid": grid.to_string() }))?;
        Ok(())
    }

    /// Gives the members of one of the app's groups `permissions` on `grid`. Scope
    /// `grids.write`.
    pub fn assign_group(&self, grid: u64, group: u64, permissions: &[&str]) -> Result<(), CallError> {
        let _: Value =
            op(self.0, "grids.assign_group", json!({ "grid": grid.to_string(), "group": group.to_string(), "permissions": permissions }))?;
        Ok(())
    }

    /// Takes back what [`Grids::assign_group`] gave the group on `grid`. Scope `grids.write`.
    pub fn unassign_group(&self, grid: u64, group: u64) -> Result<(), CallError> {
        let _: Value = op(self.0, "grids.unassign_group", json!({ "grid": grid.to_string(), "group": group.to_string() }))?;
        Ok(())
    }

    /// Replaces the permission keys every player holds on `grid` ([`Grid::open_permissions`]).
    /// Scope `grids.write`.
    pub fn set_open_permissions(&self, grid: u64, permissions: &[&str]) -> Result<(), CallError> {
        let _: Value = op(self.0, "grids.set_open_permissions", json!({ "grid": grid.to_string(), "permissions": permissions }))?;
        Ok(())
    }

    fn grid_op(&self, body: Value) -> Result<Option<Grid>, CallError> {
        #[derive(Deserialize)]
        struct R {
            grid: Option<WireGrid>,
        }
        let r: R = op(self.0, "grids.get", body)?;
        Ok(r.grid.map(Grid::from))
    }
}

/// Player lookups and app state: [`Ctx::players`].
pub struct Players<'a>(pub(crate) &'a Ctx);

impl Players<'_> {
    /// A player's public identity, for players currently in this app. Scope `players.read`.
    pub fn get(&self, player: u64) -> Result<Player, CallError> {
        op(self.0, "players.get", serde_json::to_value(PlayerReq { player }).map_err(|e| bad_request(e.to_string()))?)
    }

    /// The feature keys the player's access tier grants in this app. Scope `players.read`.
    pub fn features(&self, player: u64) -> Result<Vec<String>, CallError> {
        #[derive(Deserialize)]
        struct R {
            features: Vec<String>,
        }
        let r: R = op(self.0, "players.features", json!({ "player": player.to_string() }))?;
        Ok(r.features)
    }

    /// Whether the player is an active member of one of this app's groups (teams, channels), or
    /// holds `permission` in it when one is named. Scope `players.read`.
    pub fn check_permission(&self, player: u64, group: u64, permission: Option<&str>) -> Result<bool, CallError> {
        #[derive(Deserialize)]
        struct R {
            allowed: bool,
        }
        let r: R = op(
            self.0,
            "players.check_permission",
            json!({ "player": player.to_string(), "group": group.to_string(), "permission": permission }),
        )?;
        Ok(r.allowed)
    }

    /// The player's state blob for this app, which game clients read too. Scope `players.read`.
    pub fn state(&self, player: u64) -> Result<Option<Vec<u8>>, CallError> {
        self.state_op("players.state", json!({ "player": player.to_string() }))
    }

    /// Replaces the player's state blob for this app (at most 256 KiB; `None` clears it). The
    /// player must have access to the app. Scope `players.write`.
    pub fn set_state(&self, player: u64, state: Option<&[u8]>) -> Result<(), CallError> {
        let _: Value = op(self.0, "players.set_state", json!({ "player": player.to_string(), "stateBase64": state.map(b64) }))?;
        Ok(())
    }

    /// An avatar's state blob for this app. Scope `players.read`.
    pub fn avatar_state(&self, avatar: u64) -> Result<Option<Vec<u8>>, CallError> {
        self.state_op("players.avatar_state", json!({ "avatar": avatar.to_string() }))
    }

    fn state_op(&self, name: &str, body: Value) -> Result<Option<Vec<u8>>, CallError> {
        #[derive(Deserialize)]
        struct R {
            state: Option<WireState>,
        }
        let r: R = op(self.0, name, body)?;
        Ok(r.state.and_then(|s| s.state_base64))
    }
}

/// Grid permission grants: [`Ctx::permissions`].
pub struct Permissions<'a>(pub(crate) &'a Ctx);

impl Permissions<'_> {
    /// Grants runtime permission keys on one of this app's grids to a player with access to the
    /// app, for `ttl_secs` or until revoked. Returns when the grant expires (RFC 3339), if it
    /// does. Scope `permissions.write`.
    pub fn grant(&self, grid: u64, player: u64, keys: &[&str], ttl_secs: Option<u32>) -> Result<Option<String>, CallError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct R {
            expires_at: Option<String>,
        }
        let r: R = op(
            self.0,
            "permissions.grant",
            json!({ "grid": grid.to_string(), "player": player.to_string(), "permissions": keys, "ttlSeconds": ttl_secs }),
        )?;
        Ok(r.expires_at)
    }

    /// Revokes the named keys, or every key when `keys` is `None`. Scope `permissions.write`.
    pub fn revoke(&self, grid: u64, player: u64, keys: Option<&[&str]>) -> Result<(), CallError> {
        let _: Value =
            op(self.0, "permissions.revoke", json!({ "grid": grid.to_string(), "player": player.to_string(), "permissions": keys }))?;
        Ok(())
    }
}

/// Players' access to this app: [`Ctx::access`].
pub struct Access<'a>(pub(crate) &'a Ctx);

impl Access<'_> {
    /// The player's access to the app, or `None` when they never had any. Scope `access.read`.
    pub fn get(&self, player: u64) -> Result<Option<PlayerAccess>, CallError> {
        #[derive(Deserialize)]
        struct R {
            access: Option<PlayerAccess>,
        }
        let r: R = op(self.0, "access.get", json!({ "player": player.to_string() }))?;
        Ok(r.access)
    }

    /// Gives the player access to the app, in access tier `tier` when one is named. Scope
    /// `access.write`.
    pub fn grant(&self, player: u64, tier: Option<u64>) -> Result<PlayerAccess, CallError> {
        #[derive(Deserialize)]
        struct R {
            access: PlayerAccess,
        }
        let mut body = json!({ "player": player.to_string() });
        if let Some(tier) = tier {
            body["tier"] = json!(tier.to_string());
        }
        let r: R = op(self.0, "access.grant", body)?;
        Ok(r.access)
    }

    /// Takes the player's access away. Scope `access.write`.
    pub fn revoke(&self, player: u64) -> Result<(), CallError> {
        let _: Value = op(self.0, "access.revoke", json!({ "player": player.to_string() }))?;
        Ok(())
    }

    /// Suspends the player's access for `seconds`. Returns until when (RFC 3339). Scope
    /// `access.write`.
    pub fn suspend(&self, player: u64, seconds: u32) -> Result<String, CallError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct R {
            suspended_until: String,
        }
        let r: R = op(self.0, "access.suspend", json!({ "player": player.to_string(), "seconds": seconds }))?;
        Ok(r.suspended_until)
    }

    /// Ends the player's suspension. Scope `access.write`.
    pub fn unsuspend(&self, player: u64) -> Result<(), CallError> {
        let _: Value = op(self.0, "access.unsuspend", json!({ "player": player.to_string() }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;
    use crate::Kind;
    use crate::abi::native::{self, Host};

    /// The game API as the tests stand it in: every operation sent, with its body, and the
    /// replies to give, in order.
    #[derive(Default)]
    struct FakeNode {
        sent: Vec<(String, Value)>,
        replies: VecDeque<Value>,
    }

    struct Shared(Rc<RefCell<FakeNode>>);

    impl Host for Shared {
        fn node(&mut self, op: &str, payload: &[u8], _deadline_ms: u32) -> Vec<u8> {
            let mut n = self.0.borrow_mut();
            n.sent.push((op.to_owned(), serde_json::from_slice(payload).expect("a JSON body")));
            let reply = n.replies.pop_front().expect("a reply for every operation");
            [&[0][..], &serde_json::to_vec(&reply).unwrap()].concat()
        }
    }

    /// Runs `f` against the game API `node` stands in for.
    fn with_node<T>(node: FakeNode, f: impl FnOnce(&Ctx) -> T) -> (T, FakeNode) {
        let node = Rc::new(RefCell::new(node));
        native::install(Some(Box::new(Shared(node.clone()))));
        let ctx = Ctx { app: 7, instance: 1, epoch: 1, version: 1, kind: Kind::Hub, node_type: "t".into(), key: String::new() };
        let out = f(&ctx);
        native::install(None);
        (out, Rc::try_unwrap(node).ok().expect("the host is gone").into_inner())
    }

    /// Runs `f`, the game API answering its one operation with `reply`: what `f` returned, and
    /// the operation and body it sent.
    fn exchange<T>(reply: Value, f: impl FnOnce(&Ctx) -> T) -> (T, String, Value) {
        let (out, mut node) = with_node(FakeNode { sent: Vec::new(), replies: VecDeque::from([reply]) }, f);
        let (name, body) = node.sent.pop().expect("one operation sent");
        (out, name, body)
    }

    /// `f` is refused before anything is sent.
    fn refused_here<T: std::fmt::Debug>(f: impl FnOnce(&Ctx) -> Result<T, CallError>) -> CallError {
        let (out, node) = with_node(FakeNode::default(), f);
        assert!(node.sent.is_empty(), "nothing sent");
        out.expect_err("refused")
    }

    fn pos(x: i64, y: i64, z: i64) -> Value {
        json!({ "x": x.to_string(), "y": y.to_string(), "z": z.to_string() })
    }

    #[test]
    fn reads_ids_as_strings_or_numbers() {
        let g: WireGrid = serde_json::from_value(json!({
            "gridId": "12", "low": { "x": "-5", "y": 0, "z": "9223372036854775807" }, "high": { "x": 1, "y": 2, "z": 3 }, "createdAt": "x"
        }))
        .unwrap();
        assert_eq!(g.grid_id, 12);
        assert_eq!(ChunkPos::from(g.low), ChunkPos::new(-5, 0, i64::MAX));
        let a: WireActor = serde_json::from_value(json!({
            "uuid": "a", "uuidHex": "61", "userId": null, "stateBase64": "AQI=", "chunkX": "1", "chunkY": "2", "chunkZ": "-3", "updatedAt": 0
        }))
        .unwrap();
        assert_eq!(a.user_id, None);
        assert_eq!(a.state_base64, Some(vec![1, 2]));
        let c: WireChunk = serde_json::from_value(json!({
            "chunkId": "7", "x": "1", "y": "2", "z": "3", "stateBase64": null, "chunkStateBase64": "AA==", "updatedAt": 0
        }))
        .unwrap();
        assert_eq!((c.chunk_id, c.chunk_state_base64), (7, Some(vec![0])));
    }

    #[test]
    fn sends_coordinates_as_decimal_strings() {
        assert_eq!(ChunkPos::new(i64::MIN, 0, 1).json(), json!({ "x": "-9223372036854775808", "y": "0", "z": "1" }));
        let grid = Grid { id: 1, low: ChunkPos::new(0, 0, 0), high: ChunkPos::new(3, 3, 3), owner: None, open_permissions: vec![] };
        assert!(grid.contains(ChunkPos::new(3, 0, 2)));
    }

    #[test]
    fn a_player_without_a_disambiguation_reads() {
        let reply = json!({ "player": "42", "gamertag": "Ada", "disambiguation": null });
        let (got, name, body) = exchange(reply, |c| c.players().get(42));
        assert_eq!((name.as_str(), body), ("players.get", json!({ "player": "42" })));
        assert_eq!(got.unwrap(), Player { player: 42, gamertag: "Ada".into(), disambiguation: None });
        let (got, ..) = exchange(json!({ "player": 42, "gamertag": "Ada" }), |c| c.players().get(42));
        assert_eq!(got.unwrap().disambiguation, None, "a platform that leaves it out");
        let (got, ..) = exchange(json!({ "player": "42", "gamertag": "Ada", "disambiguation": "0001" }), |c| c.players().get(42));
        assert_eq!(got.unwrap().disambiguation.as_deref(), Some("0001"));
    }

    #[test]
    fn voxels_are_signed_and_name_their_author_and_time() {
        let reply = json!({ "voxels": [
            { "voxelX": 1, "voxelY": 2, "voxelZ": 3, "voxelType": -300, "stateBase64": "Bw==", "createdBy": "42",
              "updatedAt": "2026-10-09T12:00:00.000Z" },
            { "voxelX": 0, "voxelY": 0, "voxelZ": 15, "voxelType": 32767, "stateBase64": null, "createdBy": "0", "updatedAt": "t" },
            { "voxelX": 4, "voxelY": 4, "voxelZ": 4, "voxelType": 9 },
        ] });
        let (got, name, body) = exchange(reply, |c| c.world().voxels(ChunkPos::new(1, -2, 3)));
        assert_eq!((name.as_str(), body), ("world.voxels", pos(1, -2, 3)));
        let got = got.unwrap();
        assert_eq!(
            got[0],
            Voxel {
                x: 1,
                y: 2,
                z: 3,
                voxel_type: -300,
                state: Some(vec![7]),
                created_by: Some("42".into()),
                updated_at: Some("2026-10-09T12:00:00.000Z".into()),
            }
        );
        assert_eq!((got[1].voxel_type, got[1].created_by.as_deref()), (i16::MAX, Some("0")), "\"0\" is the app's own code");
        assert_eq!((got[2].created_by.clone(), got[2].updated_at.clone()), (None, None), "an older platform's voxel still reads");
        let v: WireVoxel =
            serde_json::from_value(json!({ "voxelX": 0, "voxelY": 0, "voxelZ": 0, "voxelType": 1, "createdBy": 42, "updatedAt": 5 }))
                .unwrap();
        assert_eq!((v.created_by.as_deref(), v.updated_at.as_deref()), (Some("42"), Some("5")), "numbers are kept as text");
    }

    #[test]
    fn voxel_writes_carry_signed_types() {
        let w = VoxelWrite { chunk: ChunkPos::new(0, 1, 2), voxel: (1, 2, 3), voxel_type: -2, state: Some(vec![7]) };
        let (got, name, body) = exchange(json!({ "voxelUpdateIds": ["5"] }), |c| c.world().set_voxels(std::slice::from_ref(&w)));
        assert_eq!(got.unwrap(), vec![5]);
        assert_eq!(name, "world.set_voxels");
        assert_eq!(
            body,
            json!({ "writes": [{ "chunk": pos(0, 1, 2), "voxel": { "x": 1, "y": 2, "z": 3 }, "voxelType": -2, "stateBase64": "Bw==" }] })
        );
        let big = VoxelWrite { state: Some(vec![0; MAX_VOXEL_STATE + 1]), ..w };
        assert_eq!(refused_here(|c| c.world().set_voxels(&[big])).status, Status::BadRequest);
    }

    #[test]
    fn a_chunk_state_is_set_or_cleared_within_its_cap() {
        let (got, name, body) = exchange(json!({ "updatedAt": "2026-10-09T12:00:00.000Z" }), |c| {
            c.world().set_chunk_state(ChunkPos::new(-1, 0, 9), Some(b"\x01\x02"))
        });
        got.unwrap();
        assert_eq!((name.as_str(), body), ("world.set_chunk_state", json!({ "chunk": pos(-1, 0, 9), "stateBase64": "AQI=" })));
        let (got, _, body) = exchange(json!({ "updatedAt": null }), |c| c.world().set_chunk_state(ChunkPos::new(0, 0, 0), None));
        got.unwrap();
        assert_eq!(body, json!({ "chunk": pos(0, 0, 0), "stateBase64": null }), "null clears it");
        let (got, ..) =
            exchange(json!({ "updatedAt": "t" }), |c| c.world().set_chunk_state(ChunkPos::default(), Some(&[0; MAX_CHUNK_STATE])));
        assert!(got.is_ok(), "the cap itself is allowed");
        let e = refused_here(|c| c.world().set_chunk_state(ChunkPos::default(), Some(&[0; MAX_CHUNK_STATE + 1])));
        assert_eq!(e.status, Status::BadRequest);
    }

    #[test]
    fn a_grid_names_its_owner_and_open_permissions() {
        let reply = json!({ "grid": {
            "gridId": "12", "low": pos(0, 0, 0), "high": pos(3, 1, 3), "owner": { "kind": "guild", "ref": "88" },
            "openPermissions": ["enter", "use"], "createdAt": "t"
        } });
        let (got, name, body) = exchange(reply, |c| c.grids().get(12));
        assert_eq!((name.as_str(), body), ("grids.get", json!({ "grid": "12" })));
        let g = got.unwrap().unwrap();
        assert_eq!(g.owner, Some(GridOwner { kind: "guild".into(), ref_: "88".into() }));
        assert_eq!(g.open_permissions, vec!["enter".to_string(), "use".to_string()]);
        assert_eq!(serde_json::to_value(g.owner.unwrap()).unwrap(), json!({ "kind": "guild", "ref": "88" }));
        let (got, ..) = exchange(json!({ "grid": { "gridId": 1, "low": pos(0, 0, 0), "high": pos(0, 0, 0) } }), |c| c.grids().get(1));
        let g = got.unwrap().unwrap();
        assert_eq!((g.owner, g.open_permissions), (None, vec![]), "an older platform's grid still reads");
        let (got, ..) = exchange(json!({ "grid": { "gridId": 1, "low": pos(0, 0, 0), "high": pos(0, 0, 0), "owner": null } }), |c| {
            c.grids().at(ChunkPos::default(), GridPick::First)
        });
        assert_eq!(got.unwrap().unwrap().owner, None);
    }

    #[test]
    fn grid_settings_and_writes_send_what_the_node_api_takes() {
        let (got, name, body) = exchange(json!({ "wildernessWritesOpen": true }), |c| c.grids().settings());
        assert_eq!((got.unwrap(), name.as_str(), body), (GridSettings { wilderness_writes_open: true }, "grids.settings", json!({})));

        let created =
            json!({ "grid": { "gridId": "77", "low": pos(-2, 0, -2), "high": pos(2, 1, 2), "owner": null, "openPermissions": [] } });
        let (got, name, body) = exchange(created, |c| c.grids().create(ChunkPos::new(-2, 0, -2), ChunkPos::new(2, 1, 2)));
        let g = got.unwrap();
        assert_eq!((g.id, g.low, g.high), (77, ChunkPos::new(-2, 0, -2), ChunkPos::new(2, 1, 2)));
        assert_eq!((name.as_str(), body), ("grids.create", json!({ "low": pos(-2, 0, -2), "high": pos(2, 1, 2) })));

        let (got, name, body) = exchange(json!({}), |c| c.grids().delete(u64::MAX));
        got.unwrap();
        assert_eq!((name.as_str(), body), ("grids.delete", json!({ "grid": "18446744073709551615" })));

        let (got, name, body) = exchange(json!({}), |c| c.grids().assign_group(77, 5, &["build", "enter"]));
        got.unwrap();
        assert_eq!((name.as_str(), body), ("grids.assign_group", json!({ "grid": "77", "group": "5", "permissions": ["build", "enter"] })));

        let (got, name, body) = exchange(json!({}), |c| c.grids().unassign_group(77, 5));
        got.unwrap();
        assert_eq!((name.as_str(), body), ("grids.unassign_group", json!({ "grid": "77", "group": "5" })));

        let (got, name, body) = exchange(json!({}), |c| c.grids().set_open_permissions(77, &["enter"]));
        got.unwrap();
        assert_eq!((name.as_str(), body), ("grids.set_open_permissions", json!({ "grid": "77", "permissions": ["enter"] })));
        let (got, _, body) = exchange(json!({}), |c| c.grids().set_open_permissions(77, &[]));
        got.unwrap();
        assert_eq!(body, json!({ "grid": "77", "permissions": [] }), "none clears them");
    }

    #[test]
    fn access_is_read_granted_revoked_and_suspended() {
        let active = json!({ "status": "active", "tierId": "3", "expiresAt": null, "suspendedUntil": null });
        let (got, name, body) = exchange(json!({ "access": active }), |c| c.access().get(42));
        assert_eq!((name.as_str(), body), ("access.get", json!({ "player": "42" })));
        assert_eq!(got.unwrap(), Some(PlayerAccess { status: "active".into(), tier: Some(3), expires_at: None, suspended_until: None }));
        let (got, ..) = exchange(json!({ "access": null }), |c| c.access().get(42));
        assert_eq!(got.unwrap(), None, "never had any");
        let suspended = json!({ "access": { "status": "active", "tierId": null, "expiresAt": "2027-01-01T00:00:00.000Z",
            "suspendedUntil": "2026-10-10T00:00:00.000Z" } });
        let (got, ..) = exchange(suspended, |c| c.access().get(42));
        let a = got.unwrap().unwrap();
        assert_eq!(
            (a.tier, a.expires_at.as_deref(), a.suspended_until.as_deref()),
            (None, Some("2027-01-01T00:00:00.000Z"), Some("2026-10-10T00:00:00.000Z"))
        );

        let granted = json!({ "access": { "status": "active", "tierId": 9, "expiresAt": null, "suspendedUntil": null } });
        let (got, name, body) = exchange(granted.clone(), |c| c.access().grant(42, Some(9)));
        assert_eq!(got.unwrap().tier, Some(9));
        assert_eq!((name.as_str(), body), ("access.grant", json!({ "player": "42", "tier": "9" })));
        let (got, _, body) = exchange(granted, |c| c.access().grant(42, None));
        got.unwrap();
        assert_eq!(body, json!({ "player": "42" }), "no tier, none named");

        let (got, name, body) = exchange(json!({}), |c| c.access().revoke(42));
        got.unwrap();
        assert_eq!((name.as_str(), body), ("access.revoke", json!({ "player": "42" })));

        let (got, name, body) = exchange(json!({ "suspendedUntil": "2026-10-10T00:00:00.000Z" }), |c| c.access().suspend(42, 3_600));
        assert_eq!(got.unwrap(), "2026-10-10T00:00:00.000Z");
        assert_eq!((name.as_str(), body), ("access.suspend", json!({ "player": "42", "seconds": 3600 })));

        let (got, name, body) = exchange(json!({}), |c| c.access().unsuspend(42));
        got.unwrap();
        assert_eq!((name.as_str(), body), ("access.unsuspend", json!({ "player": "42" })));
    }

    #[test]
    fn a_reply_that_does_not_read_is_internal() {
        let (got, ..) = exchange(json!({ "suspended": true }), |c| c.access().suspend(42, 1));
        let e = got.unwrap_err();
        assert_eq!(e.status, Status::Internal);
        assert!(e.message.contains("access.suspend"), "{}", e.message);
    }
}
