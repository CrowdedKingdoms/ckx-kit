//! `crowdy-client-sdk`: the guest SDK for the CLIENT half of a ck-exec mod.
//!
//! A CLIENT half runs in the browser of each player who visits the mod's grid and consents to
//! it: a HUD, an overlay, input handling. The game API builds it for `wasm32-unknown-unknown`
//! (`execModClientBuild`), attaches it to the mod (`execModClientDeploy`) and serves it to
//! visitors (`execModClientArtifact`); CrowdyJS's player broker runs it in a Web Worker and
//! answers its host calls through the grid-confined allowlist.
//!
//! ABI v0 is the one the browser glue implements. The module imports `ck::{log, now_ms,
//! state_get, state_set, host_call}`, and `wasi_snapshot_preview1::random_get` only when it
//! calls [`random_bytes`]. It exports `ck_alloc`, `ck_free` and, through [`register_module!`],
//! `init`, `tick(dt_ms)`, `handle_invoke(ptr, len) -> u64` and optionally `on_event(ptr, len)`.
//! Buffers cross as `(ptr, len)` in guest memory; `handle_invoke` and `host_call` return a
//! packed `ptr << 32 | len` whose buffer the receiver releases with `ck_free`.
//!
//! [`state_get`] / [`state_set`] hold one blob for the life of the worker; nothing persists it.
//! Every dispatch runs on a fuel budget the platform injects at build time, so a runaway
//! `tick` traps instead of hanging the page.

pub const ABI_VERSION: u32 = 0;

/// Host imports (module `ck`).
#[cfg(target_arch = "wasm32")]
pub mod host {
    #[link(wasm_import_module = "ck")]
    extern "C" {
        /// level: 0 debug, 1 info, 2 warn, 3 error.
        pub fn log(level: u32, ptr: *const u8, len: u32);
        /// Milliseconds since the Unix epoch, from the browser's clock.
        pub fn now_ms() -> u64;
        /// Copies up to `cap` bytes of the state blob into `dest` and returns the blob's full
        /// length (call with `cap = 0` to size it).
        pub fn state_get(dest: *mut u8, cap: u32) -> u32;
        /// Replaces the state blob. Returns 0.
        pub fn state_set(ptr: *const u8, len: u32) -> u32;
        /// The host-call gateway: a JSON request in guest memory, answered synchronously with
        /// a packed (resp_ptr << 32 | resp_len) JSON response that the guest frees.
        pub fn host_call(ptr: *const u8, len: u32) -> u64;
    }
}

/// Native stand-in for the host, so a crate built on the SDK can `cargo test` without a
/// browser: it delegates to the thread's [`testhost`]. Absent from wasm builds.
#[cfg(not(target_arch = "wasm32"))]
pub mod host {
    use crate::testhost;

    pub unsafe fn log(level: u32, ptr: *const u8, len: u32) {
        let msg = String::from_utf8_lossy(core::slice::from_raw_parts(ptr, len as usize))
            .into_owned();
        testhost::with(|m| m.logs.push((level, msg)));
    }

    pub unsafe fn now_ms() -> u64 {
        testhost::with(|m| m.now_ms)
    }

    pub unsafe fn state_get(dest: *mut u8, cap: u32) -> u32 {
        testhost::with(|m| {
            let len = m.state.len() as u32;
            if !dest.is_null() && cap > 0 {
                let n = len.min(cap) as usize;
                core::ptr::copy_nonoverlapping(m.state.as_ptr(), dest, n);
            }
            len
        })
    }

    pub unsafe fn state_set(ptr: *const u8, len: u32) -> u32 {
        let bytes = core::slice::from_raw_parts(ptr, len as usize).to_vec();
        testhost::with(|m| m.state = bytes);
        0
    }
}

/// A scriptable fake host for native `cargo test`: install a handler for host calls, set the
/// clock, and read the logs and state. Thread-local, so parallel tests are isolated. Not
/// compiled for wasm.
#[cfg(not(target_arch = "wasm32"))]
pub mod testhost {
    use std::cell::RefCell;

    type HostCallFn = Box<
        dyn FnMut(&str, &serde_json::Value) -> Result<serde_json::Value, (String, Option<String>)>,
    >;

    pub struct MockHost {
        pub now_ms: u64,
        pub state: Vec<u8>,
        pub logs: Vec<(u32, String)>,
        pub rng_seed: u64,
        pub on_host_call: Option<HostCallFn>,
    }

    impl Default for MockHost {
        fn default() -> Self {
            Self { now_ms: 0, state: Vec::new(), logs: Vec::new(), rng_seed: 1, on_host_call: None }
        }
    }

    thread_local! {
        static MOCK: RefCell<MockHost> = RefCell::new(MockHost::default());
    }

    /// Installs a fresh mock (call at the start of each test).
    pub fn install(mock: MockHost) {
        MOCK.with(|cell| *cell.borrow_mut() = mock);
    }

    /// Resets to the defaults.
    pub fn reset() {
        install(MockHost::default());
    }

    /// The current thread's mock.
    pub fn with<R>(f: impl FnOnce(&mut MockHost) -> R) -> R {
        MOCK.with(|cell| f(&mut cell.borrow_mut()))
    }

    pub(crate) fn dispatch(request: &[u8]) -> Vec<u8> {
        #[derive(serde::Deserialize)]
        struct Req {
            #[serde(rename = "fn")]
            fn_name: String,
            #[serde(default)]
            args: serde_json::Value,
        }
        let outcome = match serde_json::from_slice::<Req>(request) {
            Ok(req) => {
                // Taken out while it runs, so the handler may use testhost::with itself.
                let mut handler = with(|m| m.on_host_call.take());
                let result = match handler.as_mut() {
                    Some(h) => h(&req.fn_name, &req.args),
                    None => Err(("no_mock_handler".into(), Some(format!("fn {}", req.fn_name)))),
                };
                if let Some(h) = handler {
                    with(|m| {
                        if m.on_host_call.is_none() {
                            m.on_host_call = Some(h);
                        }
                    });
                }
                result
            }
            Err(e) => Err(("bad_request".into(), Some(e.to_string()))),
        };
        let body = match outcome {
            Ok(data) => serde_json::json!({ "ok": true, "data": data }),
            Err((kind, message)) => {
                serde_json::json!({ "ok": false, "error": { "kind": kind, "message": message } })
            }
        };
        serde_json::to_vec(&body).expect("serialize mock response")
    }

    /// Deterministic bytes for the native `random_bytes` (xorshift64*).
    pub(crate) fn random(len: usize) -> Vec<u8> {
        with(|m| {
            let mut out = Vec::with_capacity(len);
            let mut s = m.rng_seed.max(1);
            while out.len() < len {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                let v = s.wrapping_mul(0x2545F4914F6CDD1D);
                out.extend_from_slice(&v.to_le_bytes());
            }
            m.rng_seed = s;
            out.truncate(len);
            out
        })
    }
}

pub fn log(level: u32, msg: &str) {
    unsafe { host::log(level, msg.as_ptr(), msg.len() as u32) }
}

pub fn now_ms() -> u64 {
    unsafe { host::now_ms() }
}

/// The state blob (empty when unset).
pub fn state_get() -> Vec<u8> {
    let len = unsafe { host::state_get(core::ptr::null_mut(), 0) };
    if len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; len as usize];
    let written = unsafe { host::state_get(buf.as_mut_ptr(), len) };
    buf.truncate(written.min(len) as usize);
    buf
}

/// Replaces the state blob.
pub fn state_set(bytes: &[u8]) -> bool {
    unsafe { host::state_set(bytes.as_ptr(), bytes.len() as u32) == 0 }
}

/// Random bytes from the browser's `crypto.getRandomValues`.
#[cfg(target_arch = "wasm32")]
pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    unsafe {
        wasi_random_get(buf.as_mut_ptr(), len as u32);
    }
    buf
}

/// Native stand-in: deterministic bytes from the test host's seed.
#[cfg(not(target_arch = "wasm32"))]
pub fn random_bytes(len: usize) -> Vec<u8> {
    testhost::random(len)
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    #[link_name = "random_get"]
    fn wasi_random_get(ptr: *mut u8, len: u32) -> u16;
}

/// Allocates a buffer the host writes into before it calls an export.
#[no_mangle]
pub extern "C" fn ck_alloc(len: u32) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr
}

/// Releases a buffer handed across the boundary.
///
/// # Safety
/// `ptr`/`len` must come from `ck_alloc` or a packed return value.
#[no_mangle]
pub unsafe extern "C" fn ck_free(ptr: *mut u8, len: u32) {
    drop(Vec::from_raw_parts(ptr, len as usize, len as usize));
}

/// Leaks `bytes` and packs its (ptr, len) into the u64 return convention.
pub fn pack_return(bytes: Vec<u8>) -> u64 {
    let len = bytes.len() as u64;
    let mut boxed = bytes.into_boxed_slice();
    let ptr = boxed.as_mut_ptr() as u64;
    core::mem::forget(boxed);
    (ptr << 32) | len
}

/// A host-provided buffer.
///
/// # Safety
/// `ptr`/`len` must describe a live buffer in guest memory.
pub unsafe fn input_slice<'a>(ptr: *const u8, len: u32) -> &'a [u8] {
    core::slice::from_raw_parts(ptr, len as usize)
}

/// A host call the broker or the page refused, or that failed.
///
/// The browser answers a refusal (outside the allowlist, outside the grid, over a rate cap,
/// not offered by this game) with `kind` `denied` and a message saying why.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HostError {
    pub kind: String,
    #[serde(default)]
    pub message: Option<String>,
}

impl core::fmt::Display for HostError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.message {
            Some(m) => write!(f, "{}: {}", self.kind, m),
            None => write!(f, "{}", self.kind),
        }
    }
}

/// On wasm the response crosses as a packed (ptr << 32 | len) buffer in 32-bit memory;
/// native tests route straight to the test host.
#[cfg(target_arch = "wasm32")]
fn host_call_raw(req: &[u8]) -> Vec<u8> {
    let packed = unsafe { host::host_call(req.as_ptr(), req.len() as u32) };
    let ptr = (packed >> 32) as u32 as *mut u8;
    let len = (packed & 0xffff_ffff) as u32;
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len as usize).to_vec() };
    unsafe { ck_free(ptr, len) };
    bytes
}

#[cfg(not(target_arch = "wasm32"))]
fn host_call_raw(req: &[u8]) -> Vec<u8> {
    testhost::dispatch(req)
}

/// Calls a host function with JSON args and returns its `data`.
pub fn host_call(fn_name: &str, args: serde_json::Value) -> Result<serde_json::Value, HostError> {
    #[derive(serde::Serialize)]
    struct Request<'a> {
        #[serde(rename = "fn")]
        fn_name: &'a str,
        args: serde_json::Value,
    }
    #[derive(serde::Deserialize)]
    struct Response {
        ok: bool,
        #[serde(default)]
        data: serde_json::Value,
        #[serde(default)]
        error: Option<HostError>,
    }

    let req = serde_json::to_vec(&Request { fn_name, args }).expect("serialize");
    let bytes = host_call_raw(&req);
    let resp: Response = serde_json::from_slice(&bytes)
        .map_err(|e| HostError { kind: "bad_response".into(), message: Some(e.to_string()) })?;
    if resp.ok {
        Ok(resp.data)
    } else {
        Err(resp.error.unwrap_or(HostError { kind: "unknown".into(), message: None }))
    }
}

/// The client host calls. The broker confines each to the mod's grid (chunk coordinates
/// inside the grid's box, `gridId` the mod's own) and the page answers it as the visiting
/// player, with that player's permissions; a game may leave some unanswered (`denied`).
/// Ids are strings where they may exceed 2^53.
pub mod api {
    use super::{host_call, HostError};
    use serde_json::{json, Value};

    // -- State ---------------------------------------------------------------------------

    /// The visiting player's app state.
    pub fn user_state_get(user_id: &str) -> Result<Value, HostError> {
        host_call("user_state_get", json!({ "userId": user_id }))
    }

    /// Replaces the visiting player's app state; `state_base64` is the new bytes.
    pub fn user_state_set(user_id: &str, state_base64: &str) -> Result<Value, HostError> {
        host_call("user_state_set", json!({ "userId": user_id, "stateBase64": state_base64 }))
    }

    /// An avatar's app state, for an avatar whose actor is inside the grid.
    pub fn avatar_state_get(avatar_id: &str) -> Result<Value, HostError> {
        host_call("avatar_state_get", json!({ "avatarId": avatar_id }))
    }

    // -- World -----------------------------------------------------------------------------

    /// One chunk inside the grid: `{ voxelsBase64 }`.
    pub fn chunk_get(x: i64, y: i64, z: i64) -> Result<Value, HostError> {
        host_call("chunk_get", json!({ "x": x, "y": y, "z": z }))
    }

    /// The voxels recorded in one chunk inside the grid.
    pub fn voxels_list(x: i64, y: i64, z: i64) -> Result<Value, HostError> {
        host_call("voxels_list", json!({ "x": x, "y": y, "z": z }))
    }

    /// The actors in one chunk inside the grid, as the page sees them.
    pub fn actors_list(x: i64, y: i64, z: i64) -> Result<Value, HostError> {
        host_call("actors_list", json!({ "x": x, "y": y, "z": z }))
    }

    /// The actors in a box of chunks around (x, y, z): `radius_xz` at most 3 and `radius_y`
    /// at most 1, clipped to the grid.
    pub fn actors_list_radius(
        x: i64,
        y: i64,
        z: i64,
        radius_xz: u8,
        radius_y: u8,
    ) -> Result<Value, HostError> {
        host_call(
            "actors_list_radius",
            json!({ "x": x, "y": y, "z": z, "radiusXz": radius_xz, "radiusY": radius_y }),
        )
    }

    /// Writes one voxel inside the grid, as the visiting player (their `update_voxel_data`).
    pub fn voxel_set(
        chunk: (i64, i64, i64),
        voxel: (u8, u8, u8),
        voxel_type: i32,
        state_base64: Option<&str>,
    ) -> Result<Value, HostError> {
        host_call(
            "voxel_set",
            json!({
                "chunkX": chunk.0, "chunkY": chunk.1, "chunkZ": chunk.2,
                "voxelX": voxel.0, "voxelY": voxel.1, "voxelZ": voxel.2,
                "voxelType": voxel_type, "stateBase64": state_base64,
            }),
        )
    }

    // -- Grid ------------------------------------------------------------------------------

    /// Whether the visiting player holds `permission_key` on this grid.
    pub fn grid_permission_check(
        user_id: &str,
        grid_id: &str,
        permission_key: &str,
    ) -> Result<bool, HostError> {
        let data = host_call(
            "grid_permission_check",
            json!({ "userId": user_id, "gridId": grid_id, "permissionKey": permission_key }),
        )?;
        Ok(data.as_bool().unwrap_or(false))
    }

    /// The mod's own grid, answered by the broker: `{ low: {x,y,z}, high: {x,y,z} }` in chunk
    /// coordinates, as decimal strings.
    pub fn grid_info() -> Result<Value, HostError> {
        host_call("grid_info", json!({}))
    }

    // -- Presentation and input ------------------------------------------------------------

    /// Hands `payload` to the game's HUD channel. The mod never touches the DOM; a game that
    /// offers no HUD answers `{ delivered: false }`.
    pub fn hud_set(payload: Value) -> Result<Value, HostError> {
        host_call("hud_set", json!({ "payload": payload }))
    }

    /// Hands `payload` to the game's overlay channel, as [`hud_set`] does.
    pub fn overlay_draw(payload: Value) -> Result<Value, HostError> {
        host_call("overlay_draw", json!({ "payload": payload }))
    }

    /// Drains the mouse clicks queued since the last call: `{ nowMs, buttons, holdingMs,
    /// clicks }`. `buttons` is `MouseEvent.buttons` (1 = left held); `holdingMs` maps a button
    /// index to how long it has been held (`"0"` = left); each click is `{ t: "down"|"up",
    /// button, atMs, heldMs?, nx, ny }` in canvas coordinates from -1 to 1 (+ny up). Call it
    /// every tick.
    pub fn pointer_clicks() -> Result<Value, HostError> {
        host_call("pointer_clicks", json!({}))
    }

    /// The visiting player's movement axes, drained by the page: `{ x, y }`.
    pub fn input_axes() -> Result<Value, HostError> {
        host_call("input_axes", json!({}))
    }

    /// Pointer-look delta since the last poll: `{ dx, dy }`.
    pub fn input_look() -> Result<Value, HostError> {
        host_call("input_look", json!({}))
    }

    /// Whether the visiting player is holding `code` (a `KeyboardEvent.code`).
    pub fn input_key(code: &str) -> Result<Value, HostError> {
        host_call("input_key", json!({ "code": code }))
    }

    /// The page's wall clock, milliseconds.
    pub fn clock() -> Result<Value, HostError> {
        host_call("clock", json!({}))
    }

    /// The visiting player's body. The page answers from the mod's hold, or the walker.
    pub fn pose_get() -> Result<Value, HostError> {
        host_call("pose_get", json!({}))
    }

    /// Moves the visiting player's body. A position outside the grid is refused.
    pub fn pose_set(pose: Value) -> Result<Value, HostError> {
        host_call("pose_set", pose)
    }

    /// Hands the visiting player's body back to the walker.
    pub fn pose_release() -> Result<Value, HostError> {
        host_call("pose_release", json!({}))
    }

    /// Asks the page to move the visiting player to a destination inside the grid.
    pub fn teleport_request(destination: Value) -> Result<Value, HostError> {
        host_call("teleport_request", destination)
    }

    /// Places a mod-owned actor inside the grid. A live player's id is refused.
    pub fn actor_spawn(actor: Value) -> Result<Value, HostError> {
        host_call("actor_spawn", actor)
    }

    /// Moves a mod-owned actor of this session. A position outside the grid is refused.
    pub fn actor_pose(actor_id: &str, pose: Value) -> Result<Value, HostError> {
        host_call("actor_pose", json!({ "actorId": actor_id, "pose": pose }))
    }

    /// Removes a mod-owned actor of this session.
    pub fn actor_despawn(actor_id: &str) -> Result<Value, HostError> {
        host_call("actor_despawn", json!({ "actorId": actor_id }))
    }

    /// The mod's scene templates, kept in the page.
    pub fn scene_catalog(catalog: Value) -> Result<Value, HostError> {
        host_call("scene_catalog", catalog)
    }

    /// Template instances drawn in the page, inside the grid.
    pub fn scene_instances(instances: Value) -> Result<Value, HostError> {
        host_call("scene_instances", instances)
    }

    /// The visiting player's avatar tint.
    pub fn avatar_appearance(appearance: Value) -> Result<Value, HostError> {
        host_call("avatar_appearance", appearance)
    }

    /// The visiting player's own avatar state blob.
    pub fn avatar_state_set(avatar_id: &str, state_base64: &str) -> Result<Value, HostError> {
        host_call(
            "avatar_state_set",
            json!({ "avatarId": avatar_id, "stateBase64": state_base64 }),
        )
    }

    /// Starts or stops the visiting player's microphone. The page still asks the browser.
    pub fn voice_set(enabled: bool) -> Result<Value, HostError> {
        host_call("voice_set", json!({ "enabled": enabled }))
    }

    /// Starts or stops the visiting player's camera. The page still asks the browser.
    pub fn video_set(enabled: bool) -> Result<Value, HostError> {
        host_call("video_set", json!({ "enabled": enabled }))
    }

    /// A ClientEvent from the visiting player's chunk, inside the grid.
    pub fn send_client_event(chunk: (i64, i64, i64), payload: Value) -> Result<Value, HostError> {
        host_call(
            "send_client_event",
            json!({ "chunkX": chunk.0, "chunkY": chunk.1, "chunkZ": chunk.2, "payload": payload }),
        )
    }

    /// ClientEvents the visiting player's session has received since the last poll.
    pub fn events_poll() -> Result<Value, HostError> {
        host_call("events_poll", json!({}))
    }

    /// A spatial text packet from the visiting player's chunk.
    pub fn send_text(chunk: (i64, i64, i64), text: &str) -> Result<Value, HostError> {
        host_call(
            "send_text",
            json!({ "chunkX": chunk.0, "chunkY": chunk.1, "chunkZ": chunk.2, "text": text }),
        )
    }

    /// A message from this session to one actor inside the grid.
    pub fn send_actor_message(actor_id: &str, payload: Value) -> Result<Value, HostError> {
        host_call("send_actor_message", json!({ "actorId": actor_id, "payload": payload }))
    }

    /// A channel message as the visiting player, on a channel they may send on.
    pub fn send_channel_message(channel_id: &str, payload: Value) -> Result<Value, HostError> {
        host_call(
            "send_channel_message",
            json!({ "channelId": channel_id, "payload": payload }),
        )
    }

    // -- Egress ----------------------------------------------------------------------------

    /// Sends a spatial message from a chunk inside the grid, as the visiting player's client.
    /// `kind` is `actor`, `client_event`, `server_event` or `text`; `uuid_hex` is 64 hex
    /// characters; `distance` is at most 8.
    pub fn emit_spatial(
        kind: &str,
        chunk: (i64, i64, i64),
        uuid_hex: &str,
        payload_base64: &str,
        distance: u8,
        decay: u8,
    ) -> Result<Value, HostError> {
        host_call(
            "emit_spatial",
            json!({
                "kind": kind,
                "chunkX": chunk.0, "chunkY": chunk.1, "chunkZ": chunk.2,
                "uuidHex": uuid_hex, "payloadBase64": payload_base64,
                "distance": distance, "decay": decay,
            }),
        )
    }

    /// Posts to one of the grid's channels.
    pub fn emit_channel(channel_id: &str, payload_base64: &str) -> Result<Value, HostError> {
        host_call("emit_channel", json!({ "channelId": channel_id, "payloadBase64": payload_base64 }))
    }

    /// Publishes an event on the page's grid event bus, which the other CLIENT mods of this
    /// grid on the same page receive through `on_event`. Nothing leaves the page.
    pub fn emit_event(name: &str, payload: Value) -> Result<Value, HostError> {
        host_call("emit_event", json!({ "name": name, "payload": payload }))
    }

    /// [`emit_event`] addressed to one mod on the bus by name, whether or not it listens for
    /// `name`.
    pub fn emit_event_to(target: &str, name: &str, payload: Value) -> Result<Value, HostError> {
        host_call("emit_event", json!({ "name": name, "payload": payload, "target": target }))
    }
}

/// Wires a CLIENT half's functions to the exports the browser calls: `init()`, `tick(dt_ms)`,
/// `handle_invoke(ptr, len) -> packed u64`, and, in the four-argument form, `on_event(ptr, len)`
/// for grid events from the page's bus. The three-argument form exports no `on_event`.
///
/// ```ignore
/// use crowdy_client_sdk as crowdy;
///
/// fn init() {}
/// fn tick(_dt_ms: u32) {
///     let _ = crowdy::api::hud_set(serde_json::json!({ "text": "hello" }));
/// }
/// fn invoke(payload: &[u8]) -> Vec<u8> { payload.to_vec() }
///
/// crowdy::register_module!(init: init, tick: tick, invoke: invoke);
/// ```
#[macro_export]
macro_rules! register_module {
    (init: $init:path, tick: $tick:path, invoke: $invoke:path $(,)?) => {
        $crate::__register_module_core!($init, $tick, $invoke);
    };
    (init: $init:path, tick: $tick:path, invoke: $invoke:path, event: $event:path $(,)?) => {
        $crate::__register_module_core!($init, $tick, $invoke);

        #[export_name = "on_event"]
        pub extern "C" fn __ck_export_on_event(ptr: *const u8, len: u32) {
            let input = unsafe { $crate::input_slice(ptr, len) };
            $event(input);
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __register_module_core {
    ($init:path, $tick:path, $invoke:path) => {
        #[export_name = "init"]
        pub extern "C" fn __ck_export_init() {
            $init();
        }

        #[export_name = "tick"]
        pub extern "C" fn __ck_export_tick(dt_ms: u32) {
            $tick(dt_ms);
        }

        #[export_name = "handle_invoke"]
        pub extern "C" fn __ck_export_handle_invoke(ptr: *const u8, len: u32) -> u64 {
            let input = unsafe { $crate::input_slice(ptr, len) };
            let out: Vec<u8> = $invoke(input);
            $crate::pack_return(out)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_call_round_trips_through_the_test_host() {
        testhost::install(testhost::MockHost {
            on_host_call: Some(Box::new(|name, args| {
                assert_eq!(name, "chunk_get");
                assert_eq!(args["x"], 3);
                Ok(serde_json::json!({ "voxelsBase64": null }))
            })),
            ..Default::default()
        });
        let data = api::chunk_get(3, 0, -2).expect("answered");
        assert!(data["voxelsBase64"].is_null());
    }

    #[test]
    fn a_refusal_is_a_host_error() {
        testhost::install(testhost::MockHost {
            on_host_call: Some(Box::new(|_, _| {
                Err(("denied".into(), Some("target is outside the player grid".into())))
            })),
            ..Default::default()
        });
        let err = api::voxel_set((99, 0, 0), (1, 2, 3), 7, None).unwrap_err();
        assert_eq!(err.kind, "denied");
        assert_eq!(err.to_string(), "denied: target is outside the player grid");
    }

    #[test]
    fn state_and_log_use_the_test_host() {
        testhost::reset();
        assert!(state_get().is_empty());
        assert!(state_set(b"hud"));
        assert_eq!(state_get(), b"hud");
        log(1, "ready");
        testhost::with(|m| assert_eq!(m.logs, vec![(1, "ready".to_string())]));
    }
}
