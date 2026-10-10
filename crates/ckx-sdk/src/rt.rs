//! Glue between the exports the macros generate and the developer's node type.

use std::alloc::{Layout, alloc as raw_alloc, dealloc};
use std::cell::UnsafeCell;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    Call, CallError, Caller, Ctx, Error, Hub, Kind, Presence, Result, Session, Spoke, Status, TopicMsg, WorldEvent, abi, decode, encode,
};

pub const ABI_VERSION: u32 = 5;

/// Timers one hub may have pending.
pub const MAX_TIMERS: usize = 1024;
/// Topics one hub may watch.
pub const MAX_WATCHES: usize = 256;
/// Shortest timer period.
pub const MIN_TIMER_MS: u64 = 10;

/// Starts a snapshot that carries the platform's part (timers, watches) before the hub's own
/// bytes. MessagePack never starts with 0xc1, and a snapshot without it is all the hub's.
const ENVELOPE: &[u8] = b"\xc1ckx2";

/// A guest is single threaded; the host never calls in while a call is running, except
/// `ckx_alloc`, which touches none of these.
struct GuestCell<T>(UnsafeCell<T>);

unsafe impl<T> Sync for GuestCell<T> {}

impl<T> GuestCell<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }

    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Timer {
    /// Unix milliseconds.
    at: u64,
    /// 0 for a one-shot timer.
    every: u64,
}

/// What the platform keeps for a hub beside its state, snapshotted with it.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Platform {
    #[serde(default)]
    timers: BTreeMap<String, Timer>,
    #[serde(default)]
    watches: BTreeSet<(String, String, String)>,
    /// The wake deadline the host was last given; `u64::MAX` when unknown.
    #[serde(skip)]
    wake: u64,
}

impl Platform {
    fn next_wake(&self) -> u64 {
        self.timers.values().map(|t| t.at).min().unwrap_or(0)
    }

    /// Timers due at `now`, earliest first. Each is rescheduled (or removed) before it runs,
    /// so its handler may replace or cancel it. A repeating timer that fell behind (the hub was
    /// stopped, or a handler ran long) runs once and continues one period from now.
    fn take_due(&mut self, now: u64) -> Vec<String> {
        let mut due: Vec<(u64, String)> = self.timers.iter().filter(|(_, t)| t.at <= now).map(|(n, t)| (t.at, n.clone())).collect();
        due.sort();
        for (_, name) in &due {
            let t = self.timers.get_mut(name).expect("listed above");
            if t.every == 0 {
                self.timers.remove(name);
            } else {
                t.at = if t.at + t.every > now { t.at + t.every } else { now + t.every };
            }
        }
        due.into_iter().map(|(_, n)| n).collect()
    }
}

static PLATFORM: GuestCell<Platform> = GuestCell::new(Platform { timers: BTreeMap::new(), watches: BTreeSet::new(), wake: 0 });

fn wrap(p: &Platform, state: &[u8]) -> Result<Vec<u8>> {
    let head = encode(p)?;
    let mut out = Vec::with_capacity(ENVELOPE.len() + 4 + head.len() + state.len());
    out.extend_from_slice(ENVELOPE);
    out.extend_from_slice(&(head.len() as u32).to_le_bytes());
    out.extend_from_slice(&head);
    out.extend_from_slice(state);
    Ok(out)
}

fn unwrap(snapshot: &[u8]) -> Result<(Platform, &[u8])> {
    let Some(rest) = snapshot.strip_prefix(ENVELOPE) else {
        return Ok((Platform::default(), snapshot));
    };
    let bad = || Error::new("malformed snapshot envelope");
    let len = u32::from_le_bytes(rest.get(..4).ok_or_else(bad)?.try_into().unwrap()) as usize;
    let head = rest.get(4..4 + len).ok_or_else(bad)?;
    Ok((decode(head)?, &rest[4 + len..]))
}

/// Tells the host about the earliest timer, if that changed since it was last told.
fn sync_wake() {
    let p = PLATFORM.get();
    let next = p.next_wake();
    if next != p.wake {
        p.wake = next;
        abi::wake_at(next);
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(Error::new("timer names are 1 to 64 bytes"));
    }
    Ok(())
}

pub(crate) fn set_timer(ctx: &Ctx, name: &str, delay_ms: u64, every_ms: u64) -> Result<()> {
    if ctx.kind != Kind::Hub {
        return Err(Error::new("timers are for hubs"));
    }
    check_name(name)?;
    let p = PLATFORM.get();
    if !p.timers.contains_key(name) && p.timers.len() >= MAX_TIMERS {
        return Err(Error(format!("a hub may have {MAX_TIMERS} timers")));
    }
    let every = if every_ms == 0 { 0 } else { every_ms.max(MIN_TIMER_MS) };
    let first = if every == 0 { delay_ms } else { delay_ms.max(MIN_TIMER_MS) };
    p.timers.insert(name.to_owned(), Timer { at: abi::now_ms() + first, every });
    sync_wake();
    Ok(())
}

pub(crate) fn cancel_timer(name: &str) -> bool {
    let removed = PLATFORM.get().timers.remove(name).is_some();
    sync_wake();
    removed
}

pub(crate) fn timers() -> Vec<(String, u64)> {
    PLATFORM.get().timers.iter().map(|(n, t)| (n.clone(), t.at)).collect()
}

pub(crate) fn watch(ctx: &Ctx, node_type: &str, key: &str, topic: &str, on: bool) -> Result<(), CallError> {
    let refuse = |message: &str| CallError { status: Status::BadRequest, message: message.into() };
    if ctx.kind != Kind::Hub {
        return Err(CallError { status: Status::Denied, message: "only hubs subscribe to topics".into() });
    }
    if topic.is_empty() || topic.len() > 255 || topic.starts_with('$') {
        return Err(refuse("topics are 1 to 255 bytes and do not start with `$`"));
    }
    let entry = (node_type.to_owned(), key.to_owned(), topic.to_owned());
    let p = PLATFORM.get();
    if on && !p.watches.contains(&entry) && p.watches.len() >= MAX_WATCHES {
        return Err(refuse(&format!("a hub may watch {MAX_WATCHES} topics")));
    }
    let result = import_result(abi::watch(&crate::request(node_type, key, topic, &[]), on)).map(|_| ());
    let p = PLATFORM.get();
    if on {
        if result.is_ok() {
            p.watches.insert(entry);
        }
    } else {
        p.watches.remove(&entry);
    }
    result
}

trait DynNode {
    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>>;
    fn persist(&mut self, ctx: &Ctx) -> Result<Vec<u8>>;
    fn evict(&mut self, ctx: &Ctx);
}

pub trait Node: Sized + 'static {
    fn init(ctx: &Ctx, seed: &[u8], snapshot: Option<(&[u8], u64)>) -> Result<Self>;
    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>>;
    fn persist(&mut self, ctx: &Ctx) -> Result<Vec<u8>>;
    fn evict(&mut self, ctx: &Ctx);
}

impl<N: Node> DynNode for N {
    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        Node::handle(self, ctx, call)
    }

    fn persist(&mut self, ctx: &Ctx) -> Result<Vec<u8>> {
        Node::persist(self, ctx)
    }

    fn evict(&mut self, ctx: &Ctx) {
        Node::evict(self, ctx)
    }
}

pub struct HubNode<T: Hub>(T);

/// `[type str8][key str16][topic str8][payload...]`
fn parse_topic(b: &[u8]) -> Result<TopicMsg<'_>> {
    let bad = || Error::new("malformed topic message");
    let str_at = |at: usize, len: usize| std::str::from_utf8(b.get(at..at + len).ok_or_else(bad)?).map_err(|_| bad());
    let tlen = *b.first().ok_or_else(bad)? as usize;
    let node_type = str_at(1, tlen)?;
    let at = 1 + tlen;
    let klen = u16::from_le_bytes(b.get(at..at + 2).ok_or_else(bad)?.try_into().unwrap()) as usize;
    let key = str_at(at + 2, klen)?;
    let at = at + 2 + klen;
    let tplen = *b.get(at).ok_or_else(bad)? as usize;
    let topic = str_at(at + 1, tplen)?;
    Ok(TopicMsg { node_type, key, topic, payload: &b[at + 1 + tplen..] })
}

impl<T: Hub> HubNode<T> {
    fn platform_call(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        use ckx_platform::*;
        match (call.method, call.caller) {
            (TIMER, Caller::Platform) => {
                // The host cleared its deadline to make this call; always give it the next one.
                PLATFORM.get().wake = u64::MAX;
                for name in PLATFORM.get().take_due(abi::now_ms()) {
                    if let Err(e) = self.0.on_timer(ctx, &name) {
                        abi::log(0, &format!("timer `{name}`: {e}"));
                    }
                }
            }
            (TOPIC, Caller::Platform) => self.0.on_topic(ctx, parse_topic(call.payload)?)?,
            (PRESENCE, Caller::Platform) => self.0.on_presence(ctx, &decode::<Presence>(call.payload)?)?,
            (WORLD, Caller::Platform) => {
                #[derive(Deserialize)]
                struct Batch {
                    events: Vec<WorldEvent>,
                }
                self.0.on_world(ctx, &decode::<Batch>(call.payload)?.events)?
            }
            (SESSION, Caller::Player(p)) => {
                let session = match call.payload {
                    [SESSION_JOINED] => Session::Joined,
                    [SESSION_LEFT] => Session::Left,
                    _ => return Err(Error::new("malformed session event")),
                };
                self.0.on_session(ctx, p, session)?
            }
            (m, c) => return Err(Error(format!("no platform event `{m}` from {c:?}"))),
        }
        Ok(Vec::new())
    }
}

/// Reserved method names, as `ckx_proto::platform` defines them.
mod ckx_platform {
    pub const TIMER: &str = "$timer";
    pub const TOPIC: &str = "$topic";
    pub const PRESENCE: &str = "$presence";
    pub const WORLD: &str = "$world";
    pub const SESSION: &str = "$session";
    pub const SESSION_JOINED: u8 = 1;
    pub const SESSION_LEFT: u8 = 0;
}

impl<T: Hub> Node for HubNode<T> {
    fn init(ctx: &Ctx, seed: &[u8], snapshot: Option<(&[u8], u64)>) -> Result<Self> {
        let hub = match snapshot {
            Some((bytes, version)) => {
                let (platform, state) = unwrap(bytes)?;
                *PLATFORM.get() = platform;
                T::load(ctx, state, version)?
            }
            None => {
                *PLATFORM.get() = Platform::default();
                T::spawn(ctx, seed)?
            }
        };
        let watches: Vec<(String, String, String)> = PLATFORM.get().watches.iter().cloned().collect();
        for (node_type, key, topic) in watches {
            let req = crate::request(&node_type, &key, &topic, &[]);
            if let Err(e) = import_result(abi::watch(&req, true)) {
                abi::log(1, &format!("could not watch `{topic}` on {node_type}/{key} again: {e}"));
            }
        }
        PLATFORM.get().wake = u64::MAX;
        sync_wake();
        Ok(Self(hub))
    }

    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        let out = if call.method.starts_with('$') { self.platform_call(ctx, call) } else { self.0.handle(ctx, call) };
        sync_wake();
        out
    }

    fn persist(&mut self, ctx: &Ctx) -> Result<Vec<u8>> {
        let state = self.0.persist(ctx)?;
        wrap(PLATFORM.get(), &state)
    }

    fn evict(&mut self, ctx: &Ctx) {
        self.0.evict(ctx)
    }
}

pub struct SpokeNode<T: Spoke>(T);

impl<T: Spoke> Node for SpokeNode<T> {
    fn init(ctx: &Ctx, _seed: &[u8], _snapshot: Option<(&[u8], u64)>) -> Result<Self> {
        Ok(Self(T::start(ctx)?))
    }

    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        if call.method.starts_with('$') {
            return Err(Error(format!("spokes receive no platform events (`{}`)", call.method)));
        }
        self.0.handle(ctx, call)
    }

    fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn evict(&mut self, _ctx: &Ctx) {}
}

static NODE: GuestCell<Option<(Ctx, Box<dyn DynNode>)>> = GuestCell::new(None);
static OUT: GuestCell<Vec<u8>> = GuestCell::new(Vec::new());

fn layout(len: u32) -> Layout {
    Layout::from_size_align(len as usize, 1).expect("valid layout")
}

pub fn alloc(len: u32) -> u32 {
    if len == 0 {
        return 1;
    }
    let ptr = unsafe { raw_alloc(layout(len)) };
    if ptr.is_null() {
        std::process::abort();
    }
    ptr as u32
}

fn free(ptr: u32, len: u32) {
    if len != 0 {
        unsafe { dealloc(ptr as *mut u8, layout(len)) }
    }
}

/// Copies a host-written buffer out and frees it.
fn take(ptr: u32, len: u32) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    let v = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }.to_vec();
    free(ptr, len);
    v
}

fn pack(ptr: u32, len: u32) -> u64 {
    ((ptr as u64) << 32) | len as u64
}

/// Writes `[status][payload]` into the reusable output buffer and returns it packed.
fn respond(result: Result<Vec<u8>>) -> u64 {
    let out = OUT.get();
    out.clear();
    match result {
        Ok(bytes) => {
            out.push(0);
            out.extend_from_slice(&bytes);
        }
        Err(Error(msg)) => {
            out.push(1);
            out.extend_from_slice(msg.as_bytes());
        }
    }
    pack(out.as_ptr() as u32, out.len() as u32)
}

/// Unpacks a result the host returned from an import.
pub(crate) fn import_result(packed: u64) -> Result<Vec<u8>, CallError> {
    let (ptr, len) = ((packed >> 32) as u32, packed as u32);
    if len == 0 {
        return Err(CallError { status: Status::Internal, message: "no reply from the host".into() });
    }
    let buf = take(ptr, len);
    match Status::from_u8(buf[0]) {
        Status::Ok => Ok(buf[1..].to_vec()),
        status => Err(CallError { status, message: String::from_utf8_lossy(&buf[1..]).into_owned() }),
    }
}

fn parse_ctx(b: &[u8]) -> Result<Ctx> {
    let bad = || Error::new("malformed instance context");
    let u64_at = |at: usize| -> Result<u64> { Ok(u64::from_le_bytes(b.get(at..at + 8).ok_or_else(bad)?.try_into().unwrap())) };
    let kind = match b.get(32).ok_or_else(bad)? {
        0 => Kind::Hub,
        _ => Kind::Spoke,
    };
    let tlen = *b.get(33).ok_or_else(bad)? as usize;
    let node_type = String::from_utf8(b.get(34..34 + tlen).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
    let at = 34 + tlen;
    let klen = u16::from_le_bytes(b.get(at..at + 2).ok_or_else(bad)?.try_into().unwrap()) as usize;
    let key = String::from_utf8(b.get(at + 2..at + 2 + klen).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
    Ok(Ctx { app: u64_at(0)?, instance: u64_at(8)?, epoch: u64_at(16)?, version: u64_at(24)?, kind, node_type, key })
}

#[allow(clippy::too_many_arguments)]
pub fn init<N: Node>(
    ctx_ptr: u32,
    ctx_len: u32,
    seed_ptr: u32,
    seed_len: u32,
    snap_ptr: u32,
    snap_len: u32,
    snap_version: u64,
    has_snap: u32,
) -> u64 {
    let ctx_bytes = take(ctx_ptr, ctx_len);
    let seed = take(seed_ptr, seed_len);
    let snap = take(snap_ptr, snap_len);
    let result = parse_ctx(&ctx_bytes).and_then(|ctx| {
        let snapshot = (has_snap != 0).then_some((snap.as_slice(), snap_version));
        let node = N::init(&ctx, &seed, snapshot)?;
        *NODE.get() = Some((ctx, Box::new(node)));
        Ok(Vec::new())
    });
    respond(result)
}

pub fn handle(caller_kind: u32, caller_id: u64, buf_ptr: u32, method_len: u32, payload_len: u32) -> u64 {
    let buf = take(buf_ptr, method_len + payload_len);
    let (method, payload) = buf.split_at(method_len as usize);
    let Ok(method) = std::str::from_utf8(method) else {
        return respond(Err(Error::new("method name is not UTF-8")));
    };
    let caller = match caller_kind {
        0 => Caller::Platform,
        1 => Caller::Instance(caller_id),
        2 => Caller::Player(caller_id),
        3 => Caller::Developer(caller_id),
        // Never the platform: a caller this SDK does not know is refused, not trusted.
        _ => return respond(Err(Error::new("unknown caller kind"))),
    };
    let Some((ctx, node)) = NODE.get().as_mut() else {
        return respond(Err(Error::new("instance not initialised")));
    };
    respond(node.handle(ctx, Call { caller, method, payload }))
}

pub fn persist() -> u64 {
    match NODE.get().as_mut() {
        Some((ctx, node)) => respond(node.persist(ctx)),
        None => respond(Err(Error::new("instance not initialised"))),
    }
}

pub fn evict() {
    if let Some((ctx, node)) = NODE.get().as_mut() {
        node.evict(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer(at: u64, every: u64) -> Timer {
        Timer { at, every }
    }

    #[test]
    fn the_envelope_carries_timers_and_watches_and_old_snapshots_pass_through() {
        let mut p = Platform::default();
        p.timers.insert("tick".into(), timer(1_000, 100));
        p.watches.insert(("arena".into(), "m1".into(), "hp".into()));
        let snap = wrap(&p, b"state").unwrap();
        let (back, state) = unwrap(&snap).unwrap();
        assert_eq!((back, state), (p, &b"state"[..]));

        let v1 = encode(&42u32).unwrap();
        let (none, state) = unwrap(&v1).unwrap();
        assert_eq!((none, state), (Platform::default(), &v1[..]));
        assert!(unwrap(b"\xc1ckx2\xff\xff\xff\xff").is_err());
    }

    #[test]
    fn due_timers_run_in_order_and_repeat_on_schedule() {
        let mut p = Platform::default();
        p.timers.insert("b".into(), timer(200, 0));
        p.timers.insert("a".into(), timer(150, 100));
        p.timers.insert("later".into(), timer(900, 0));
        assert_eq!(p.take_due(210), vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(p.timers["a"], timer(250, 100), "keeps its schedule while it keeps up");
        assert!(!p.timers.contains_key("b"), "one-shot timers are removed");
        assert_eq!(p.next_wake(), 250);
        assert!(p.take_due(240).is_empty());
    }

    #[test]
    fn an_overdue_repeating_timer_runs_once_then_continues_from_now() {
        let mut p = Platform::default();
        p.timers.insert("tick".into(), timer(1_000, 100));
        assert_eq!(p.take_due(60_000), vec!["tick".to_owned()]);
        assert_eq!(p.timers["tick"], timer(60_100, 100));
        assert!(p.take_due(60_050).is_empty());
    }

    #[test]
    fn topic_messages_parse_the_request_layout() {
        let m = parse_topic(b"\x05arena\x02\x00m1\x02hp\x09").unwrap();
        assert_eq!((m.node_type, m.key, m.topic, m.payload), ("arena", "m1", "hp", &b"\x09"[..]));
        assert!(parse_topic(b"\x05are").is_err());
    }
}
