//! Unit tests for hubs, spokes and mods, run natively with `cargo test`.
//!
//! [`TestHub`] and [`TestSpoke`] run a node on the test's thread the way a sandbox runs it:
//! calls from players, developers and other instances; a hub's platform events (timers on a
//! clock the test moves, topics it watches, presence, sessions, a mod's world events, and a
//! player's erasure and export); and a snapshot and a start from it. The platform's side is a [`FakePlatform`]. It keeps what the
//! node logged, published, sent and emitted; holds each call to the sandbox's bounds
//! ([`crate::limits`]); answers calls to other types and node API operations with what the test
//! scripts; and serves a mod its [`FakeGrid`] under the node API's rules for mods.
//!
//! ```
//! use ckx_sdk::prelude::*;
//! use ckx_sdk::testing::TestHub;
//!
//! #[derive(Default, Serialize, Deserialize)]
//! struct Counter {
//!     total: i64,
//! }
//!
//! impl Hub for Counter {
//!     fn spawn(_ctx: &Ctx, _seed: &[u8]) -> Result<Self> {
//!         Ok(Self::default())
//!     }
//!     fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
//!         decode(snapshot)
//!     }
//!     fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
//!         encode(self)
//!     }
//!     fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
//!         match call.method {
//!             "add" => {
//!                 self.total += call.decode::<i64>()?;
//!                 ctx.publish("total", &encode(&self.total)?);
//!                 encode(&self.total)
//!             }
//!             other => Err(Error::unknown_method(other)),
//!         }
//!     }
//! }
//!
//! let mut hub = TestHub::<Counter>::spawn("counter", "a").unwrap();
//! let total: i64 = hub.call_typed(Caller::Player(42), "add", &5).unwrap();
//! assert_eq!(total, 5);
//! assert_eq!(hub.published()[0].0, "total");
//! hub.restart().unwrap();
//! assert_eq!(hub.hub().total, 5);
//! ```
//!
//! Fuel, memory and deadlines are the platform's alone: they meter the built module, which the
//! `ckx-kit` command line builds and checks. A test does not see the app's manifest either, so
//! it holds an app's own types to no call policy and no node API scopes, and it applies no node
//! API rate limits.
//!
//! A thread runs one node at a time, as a sandbox runs one call at a time: drop a `TestHub` or
//! `TestSpoke` before starting the next on the same thread. `cargo test` runs each test on a
//! thread of its own.

use std::cell::{Ref, RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::rc::Rc;

use base64::Engine as _;
use base64::alphabet;
use base64::engine::general_purpose::STANDARD as B64;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::abi::native::{self, Host};
use crate::node::{Actor, Chunk, ChunkPos, Voxel, rfc3339};
use crate::rt::{HubNode, Node, SpokeNode, ckx_platform as platform};
use crate::{Call, CallError, Caller, Ctx, Error, Hub, Kind, Level, Presence, Session, Spoke, Status, WorldEvent, decode, encode, limits};

/// Where a test's clock starts: 2026-01-01T00:00:00Z, in Unix milliseconds.
pub const START_MS: u64 = 1_767_225_600_000;
/// The app every test node belongs to.
pub const TEST_APP: u64 = 1;

const SENT_ENOUGH: &str = "this call sent as much as one call may";
/// Timer calls one advance may make before the test hears its timers never settle.
const MAX_TIMER_CALLS: u32 = 1_000_000;

/// A message the node sent another instance: a call it waited on, or a one-way send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub node_type: String,
    pub key: String,
    pub method: String,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn decode<T: DeserializeOwned>(&self) -> crate::Result<T> {
        decode(&self.payload)
    }
}

/// A line the node logged, as the platform keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: Level,
    pub text: String,
}

type CallAnswer = Box<dyn FnMut(&Message) -> Result<Vec<u8>, CallError>>;
type NodeAnswer = Box<dyn FnMut(&Value) -> Result<Value, CallError>>;

struct State {
    node_type: String,
    hub: bool,
    now: u64,
    wake: Option<u64>,
    logs: Vec<LogLine>,
    published: Vec<(String, Vec<u8>)>,
    calls: Vec<Message>,
    sent: Vec<Message>,
    emitted: Vec<Vec<u8>>,
    dropped: u32,
    node_requests: Vec<(String, Value)>,
    watching: BTreeSet<(String, String, String)>,
    persist_asked: u32,
    logs_left: u32,
    out_msgs_left: u32,
    out_bytes_left: u64,
    call_answers: Vec<(String, CallAnswer)>,
    node_answers: Vec<(String, NodeAnswer)>,
    grid: Option<FakeGrid>,
}

impl State {
    fn is_mod(&self) -> bool {
        self.node_type.starts_with(limits::mods::TYPE_PREFIX)
    }

    /// What a call into the node may do, renewed for each one.
    fn begin(&mut self) {
        self.logs_left = limits::LOGS_PER_CALL;
        self.out_msgs_left = limits::OUT_MSGS_PER_CALL;
        self.out_bytes_left = limits::OUT_BYTES_PER_CALL;
    }

    fn spend(&mut self, bytes: usize) -> bool {
        let bytes = bytes as u64;
        if self.out_msgs_left == 0 || self.out_bytes_left < bytes {
            return false;
        }
        self.out_msgs_left -= 1;
        self.out_bytes_left -= bytes;
        true
    }

    fn refuse_mod_call(&self, target: &str) -> Vec<u8> {
        reply(Status::Denied, format!("`{}` may not call `{target}`", self.node_type).as_bytes())
    }
}

fn reply(status: Status, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + payload.len());
    out.push(status.as_u8());
    out.extend_from_slice(payload);
    out
}

fn answered(result: Result<Vec<u8>, CallError>) -> Vec<u8> {
    match result {
        Ok(bytes) => reply(Status::Ok, &bytes),
        Err(e) => reply(e.status, e.message.as_bytes()),
    }
}

/// What the sandbox ends the call for: the test fails here, where the module would trap.
fn trap(why: String) -> ! {
    panic!("the sandbox traps this call: {why}")
}

fn check_request(req: &[u8]) {
    if req.len() > limits::MAX_PAYLOAD + 1024 {
        trap(format!("a request of {} bytes is over {}", req.len(), limits::MAX_PAYLOAD + 1024));
    }
}

/// `[type str8][key str16][method str8][payload...]`
fn parse_request(req: &[u8]) -> Option<Message> {
    let tlen = *req.first()? as usize;
    let node_type = std::str::from_utf8(req.get(1..1 + tlen)?).ok()?.to_owned();
    let at = 1 + tlen;
    let klen = u16::from_le_bytes(req.get(at..at + 2)?.try_into().ok()?) as usize;
    let key = std::str::from_utf8(req.get(at + 2..at + 2 + klen)?).ok()?.to_owned();
    let at = at + 2 + klen;
    let mlen = *req.get(at)? as usize;
    let method = std::str::from_utf8(req.get(at + 1..at + 1 + mlen)?).ok()?.to_owned();
    let payload = req.get(at + 1 + mlen..)?.to_vec();
    Some(Message { node_type, key, method, payload })
}

fn level(v: u32) -> Level {
    match v {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        _ => Level::Debug,
    }
}

struct Shared(Rc<RefCell<State>>);

impl Host for Shared {
    fn log(&mut self, level_: u32, msg: &str) {
        let mut s = self.0.borrow_mut();
        if s.logs_left == 0 {
            return;
        }
        s.logs_left -= 1;
        let text = String::from_utf8_lossy(&msg.as_bytes()[..msg.len().min(limits::MAX_LOG_BYTES)]).into_owned();
        s.logs.push(LogLine { level: level(level_), text });
    }

    fn now_ms(&mut self) -> u64 {
        self.0.borrow().now
    }

    fn call(&mut self, req: &[u8], _deadline_ms: u32) -> Vec<u8> {
        check_request(req);
        let Some(m) = parse_request(req) else {
            return reply(Status::BadRequest, b"malformed request");
        };
        let mut s = self.0.borrow_mut();
        if !s.spend(m.payload.len()) {
            return reply(Status::Busy, SENT_ENOUGH.as_bytes());
        }
        if s.is_mod() {
            return s.refuse_mod_call(&m.node_type);
        }
        s.calls.push(m.clone());
        let result = match s.call_answers.iter_mut().rev().find(|(t, _)| *t == m.node_type || t == "*") {
            Some((_, answer)) => answer(&m),
            None => Err(CallError { status: Status::NotFound, message: format!("the test answers no calls to `{}`", m.node_type) }),
        };
        answered(result.and_then(|bytes| {
            if bytes.len() > limits::MAX_PAYLOAD {
                let message = format!("the handler's reply of {} bytes exceeds {}", bytes.len(), limits::MAX_PAYLOAD);
                return Err(CallError { status: Status::Internal, message });
            }
            Ok(bytes)
        }))
    }

    fn send(&mut self, req: &[u8]) -> u32 {
        check_request(req);
        let Some(m) = parse_request(req) else {
            return Status::BadRequest.as_u8().into();
        };
        let mut s = self.0.borrow_mut();
        if !s.spend(m.payload.len()) {
            return Status::Busy.as_u8().into();
        }
        if s.is_mod() {
            s.dropped += 1;
        } else {
            s.sent.push(m);
        }
        0
    }

    fn publish(&mut self, topic: &str, payload: &[u8]) {
        if topic.len() > 255 {
            trap(format!("a topic of {} bytes is over 255", topic.len()));
        }
        if payload.len() > limits::MAX_PAYLOAD {
            trap(format!("a payload of {} bytes is over {}", payload.len(), limits::MAX_PAYLOAD));
        }
        let mut s = self.0.borrow_mut();
        if s.spend(payload.len()) {
            s.published.push((topic.to_owned(), payload.to_vec()));
        }
    }

    fn node(&mut self, op: &str, payload: &[u8], _deadline_ms: u32) -> Vec<u8> {
        if op.len() > 255 {
            trap(format!("an operation name of {} bytes is over 255", op.len()));
        }
        if payload.len() > limits::MAX_PAYLOAD {
            trap(format!("a payload of {} bytes is over {}", payload.len(), limits::MAX_PAYLOAD));
        }
        let mut s = self.0.borrow_mut();
        if !s.spend(payload.len()) {
            return reply(Status::Busy, SENT_ENOUGH.as_bytes());
        }
        let body: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
        s.node_requests.push((op.to_owned(), body.clone()));
        let now = s.now;
        let result = match s.grid.as_mut() {
            Some(grid) => grid.answer(op, &body, now),
            None => match s.node_answers.iter_mut().rev().find(|(o, _)| o == op || o == "*") {
                Some((_, answer)) => answer(&body),
                None => Err(CallError { status: Status::NotFound, message: format!("the test answers no node API operation `{op}`") }),
            },
        };
        answered(result.map(|v| serde_json::to_vec(&v).expect("a JSON value encodes")))
    }

    fn wake_at(&mut self, at_ms: u64) {
        let mut s = self.0.borrow_mut();
        if s.hub {
            s.wake = (at_ms != 0).then_some(at_ms);
        }
    }

    fn watch(&mut self, req: &[u8], on: bool) -> Vec<u8> {
        if req.len() > 1024 {
            trap(format!("a subscription of {} bytes is over 1024", req.len()));
        }
        let Some(m) = parse_request(req) else {
            return reply(Status::BadRequest, b"malformed subscription");
        };
        let mut s = self.0.borrow_mut();
        if !s.hub {
            return reply(Status::Denied, b"only hubs subscribe to topics");
        }
        if !s.spend(0) {
            return reply(Status::Busy, SENT_ENOUGH.as_bytes());
        }
        if s.is_mod() {
            return s.refuse_mod_call(&m.node_type);
        }
        let entry = (m.node_type, m.key, m.method);
        if on {
            s.watching.insert(entry);
        } else {
            s.watching.remove(&entry);
        }
        reply(Status::Ok, &[])
    }

    fn persist_now(&mut self) {
        let mut s = self.0.borrow_mut();
        if s.hub {
            s.persist_asked += 1;
        }
    }

    fn emit(&mut self, inject: &[u8]) -> u32 {
        if inject.len() > limits::MAX_EMIT_BYTES {
            return Status::BadRequest.as_u8().into();
        }
        let mut s = self.0.borrow_mut();
        if !s.spend(inject.len()) {
            return Status::Busy.as_u8().into();
        }
        if s.is_mod() {
            s.dropped += 1;
        } else {
            s.emitted.push(inject.to_vec());
        }
        0
    }
}

fn test_ctx(kind: Kind, node_type: &str, key: &str) -> Ctx {
    Ctx { app: TEST_APP, instance: 1, epoch: 1, version: 1, kind, node_type: node_type.to_owned(), key: key.to_owned() }
}

fn app_error(message: String) -> CallError {
    CallError { status: Status::AppError, message }
}

fn bad_request(message: impl Into<String>) -> CallError {
    CallError { status: Status::BadRequest, message: message.into() }
}

fn denied(message: &str) -> CallError {
    CallError { status: Status::Denied, message: message.into() }
}

/// A start or persist error, cut as the sandbox cuts it.
fn guest_text(msg: String) -> String {
    let cut = msg.as_bytes();
    String::from_utf8_lossy(&cut[..cut.len().min(limits::MAX_LOG_BYTES)]).into_owned()
}

/// The platform as a test's node sees it: what the node did, what answers it, and the clock.
/// [`TestHub`] and [`TestSpoke`] dereference to theirs.
pub struct FakePlatform {
    ctx: Ctx,
    state: Rc<RefCell<State>>,
}

impl FakePlatform {
    fn install(ctx: Ctx, grid: Option<FakeGrid>) -> Self {
        assert!(
            native::with(|_| ()).is_none(),
            "one node runs on a thread at a time: drop the last TestHub or TestSpoke before starting another"
        );
        let state = State {
            node_type: ctx.node_type.clone(),
            hub: ctx.kind == Kind::Hub,
            now: START_MS,
            wake: None,
            logs: Vec::new(),
            published: Vec::new(),
            calls: Vec::new(),
            sent: Vec::new(),
            emitted: Vec::new(),
            dropped: 0,
            node_requests: Vec::new(),
            watching: BTreeSet::new(),
            persist_asked: 0,
            logs_left: 0,
            out_msgs_left: 0,
            out_bytes_left: 0,
            call_answers: Vec::new(),
            node_answers: Vec::new(),
            grid,
        };
        let state = Rc::new(RefCell::new(state));
        native::install(Some(Box::new(Shared(state.clone()))));
        Self { ctx, state }
    }

    fn init<N: Node>(&self, seed: &[u8], snapshot: Option<(&[u8], u64)>) -> Result<N, CallError> {
        self.state.borrow_mut().begin();
        N::init(&self.ctx, seed, snapshot).map_err(|Error(m)| app_error(guest_text(m)))
    }

    fn admit(&self, caller: Caller, method: &str) -> Result<(), CallError> {
        if method.starts_with('$') {
            return Err(denied("methods starting with `$` are the platform's"));
        }
        match caller {
            Caller::Platform => {
                Err(bad_request("the platform calls a node only with its events: advance, topic, presence, session, world, erase, export"))
            }
            Caller::Instance(_) if self.state.borrow().is_mod() => Err(denied("only players call mods")),
            _ => Ok(()),
        }
    }

    fn handle<N: Node>(&self, node: &mut N, caller: Caller, method: &str, payload: &[u8]) -> Result<Vec<u8>, CallError> {
        self.state.borrow_mut().begin();
        match node.handle(&self.ctx, Call { caller, method, payload }) {
            Ok(reply) if reply.len() > limits::MAX_PAYLOAD => Err(CallError {
                status: Status::Internal,
                message: format!("the handler's reply of {} bytes exceeds {}", reply.len(), limits::MAX_PAYLOAD),
            }),
            Ok(reply) => Ok(reply),
            Err(Error(message)) => Err(app_error(message)),
        }
    }

    fn persist<N: Node>(&self, node: &mut N) -> Result<Vec<u8>, CallError> {
        self.state.borrow_mut().begin();
        match node.persist(&self.ctx) {
            Ok(bytes) if bytes.len() > limits::MAX_SNAPSHOT_BYTES => {
                Err(app_error(format!("a snapshot of {} bytes exceeds {}", bytes.len(), limits::MAX_SNAPSHOT_BYTES)))
            }
            Ok(bytes) => Ok(bytes),
            Err(Error(m)) => Err(app_error(guest_text(m))),
        }
    }

    /// The node's identity: app [`TEST_APP`], instance 1, epoch 1, code version 1 (or the
    /// version an upgrade moved it to).
    pub fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// The test's clock, in Unix milliseconds. It starts at [`START_MS`] and moves only when the
    /// test moves it.
    pub fn now_ms(&self) -> u64 {
        self.state.borrow().now
    }

    pub fn logs(&self) -> Vec<LogLine> {
        self.state.borrow().logs.clone()
    }

    /// What the node published, by topic, in order.
    pub fn published(&self) -> Vec<(String, Vec<u8>)> {
        self.state.borrow().published.clone()
    }

    /// The calls the node made (and waited on), in order.
    pub fn calls(&self) -> Vec<Message> {
        self.state.borrow().calls.clone()
    }

    /// The one-way messages the node sent, in order.
    pub fn sent(&self) -> Vec<Message> {
        self.state.borrow().sent.clone()
    }

    /// The realtime injects the node emitted, as [`crate::emit`] built them.
    pub fn emitted(&self) -> Vec<Vec<u8>> {
        self.state.borrow().emitted.clone()
    }

    /// Sends and emits the platform dropped: a mod's.
    pub fn dropped(&self) -> u32 {
        self.state.borrow().dropped
    }

    /// The node API requests the node made: each operation and its JSON body.
    pub fn node_requests(&self) -> Vec<(String, Value)> {
        self.state.borrow().node_requests.clone()
    }

    /// The topics the hub watches: `(node type, key, topic)`.
    pub fn watching(&self) -> Vec<(String, String, String)> {
        self.state.borrow().watching.iter().cloned().collect()
    }

    /// How many times the hub asked for a snapshot with [`Ctx::persist_now`].
    pub fn persist_asked(&self) -> u32 {
        self.state.borrow().persist_asked
    }

    /// When the hub next wants its `$timer` call, if it has a timer.
    pub fn next_wake(&self) -> Option<u64> {
        self.state.borrow().wake
    }

    /// Forgets what the node did so far: logs, publishes, calls, sends, emits and node API
    /// requests.
    pub fn clear(&self) {
        let mut s = self.state.borrow_mut();
        s.logs.clear();
        s.published.clear();
        s.calls.clear();
        s.sent.clear();
        s.emitted.clear();
        s.dropped = 0;
        s.node_requests.clear();
        s.persist_asked = 0;
    }

    /// Answers the node's calls to `node_type` (`"*"` for any type) with `answer`; the latest
    /// answer for a type wins. A call nothing answers gets `NotFound`.
    pub fn answer_calls(&self, node_type: &str, answer: impl FnMut(&Message) -> Result<Vec<u8>, CallError> + 'static) {
        self.state.borrow_mut().call_answers.push((node_type.to_owned(), Box::new(answer)));
    }

    /// Answers node API operation `op` (`"*"` for any) with `answer`, given the request's JSON
    /// body; the latest answer for an operation wins. An operation nothing answers gets
    /// `NotFound`. A mod's operations are its grid's to answer instead.
    pub fn answer_node(&self, op: &str, answer: impl FnMut(&Value) -> Result<Value, CallError> + 'static) {
        self.state.borrow_mut().node_answers.push((op.to_owned(), Box::new(answer)));
    }

    /// A mod's grid, to look at.
    pub fn grid(&self) -> Ref<'_, FakeGrid> {
        Ref::map(self.state.borrow(), |s| s.grid.as_ref().expect("a mod's platform has its grid"))
    }

    /// A mod's grid, to change between calls: move actors, hand the grid to another player.
    pub fn grid_mut(&self) -> RefMut<'_, FakeGrid> {
        RefMut::map(self.state.borrow_mut(), |s| s.grid.as_mut().expect("a mod's platform has its grid"))
    }
}

impl Drop for FakePlatform {
    fn drop(&mut self) {
        native::install(None);
    }
}

/// A hub (or a mod) under test, with its [`FakePlatform`].
pub struct TestHub<T: Hub> {
    node: Option<HubNode<T>>,
    platform: FakePlatform,
}

impl<T: Hub> TestHub<T> {
    /// Spawns `(node_type, key)` with an empty seed.
    pub fn spawn(node_type: &str, key: &str) -> Result<Self, CallError> {
        Self::spawn_with(node_type, key, &[])
    }

    /// Spawns `(node_type, key)` with the seed its first caller gave.
    pub fn spawn_with(node_type: &str, key: &str, seed: &[u8]) -> Result<Self, CallError> {
        Self::start(test_ctx(Kind::Hub, node_type, key), None, seed, None)
    }

    /// Starts `(node_type, key)` from a snapshot code version `from_version` wrote: what a hub
    /// gets after a deploy, or on another host after a move.
    pub fn load(node_type: &str, key: &str, snapshot: &[u8], from_version: u64) -> Result<Self, CallError> {
        Self::start(test_ctx(Kind::Hub, node_type, key), None, &[], Some((snapshot, from_version)))
    }

    /// Spawns mod `name` on `grid`: the hub `("mod:<name>", "<grid id>")`, under the platform's
    /// rules for mods. Only players call it; its calls and subscriptions are refused, and what
    /// it sends and emits is dropped; its node API operations reach only `grid`.
    pub fn mod_on(name: &str, grid: FakeGrid) -> Result<Self, CallError> {
        let ctx = test_ctx(Kind::Hub, &format!("{}{name}", limits::mods::TYPE_PREFIX), &grid.id.to_string());
        Self::start(ctx, Some(grid), &[], None)
    }

    fn start(ctx: Ctx, grid: Option<FakeGrid>, seed: &[u8], snapshot: Option<(&[u8], u64)>) -> Result<Self, CallError> {
        let platform = FakePlatform::install(ctx, grid);
        let node = platform.init::<HubNode<T>>(seed, snapshot)?;
        Ok(Self { node: Some(node), platform })
    }

    fn node(&mut self) -> &mut HubNode<T> {
        self.node.as_mut().expect("the hub runs")
    }

    pub fn hub(&self) -> &T {
        self.node.as_ref().expect("the hub runs").hub()
    }

    pub fn hub_mut(&mut self) -> &mut T {
        self.node().hub_mut()
    }

    /// Calls `method` as `caller` and returns what the caller receives: the reply, or the
    /// handler's error with status `AppError`.
    pub fn call(&mut self, caller: Caller, method: &str, payload: &[u8]) -> Result<Vec<u8>, CallError> {
        self.platform.admit(caller, method)?;
        let node = self.node.as_mut().expect("the hub runs");
        self.platform.handle(node, caller, method, payload)
    }

    /// [`TestHub::call`] with MessagePack on both sides.
    pub fn call_typed<Req: Serialize + ?Sized, Resp: DeserializeOwned>(
        &mut self,
        caller: Caller,
        method: &str,
        req: &Req,
    ) -> Result<Resp, CallError> {
        let payload = encode(req).map_err(|Error(m)| bad_request(m))?;
        let reply = self.call(caller, method, &payload)?;
        decode(&reply).map_err(|Error(message)| CallError { status: Status::Internal, message })
    }

    fn event(&mut self, caller: Caller, method: &str, payload: &[u8]) -> Result<(), CallError> {
        let node = self.node.as_mut().expect("the hub runs");
        self.platform.handle(node, caller, method, payload).map(|_| ())
    }

    /// Moves the clock `ms` on, calling `$timer` each time a timer falls due on the way.
    pub fn advance(&mut self, ms: u64) -> Result<(), CallError> {
        let until = self.now_ms().saturating_add(ms);
        self.advance_to(until)
    }

    /// Moves the clock to `at_ms` (Unix milliseconds), calling `$timer` each time a timer falls
    /// due on the way. A time already past changes nothing.
    pub fn advance_to(&mut self, at_ms: u64) -> Result<(), CallError> {
        let mut calls = 0;
        loop {
            let due = self.platform.state.borrow().wake.filter(|&w| w <= at_ms);
            let Some(due) = due else { break };
            calls += 1;
            assert!(calls <= MAX_TIMER_CALLS, "timers fell due {MAX_TIMER_CALLS} times in one advance: one is due again at once");
            {
                let mut s = self.platform.state.borrow_mut();
                s.now = s.now.max(due);
                s.wake = None;
            }
            self.event(Caller::Platform, platform::TIMER, &[])?;
        }
        let mut s = self.platform.state.borrow_mut();
        s.now = s.now.max(at_ms);
        Ok(())
    }

    /// Hands a mod world events as the platform does: only its grid's, at most
    /// [`limits::mods::WORLD_BATCH_MAX`] a call.
    pub fn world(&mut self, events: &[WorldEvent]) -> Result<(), CallError> {
        let Some(grid) = self.platform.state.borrow().grid.as_ref().map(|g| g.id) else {
            return Err(bad_request("world events are for mods"));
        };
        let mine: Vec<WorldEvent> = events.iter().filter(|e| e.grid == grid).cloned().collect();
        for batch in mine.chunks(limits::mods::WORLD_BATCH_MAX) {
            let payload = world_payload(batch).map_err(|Error(m)| bad_request(m))?;
            self.event(Caller::Platform, platform::WORLD, &payload)?;
        }
        Ok(())
    }

    /// A player's connection subscribing to this hub's first topic, or leaving its last.
    pub fn session(&mut self, player: u64, session: Session) -> Result<(), CallError> {
        let payload = [match session {
            Session::Joined => platform::SESSION_JOINED,
            Session::Left => platform::SESSION_LEFT,
        }];
        self.event(Caller::Player(player), platform::SESSION, &payload)
    }

    /// Players entering or leaving the app, as the root hub hears it.
    pub fn presence(&mut self, presence: &Presence) -> Result<(), CallError> {
        let payload = encode(presence).map_err(|Error(m)| bad_request(m))?;
        self.event(Caller::Platform, platform::PRESENCE, &payload)
    }

    /// `player`'s account being erased, as the platform tells every hub: `$erase`, then a
    /// snapshot, which the platform stores before it counts the hub done. A hub without the hook
    /// answers `AppError` "not implemented: …".
    pub fn erase(&mut self, player: u64) -> Result<(), CallError> {
        let payload = player_payload(player).map_err(|Error(m)| bad_request(m))?;
        self.event(Caller::Platform, platform::ERASE, &payload)?;
        self.persist().map(|_| ())
    }

    /// What the hub holds about `player`, as the platform asks it for their data export:
    /// `Value::Null` when it holds nothing. A hub without the hook answers `AppError` "not
    /// implemented: …".
    pub fn export(&mut self, player: u64) -> Result<Value, CallError> {
        let payload = player_payload(player).map_err(|Error(m)| bad_request(m))?;
        let node = self.node.as_mut().expect("the hub runs");
        let json = self.platform.handle(node, Caller::Platform, platform::EXPORT, &payload)?;
        serde_json::from_slice(&json)
            .map_err(|e| CallError { status: Status::Internal, message: format!("an export that is not JSON: {e}") })
    }

    /// `node_type`/`key` publishing `payload` on `topic`. A hub hears only the topics it
    /// watches.
    pub fn topic(&mut self, node_type: &str, key: &str, topic: &str, payload: &[u8]) -> Result<(), CallError> {
        let entry = (node_type.to_owned(), key.to_owned(), topic.to_owned());
        if !self.platform.state.borrow().watching.contains(&entry) {
            return Err(bad_request(format!("the hub does not watch `{topic}` on {node_type}/{key}")));
        }
        self.event(Caller::Platform, platform::TOPIC, &crate::request(node_type, key, topic, payload))
    }

    /// The hub's snapshot, as the platform takes it: its own bytes behind its timers and
    /// watches.
    pub fn persist(&mut self) -> Result<Vec<u8>, CallError> {
        let node = self.node.as_mut().expect("the hub runs");
        self.platform.persist(node)
    }

    /// Stops the hub and starts it again from its snapshot, as a move to another host does.
    /// Its timers and watches come back with it.
    pub fn restart(&mut self) -> Result<(), CallError> {
        let snapshot = self.persist()?;
        self.stop();
        let version = self.platform.ctx.version;
        self.node = Some(self.platform.init::<HubNode<T>>(&[], Some((&snapshot, version)))?);
        Ok(())
    }

    /// Deploys the hub's next version: its snapshot, taken now, loads into `U` at code version
    /// one higher.
    pub fn upgrade<U: Hub>(mut self) -> Result<TestHub<U>, CallError> {
        let snapshot = self.persist()?;
        self.stop();
        let mut platform = self.platform;
        let from = platform.ctx.version;
        platform.ctx.version = from + 1;
        let node = platform.init::<HubNode<U>>(&[], Some((&snapshot, from)))?;
        Ok(TestHub { node: Some(node), platform })
    }

    fn stop(&mut self) {
        if let Some(mut node) = self.node.take() {
            self.platform.state.borrow_mut().begin();
            node.evict(&self.platform.ctx);
        }
        let mut s = self.platform.state.borrow_mut();
        s.wake = None;
        s.watching.clear();
    }

    /// The hub's pending timers and when each is next due.
    pub fn timers(&self) -> Vec<(String, u64)> {
        crate::rt::timers()
    }
}

impl<T: Hub> Deref for TestHub<T> {
    type Target = FakePlatform;

    fn deref(&self) -> &FakePlatform {
        &self.platform
    }
}

/// A replica of a spoke under test, with its [`FakePlatform`].
pub struct TestSpoke<T: Spoke> {
    node: SpokeNode<T>,
    platform: FakePlatform,
}

impl<T: Spoke> TestSpoke<T> {
    /// Starts a replica of `node_type`.
    pub fn start(node_type: &str) -> Result<Self, CallError> {
        let platform = FakePlatform::install(test_ctx(Kind::Spoke, node_type, ""), None);
        let node = platform.init::<SpokeNode<T>>(&[], None)?;
        Ok(Self { node, platform })
    }

    pub fn spoke(&self) -> &T {
        self.node.spoke()
    }

    pub fn spoke_mut(&mut self) -> &mut T {
        self.node.spoke_mut()
    }

    /// Calls `method` as `caller` and returns what the caller receives.
    pub fn call(&mut self, caller: Caller, method: &str, payload: &[u8]) -> Result<Vec<u8>, CallError> {
        self.platform.admit(caller, method)?;
        self.platform.handle(&mut self.node, caller, method, payload)
    }

    /// [`TestSpoke::call`] with MessagePack on both sides.
    pub fn call_typed<Req: Serialize + ?Sized, Resp: DeserializeOwned>(
        &mut self,
        caller: Caller,
        method: &str,
        req: &Req,
    ) -> Result<Resp, CallError> {
        let payload = encode(req).map_err(|Error(m)| bad_request(m))?;
        let reply = self.call(caller, method, &payload)?;
        decode(&reply).map_err(|Error(message)| CallError { status: Status::Internal, message })
    }
}

impl<T: Spoke> Deref for TestSpoke<T> {
    type Target = FakePlatform;

    fn deref(&self) -> &FakePlatform {
        &self.platform
    }
}

/// The payload of the `$world` call the platform makes for `events`: what [`TestHub::world`]
/// hands a mod.
pub fn world_payload(events: &[WorldEvent]) -> crate::Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Batch<'a> {
        events: &'a [WorldEvent],
    }
    encode(&Batch { events })
}

/// The payload of the `$erase` and `$export` calls the platform makes about `player`: what
/// [`TestHub::erase`] and [`TestHub::export`] hand a hub.
pub fn player_payload(player: u64) -> crate::Result<Vec<u8>> {
    encode(&crate::rt::PlayerEvent { player })
}

// ---- the node API, as it serves a mod ----

/// The node API operations a mod may call.
const MOD_OPS: [&str; 7] =
    ["world.chunk", "world.voxels", "world.actors", "world.actors_radius", "world.set_voxels", "grids.get", "grids.check_permission"];

/// Each node API operation and the scope it needs.
const OP_SCOPES: [(&str, &str); 29] = [
    ("players.get", "players.read"),
    ("players.features", "players.read"),
    ("players.check_permission", "players.read"),
    ("players.state", "players.read"),
    ("players.avatar_state", "players.read"),
    ("players.blocked", "players.read"),
    ("players.set_state", "players.write"),
    ("world.chunk", "world.read"),
    ("world.voxels", "world.read"),
    ("world.actors", "world.read"),
    ("world.actors_radius", "world.read"),
    ("world.set_voxels", "world.write"),
    ("world.set_chunk_state", "world.write"),
    ("grids.get", "grids.read"),
    ("grids.check_permission", "grids.read"),
    ("grids.settings", "grids.read"),
    ("grids.create", "grids.write"),
    ("grids.delete", "grids.write"),
    ("grids.assign_group", "grids.write"),
    ("grids.unassign_group", "grids.write"),
    ("grids.set_open_permissions", "grids.write"),
    ("permissions.grant", "permissions.write"),
    ("permissions.revoke", "permissions.write"),
    ("access.get", "access.read"),
    ("access.grant", "access.write"),
    ("access.revoke", "access.write"),
    ("access.suspend", "access.write"),
    ("access.unsuspend", "access.write"),
    ("safety.report", "safety.write"),
];

/// Rows the node API returns at most: actors in one chunk, in a radius, voxels in a chunk.
const ACTOR_ROWS: usize = 200;
const RADIUS_ROWS: usize = 500;
const VOXEL_ROWS: usize = 2048;
const RADIUS_XZ: i64 = 3;
const RADIUS_Y: i64 = 1;

/// Base64 as the game API reads it: padding or none.
const LENIENT: GeneralPurpose =
    GeneralPurpose::new(&alphabet::STANDARD, GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent));

/// The node API's refusal: the game API's error body with its HTTP status, as the host hands
/// both to the node.
fn refusal(http: u16, message: &str) -> CallError {
    let (status, error) = match http {
        400 => (Status::AppError, "Bad Request"),
        403 => (Status::Denied, "Forbidden"),
        404 => (Status::NotFound, "Not Found"),
        _ => (Status::Unavailable, "Error"),
    };
    let message = serde_json::to_string(message).expect("a string encodes");
    CallError { status, message: format!(r#"{{"message":{message},"error":"{error}","statusCode":{http}}}"#) }
}

fn int64(value: Option<&Value>, name: &str) -> Result<i64, CallError> {
    let bad = || refusal(400, &format!("{name} must be a 64-bit integer"));
    match value {
        Some(Value::Number(n)) => {
            let f = n.as_f64().ok_or_else(bad)?;
            if f.fract() != 0.0 || f.abs() > 9_007_199_254_740_991.0 {
                return Err(bad());
            }
            Ok(f as i64)
        }
        Some(Value::String(s)) => {
            let digits = s.strip_prefix('-').unwrap_or(s);
            if digits.is_empty() || digits.len() > 19 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            s.parse().map_err(|_| bad())
        }
        _ => Err(bad()),
    }
}

fn id64(value: Option<&Value>, name: &str) -> Result<u64, CallError> {
    let v = int64(value, name)?;
    if v <= 0 {
        return Err(refusal(400, &format!("{name} must be a positive id")));
    }
    Ok(v as u64)
}

fn int_in(value: Option<&Value>, name: &str, min: i64, max: i64) -> Result<i64, CallError> {
    let n = value.and_then(Value::as_f64).filter(|f| f.fract() == 0.0 && *f >= min as f64 && *f <= max as f64);
    n.map(|f| f as i64).ok_or_else(|| refusal(400, &format!("{name} must be an integer from {min} to {max}")))
}

fn text(value: Option<&Value>, name: &str) -> Result<String, CallError> {
    match value.and_then(Value::as_str) {
        Some(s) if !s.is_empty() && s.encode_utf16().count() <= 128 => Ok(s.to_owned()),
        _ => Err(refusal(400, &format!("{name} must be a non-empty string of at most 128 characters"))),
    }
}

fn base64_bytes(value: Option<&Value>, name: &str, max: usize) -> Result<Option<Vec<u8>>, CallError> {
    let s = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => s,
        Some(_) => return Err(refusal(400, &format!("{name} must be base64"))),
    };
    let body = s.trim_end_matches('=');
    if s.len() - body.len() > 2 || !body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/') {
        return Err(refusal(400, &format!("{name} must be base64")));
    }
    let bytes = LENIENT.decode(body).map_err(|_| refusal(400, &format!("{name} must be base64")))?;
    if bytes.len() > max {
        return Err(refusal(400, &format!("{name} is capped at {max} bytes")));
    }
    Ok(Some(bytes))
}

fn chunk_pos(value: Option<&Value>, name: &str) -> Result<ChunkPos, CallError> {
    let v = value.unwrap_or(&Value::Null);
    Ok(ChunkPos::new(
        int64(v.get("x"), &format!("{name}.x"))?,
        int64(v.get("y"), &format!("{name}.y"))?,
        int64(v.get("z"), &format!("{name}.z"))?,
    ))
}

fn pos_json(p: ChunkPos) -> Value {
    json!({ "x": p.x.to_string(), "y": p.y.to_string(), "z": p.z.to_string() })
}

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

type Pos = (i64, i64, i64);

fn key(p: ChunkPos) -> Pos {
    (p.x, p.y, p.z)
}

#[derive(Debug, Clone, Default)]
struct FakeChunk {
    /// The chunk as stored, when the app stored it.
    stored: Option<Chunk>,
    /// Voxels written into it, by `(y, x, z)`: the order the node API lists them in.
    edits: BTreeMap<(i16, i16, i16), Voxel>,
}

/// A mod's grid as the node API serves it to the mod: the box, its owner, the chunks and
/// voxels in it, the actors there and its permissions, under the node API's rules for mods.
/// A mod calls seven operations (`world.chunk`, `world.voxels`, `world.actors`,
/// `world.actors_radius`, `world.set_voxels`, `grids.get`, `grids.check_permission`), only
/// inside its grid; writes voxels only where its owner may edit them; and is refused
/// everything once its owner no longer owns the grid. A refusal reads as the game API's.
#[derive(Debug, Clone)]
pub struct FakeGrid {
    id: u64,
    low: ChunkPos,
    high: ChunkPos,
    owner: u64,
    /// Who holds the grid now; `None` once it is gone.
    holder: Option<u64>,
    open_permissions: BTreeSet<String>,
    chunks: BTreeMap<Pos, FakeChunk>,
    actors: Vec<Actor>,
    grants: BTreeSet<(u64, String)>,
    /// Chunks where a grid nested in this one does not grant the owner `update_voxel_data`.
    closed: BTreeSet<Pos>,
    next_update: u64,
}

impl FakeGrid {
    /// Grid `id` from chunk `low` to chunk `high` (both included), owned by player `owner`, who
    /// deployed the mod.
    pub fn new(id: u64, low: ChunkPos, high: ChunkPos, owner: u64) -> Self {
        Self {
            id,
            low,
            high,
            owner,
            holder: Some(owner),
            open_permissions: BTreeSet::new(),
            chunks: BTreeMap::new(),
            actors: Vec::new(),
            grants: BTreeSet::new(),
            closed: BTreeSet::new(),
            next_update: 1,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// The player who deployed the mod.
    pub fn owner(&self) -> u64 {
        self.owner
    }

    pub fn contains(&self, at: ChunkPos) -> bool {
        self.within(at, 0, 0)
    }

    /// A chunk the app stored: its dense grid and its state blob.
    pub fn with_chunk(mut self, chunk: Chunk) -> Self {
        let at = key(chunk.at);
        self.chunks.entry(at).or_default().stored = Some(chunk);
        self
    }

    /// A voxel written into `chunk` before the test.
    pub fn with_voxel(mut self, chunk: ChunkPos, voxel: Voxel) -> Self {
        self.chunks.entry(key(chunk)).or_default().edits.insert((voxel.y, voxel.x, voxel.z), voxel);
        self
    }

    /// An actor in the world; the node API lists the ones in the chunks asked about.
    pub fn with_actor(mut self, actor: Actor) -> Self {
        self.actors.push(actor);
        self
    }

    /// `player` holds permission key `permission` on the grid.
    pub fn with_permission(mut self, player: u64, permission: &str) -> Self {
        self.grants.insert((player, permission.to_owned()));
        self
    }

    /// The keys every player holds on the grid.
    pub fn with_open_permissions(mut self, permissions: &[&str]) -> Self {
        self.open_permissions = permissions.iter().map(|p| (*p).to_owned()).collect();
        self
    }

    /// A grid nested in this one decides `chunk`, and does not grant the owner
    /// `update_voxel_data` there: the mod's writes to it are refused.
    pub fn without_voxel_edits_at(mut self, chunk: ChunkPos) -> Self {
        self.closed.insert(key(chunk));
        self
    }

    pub fn actors(&self) -> &[Actor] {
        &self.actors
    }

    /// Replaces the actors, as players move.
    pub fn set_actors(&mut self, actors: Vec<Actor>) {
        self.actors = actors;
    }

    /// Another player owns the grid now: the mod's node API operations are refused.
    pub fn transfer(&mut self, to: u64) {
        self.holder = Some(to);
    }

    /// The grid is deleted: the mod's node API operations are refused.
    pub fn delete(&mut self) {
        self.holder = None;
    }

    /// The chunk at `at`, as stored.
    pub fn chunk(&self, at: ChunkPos) -> Option<Chunk> {
        self.chunks.get(&key(at)).and_then(|c| c.stored.clone())
    }

    /// The voxels written into the chunk at `at`, in the node API's order.
    pub fn voxels(&self, at: ChunkPos) -> Vec<Voxel> {
        self.chunks.get(&key(at)).map(|c| c.edits.values().cloned().collect()).unwrap_or_default()
    }

    /// The voxel written at `voxel` in `chunk`, if one was.
    pub fn voxel(&self, chunk: ChunkPos, voxel: (i16, i16, i16)) -> Option<Voxel> {
        self.chunks.get(&key(chunk)).and_then(|c| c.edits.get(&(voxel.1, voxel.0, voxel.2)).cloned())
    }

    fn within(&self, at: ChunkPos, rxz: i64, ry: i64) -> bool {
        let (x, y, z) = (i128::from(at.x), i128::from(at.y), i128::from(at.z));
        let (rxz, ry) = (i128::from(rxz), i128::from(ry));
        x - rxz >= i128::from(self.low.x)
            && x + rxz <= i128::from(self.high.x)
            && y - ry >= i128::from(self.low.y)
            && y + ry <= i128::from(self.high.y)
            && z - rxz >= i128::from(self.low.z)
            && z + rxz <= i128::from(self.high.z)
    }

    fn inside(&self, at: ChunkPos, rxz: i64, ry: i64) -> Result<(), CallError> {
        if self.within(at, rxz, ry) { Ok(()) } else { Err(refusal(403, "a mod reaches only inside its grid's bounds")) }
    }

    fn own_grid(&self, body: &Value) -> Result<(), CallError> {
        match body.get("grid") {
            Some(v) if id64(Some(v), "grid")? == self.id => Ok(()),
            _ => Err(refusal(403, "a mod reads only its own grid")),
        }
    }

    fn chunk_json(&self, at: ChunkPos) -> Value {
        match self.chunk(at) {
            None => Value::Null,
            Some(c) => json!({
                "chunkId": c.id.to_string(),
                "x": at.x.to_string(),
                "y": at.y.to_string(),
                "z": at.z.to_string(),
                "stateBase64": c.voxels.as_deref().map(b64),
                "chunkStateBase64": c.chunk_state.as_deref().map(b64),
                "updatedAt": Value::Null,
            }),
        }
    }

    fn voxels_json(&self, at: ChunkPos) -> Value {
        let list: Vec<Value> = self
            .voxels(at)
            .into_iter()
            .take(VOXEL_ROWS)
            .map(|v| {
                json!({
                    "voxelX": v.x,
                    "voxelY": v.y,
                    "voxelZ": v.z,
                    "voxelType": v.voxel_type,
                    "stateBase64": v.state.as_deref().map(b64),
                    "updatedAt": v.updated_at,
                    "createdBy": v.created_by,
                })
            })
            .collect();
        Value::Array(list)
    }

    fn actors_json(&self, low: ChunkPos, high: ChunkPos, rows: usize) -> Value {
        let inside = |c: ChunkPos| (low.x..=high.x).contains(&c.x) && (low.y..=high.y).contains(&c.y) && (low.z..=high.z).contains(&c.z);
        let list: Vec<Value> = self
            .actors
            .iter()
            .filter(|a| inside(a.chunk))
            .take(rows)
            .map(|a| {
                json!({
                    "uuid": a.uuid,
                    "uuidHex": a.uuid.bytes().map(|b| format!("{b:02x}")).collect::<String>(),
                    "userId": a.player.map(|p| p.to_string()),
                    "stateBase64": a.state.as_deref().map(b64),
                    "chunkX": a.chunk.x.to_string(),
                    "chunkY": a.chunk.y.to_string(),
                    "chunkZ": a.chunk.z.to_string(),
                    "updatedAt": Value::Null,
                })
            })
            .collect();
        Value::Array(list)
    }

    fn grid_json(&self) -> Value {
        json!({
            "gridId": self.id.to_string(),
            "low": pos_json(self.low),
            "high": pos_json(self.high),
            "createdAt": Value::Null,
            "owner": self.holder.map(|h| json!({ "kind": "user", "ref": h.to_string() })),
            "openPermissions": self.open_permissions.iter().collect::<Vec<_>>(),
        })
    }

    /// Answers node API operation `op` with `body` for the grid's mod at Unix time `now`.
    fn answer(&mut self, op: &str, body: &Value, now: u64) -> Result<Value, CallError> {
        let Some(&(_, scope)) = OP_SCOPES.iter().find(|(o, _)| *o == op) else {
            return Err(refusal(404, &format!("unknown node API operation {op}")));
        };
        if !limits::mods::SCOPES.contains(&scope) {
            return Err(refusal(403, &format!("the capability lacks scope {scope}")));
        }
        let writes = if op == "world.set_voxels" { voxel_writes(body)? } else { Vec::new() };
        if op == "world.set_chunk_state" {
            base64_bytes(body.get("stateBase64"), "stateBase64", crate::node::MAX_CHUNK_STATE)?;
        }
        if !MOD_OPS.contains(&op) {
            return Err(refusal(403, &format!("a mod may not call {op}")));
        }
        let Some(holder) = self.holder else {
            return Err(refusal(404, "the mod's grid is gone"));
        };
        if holder != self.owner {
            return Err(refusal(403, "the mod's owner no longer owns its grid"));
        }
        match op {
            "world.chunk" | "world.voxels" | "world.actors" => {
                let at = chunk_pos(Some(body), "chunk")?;
                self.inside(at, 0, 0)?;
                Ok(match op {
                    "world.chunk" => json!({ "chunk": self.chunk_json(at) }),
                    "world.voxels" => json!({ "voxels": self.voxels_json(at) }),
                    _ => json!({ "actors": self.actors_json(at, at, ACTOR_ROWS) }),
                })
            }
            "world.actors_radius" => {
                let or_zero = |k: &str| body.get(k).filter(|v| !v.is_null()).cloned().unwrap_or(json!(0));
                let rxz = int_in(Some(&or_zero("radiusXz")), "radiusXz", 0, RADIUS_XZ)?;
                let ry = int_in(Some(&or_zero("radiusY")), "radiusY", 0, RADIUS_Y)?;
                let at = chunk_pos(Some(body), "chunk")?;
                self.inside(at, rxz, ry)?;
                let low = ChunkPos::new(at.x - rxz, at.y - ry, at.z - rxz);
                let high = ChunkPos::new(at.x + rxz, at.y + ry, at.z + rxz);
                Ok(json!({ "actors": self.actors_json(low, high, RADIUS_ROWS) }))
            }
            "world.set_voxels" => {
                if !writes.iter().all(|w| self.contains(w.chunk)) {
                    return Err(refusal(403, "a mod reaches only inside its grid's bounds"));
                }
                let mut seen = BTreeSet::new();
                for w in &writes {
                    let at = key(w.chunk);
                    if seen.insert(at) && self.closed.contains(&at) {
                        return Err(refusal(
                            403,
                            &format!(
                                "the mod's owner may not edit voxels at chunk ({}, {}, {}): the most specific grid there does not grant them 'update_voxel_data'",
                                at.0, at.1, at.2
                            ),
                        ));
                    }
                }
                let mut ids = Vec::with_capacity(writes.len());
                for w in writes {
                    let voxel = Voxel {
                        x: w.voxel.0,
                        y: w.voxel.1,
                        z: w.voxel.2,
                        voxel_type: w.voxel_type,
                        state: w.state,
                        created_by: Some("0".to_owned()),
                        updated_at: Some(rfc3339(now)),
                    };
                    self.chunks.entry(key(w.chunk)).or_default().edits.insert((voxel.y, voxel.x, voxel.z), voxel);
                    ids.push(self.next_update.to_string());
                    self.next_update += 1;
                }
                Ok(json!({ "voxelUpdateIds": ids }))
            }
            "grids.get" => {
                self.own_grid(body)?;
                Ok(json!({ "grid": self.grid_json() }))
            }
            _ => {
                self.own_grid(body)?;
                let player = id64(body.get("player"), "player")?;
                let permission = text(body.get("permission"), "permission")?;
                Ok(json!({ "allowed": self.grants.contains(&(player, permission)) }))
            }
        }
    }
}

struct Write {
    chunk: ChunkPos,
    voxel: (i16, i16, i16),
    voxel_type: i16,
    state: Option<Vec<u8>>,
}

fn voxel_writes(body: &Value) -> Result<Vec<Write>, CallError> {
    let max = crate::node::MAX_VOXEL_WRITES;
    let list = body
        .get("writes")
        .and_then(Value::as_array)
        .filter(|l| (1..=max).contains(&l.len()))
        .ok_or_else(|| refusal(400, &format!("writes must list 1 to {max} voxel writes")))?;
    let axis = |v: &Value, i: usize, k: &str| int_in(v.get(k), &format!("writes[{i}].voxel.{k}"), i16::MIN.into(), i16::MAX.into());
    list.iter()
        .enumerate()
        .map(|(i, w)| {
            let chunk = chunk_pos(w.get("chunk"), &format!("writes[{i}].chunk"))?;
            let v = w.get("voxel").unwrap_or(&Value::Null);
            let voxel = (axis(v, i, "x")? as i16, axis(v, i, "y")? as i16, axis(v, i, "z")? as i16);
            let voxel_type = int_in(w.get("voxelType"), &format!("writes[{i}].voxelType"), i16::MIN.into(), i16::MAX.into())? as i16;
            let state = base64_bytes(w.get("stateBase64"), &format!("writes[{i}].stateBase64"), crate::node::MAX_VOXEL_STATE)?;
            Ok(Write { chunk, voxel, voxel_type, state })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::*;

    #[derive(Default, Serialize, Deserialize)]
    struct Ticker {
        ticks: u32,
        heard: Vec<String>,
    }

    impl Hub for Ticker {
        fn spawn(ctx: &Ctx, _seed: &[u8]) -> Result<Self> {
            ctx.timer_every("tick", 100)?;
            Ok(Self::default())
        }

        fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
            decode(snapshot)
        }

        fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
            encode(self)
        }

        fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
            match call.method {
                "watch" => {
                    ctx.subscribe("arena", "m1", "hp")?;
                    Ok(Vec::new())
                }
                "ask" => Ok(ctx.call("bank", "a", "balance", call.payload)?),
                "flood" => {
                    for _ in 0..40 {
                        ctx.log(Level::Info, "line");
                    }
                    let mut refused = 0u32;
                    for _ in 0..=limits::OUT_MSGS_PER_CALL {
                        if ctx.send("bank", "a", "noop", &[]).is_err() {
                            refused += 1;
                        }
                    }
                    encode(&refused)
                }
                "big" => Ok(vec![0; limits::MAX_PAYLOAD + 1]),
                other => Err(Error::unknown_method(other)),
            }
        }

        fn on_timer(&mut self, ctx: &Ctx, _timer: &str) -> Result<()> {
            self.ticks += 1;
            ctx.publish("ticks", &encode(&self.ticks)?);
            Ok(())
        }

        fn on_topic(&mut self, _ctx: &Ctx, msg: TopicMsg<'_>) -> Result<()> {
            self.heard.push(format!("{}/{} {}", msg.node_type, msg.key, msg.topic));
            Ok(())
        }
    }

    #[test]
    fn timers_fall_due_as_the_clock_moves_and_survive_a_restart() {
        let mut hub = TestHub::<Ticker>::spawn("ticker", "a").unwrap();
        assert_eq!(hub.next_wake(), Some(START_MS + 100));
        hub.advance(350).unwrap();
        assert_eq!((hub.hub().ticks, hub.published().len()), (3, 3));
        hub.restart().unwrap();
        assert_eq!(hub.timers(), vec![("tick".to_owned(), START_MS + 400)]);
        hub.advance(50).unwrap();
        assert_eq!(hub.hub().ticks, 4);
    }

    #[test]
    fn topics_reach_a_hub_only_while_it_watches_them() {
        let mut hub = TestHub::<Ticker>::spawn("ticker", "a").unwrap();
        assert_eq!(hub.topic("arena", "m1", "hp", b"\x09").unwrap_err().status, Status::BadRequest);
        hub.call(Caller::Player(1), "watch", &[]).unwrap();
        hub.topic("arena", "m1", "hp", b"\x09").unwrap();
        hub.restart().unwrap();
        assert_eq!(hub.watching(), vec![("arena".to_owned(), "m1".to_owned(), "hp".to_owned())], "renewed on start");
        assert_eq!(hub.hub().heard, vec!["arena/m1 hp".to_owned()]);
    }

    #[test]
    fn calls_to_other_types_get_what_the_test_scripts() {
        let mut hub = TestHub::<Ticker>::spawn("ticker", "a").unwrap();
        let e = hub.call(Caller::Player(1), "ask", b"q").unwrap_err();
        assert!(e.message.starts_with("NotFound: the test answers no calls to `bank`"), "{e}");
        hub.answer_calls("bank", |m| Ok([m.payload.as_slice(), b"!"].concat()));
        assert_eq!(hub.call(Caller::Player(1), "ask", b"q").unwrap(), b"q!");
        assert_eq!(hub.calls().last().map(|m| m.method.clone()), Some("balance".to_owned()));
    }

    #[test]
    fn a_call_is_held_to_the_sandbox_bounds() {
        let mut hub = TestHub::<Ticker>::spawn("ticker", "a").unwrap();
        let refused: u32 = hub.call_typed(Caller::Player(1), "flood", &()).unwrap();
        assert_eq!((refused, hub.logs().len()), (1, limits::LOGS_PER_CALL as usize));
        assert_eq!(hub.sent().len(), limits::OUT_MSGS_PER_CALL as usize);
        let e = hub.call(Caller::Player(1), "big", &[]).unwrap_err();
        assert!(e.status == Status::Internal && e.message.contains("exceeds"), "{e}");
        assert_eq!(hub.call(Caller::Player(1), "$timer", &[]).unwrap_err().status, Status::Denied);
        assert_eq!(hub.call(Caller::Player(1), "nope", &[]).unwrap_err().status, Status::AppError);
    }

    /// Scores by player: what the platform's erasure and export reach.
    #[derive(Default, Serialize, Deserialize)]
    struct Keeper {
        scores: std::collections::BTreeMap<u64, i64>,
    }

    impl Hub for Keeper {
        fn spawn(_ctx: &Ctx, _seed: &[u8]) -> Result<Self> {
            Ok(Self::default())
        }

        fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
            decode(snapshot)
        }

        fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
            encode(self)
        }

        fn handle(&mut self, _ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
            match call.method {
                "score" => {
                    *self.scores.entry(call.player()?).or_default() += call.decode::<i64>()?;
                    Ok(Vec::new())
                }
                "flood" => {
                    self.scores.extend((0..60_000).map(|p| (p, i64::MAX)));
                    Ok(Vec::new())
                }
                other => Err(Error::unknown_method(other)),
            }
        }

        fn on_player_erased(&mut self, _ctx: &Ctx, player: u64) -> Result<()> {
            self.scores.remove(&player);
            Ok(())
        }

        fn on_player_export(&mut self, _ctx: &Ctx, player: u64) -> Result<Value> {
            if player == 7 {
                return Ok(json!({ "everyone": self.scores }));
            }
            Ok(self.scores.get(&player).map_or(Value::Null, |s| json!({ "score": s })))
        }
    }

    #[test]
    fn a_hub_erases_and_exports_a_player_as_the_platform_asks() {
        let mut hub = TestHub::<Keeper>::spawn("board", "a").unwrap();
        hub.call(Caller::Player(42), "score", &encode(&5).unwrap()).unwrap();
        hub.call(Caller::Player(43), "score", &encode(&9).unwrap()).unwrap();
        assert_eq!(hub.export(42).unwrap(), json!({ "score": 5 }));
        assert_eq!(hub.export(44).unwrap(), Value::Null, "nothing held");
        hub.erase(42).unwrap();
        assert_eq!(hub.export(42).unwrap(), Value::Null);
        hub.restart().unwrap();
        assert_eq!(hub.hub().scores.keys().copied().collect::<Vec<_>>(), vec![43], "the erasure is in the snapshot");
        for method in ["$erase", "$export"] {
            for caller in [Caller::Player(42), Caller::Developer(1), Caller::Instance(9)] {
                let e = hub.call(caller, method, &player_payload(43).unwrap()).unwrap_err();
                assert_eq!(e.status, Status::Denied, "{method} from {caller:?}");
            }
        }
        assert_eq!(hub.hub().scores.len(), 1);

        hub.call(Caller::Player(1), "flood", &[]).unwrap();
        let e = hub.export(7).unwrap_err();
        assert!(e.status == Status::AppError && e.message.contains("exceeds"), "{e}");
    }

    #[test]
    fn a_hub_without_the_hooks_answers_not_implemented() {
        let mut hub = TestHub::<Ticker>::spawn("ticker", "a").unwrap();
        for e in [hub.erase(42).unwrap_err(), hub.export(42).unwrap_err()] {
            assert_eq!(e.status, Status::AppError);
            assert!(e.message.starts_with("not implemented: `on_player_e"), "{e}");
        }
    }

    #[test]
    fn one_node_runs_on_a_thread_at_a_time() {
        let hub = TestHub::<Ticker>::spawn("ticker", "a").unwrap();
        let second = std::panic::catch_unwind(|| TestHub::<Ticker>::spawn("ticker", "b").map(|_| ()));
        assert!(second.is_err());
        drop(hub);
        TestHub::<Ticker>::spawn("ticker", "b").unwrap();
    }
}
