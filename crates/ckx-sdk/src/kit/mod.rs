//! The game kit: the protocol kit hubs speak to their clients, and the pieces the starter
//! packs are built from.
//!
//! # The protocol
//!
//! A kit hub's topics carry [`Message`]s: MessagePack maps holding the hub's data with four
//! reserved fields beside it, `v` (the protocol version, [`PROTOCOL`]), `epoch` (the epoch of
//! the instance that published it, which changes whenever the instance is placed again: after
//! a move, a restart or a crash), `seq` (one more than the topic's last message) and `kind`:
//!
//! - `snapshot`: the topic's whole public state. It replaces what a client holds.
//! - `delta`: one change to a collection: an `id`, then `type_name` and `props`, or `removed`.
//!   Deltas apply in `seq` order; a client that misses one (a gap in `seq`, or another
//!   `epoch`) asks for a snapshot again.
//! - `changed`: something private changed for the players in `who` (`what` names it). They
//!   read it back with a call the hub answers for its caller alone. A topic has no access
//!   control of its own: anyone who may call a type may subscribe to every topic it has, so
//!   nothing private is ever published.
//! - `event`: something that happened, with nothing to keep.
//!
//! Every kit hub answers [`SNAPSHOT_METHOD`] (`{ topic }`) with that topic's current state as a
//! `snapshot` message whose `seq` is the topic's last. A client subscribes first, then asks,
//! and applies what arrives after the snapshot's `seq`.
//!
//! A call that changes something may carry an `action_id` (at most [`ACTION_ID_MAX`] bytes)
//! beside its arguments. [`Receipts`] keep what each caller's actions were answered, so a call
//! sent again after an unknown outcome (a deadline passed, a link was lost) gets the first
//! answer instead of running twice.
//!
//! A refusal's message starts with a code, `kit:<code>: <text>` ([`error`], [`error_code`]),
//! which clients branch on without reading prose. [`codes`] has the common ones.
//!
//! ```ignore
//! use ckx_sdk::kit::{self, MessageKind, Publisher, Receipts};
//!
//! #[derive(Default, Serialize, Deserialize)]
//! struct Board {
//!     cells: Vec<u8>,
//!     #[serde(default)]
//!     kit: Publisher,
//!     #[serde(default)]
//!     receipts: Receipts,
//! }
//!
//! // in `handle`:
//! "snapshot" => self.kit.snapshot_reply(ctx, "board", &self.view()),
//! "mark" => Receipts::run(self, |b| &mut b.receipts, &call, ctx.now_ms(), |b| {
//!     let at: Mark = kit::args(&call)?;
//!     b.mark(at)?;
//!     b.kit.publish(ctx, "board", MessageKind::Snapshot, &b.view())?;
//!     encode(&true)
//! }),
//! ```

use std::collections::{BTreeMap, VecDeque};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::model::{Model, Published, Schema, Viewer};
use crate::{Call, Caller, Ctx, Error, Result, decode, encode};

pub mod ai;
pub mod play;
pub mod presence;
pub mod rng;
pub mod shard;
pub mod sim;
pub mod wire;

pub use rng::Rng;

/// The protocol version kit messages carry in `v`.
pub const PROTOCOL: u8 = 1;
/// The method every kit hub answers with a topic's snapshot.
pub const SNAPSHOT_METHOD: &str = "snapshot";
/// The longest `action_id` a call may carry, in bytes.
pub const ACTION_ID_MAX: usize = 64;
/// Receipts a hub keeps at most; the oldest go first.
pub const RECEIPTS_MAX: usize = 256;
/// How long a receipt is kept.
pub const RECEIPT_TTL_MS: u64 = 10 * 60 * 1000;
/// The largest reply a receipt keeps. A call sent again whose first reply was larger is
/// refused with [`codes::ALREADY_DONE`]: it ran, and the hub's state has its result.
pub const RECEIPT_REPLY_MAX: usize = 1024;

/// What a [`Message`] is.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Snapshot,
    Delta,
    Changed,
    Event,
}

/// The reserved fields of every kit message.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub v: u8,
    pub epoch: u64,
    pub seq: u64,
    pub kind: MessageKind,
}

/// A kit message as it is read back: its header, and the body's own fields beside it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message<T> {
    #[serde(flatten)]
    pub header: Header,
    #[serde(flatten)]
    pub body: T,
}

/// The body of a `changed` message.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    /// The players who should read again.
    pub who: Vec<u64>,
    /// What changed for them, as the hub's endpoints name it.
    pub what: String,
}

/// The body of a collection's snapshot: every member as a `delta` would show it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Containers {
    pub containers: Vec<Published>,
}

#[derive(Serialize)]
struct Outgoing<'a, T: ?Sized> {
    #[serde(flatten)]
    header: Header,
    #[serde(flatten)]
    body: &'a T,
}

/// The fields a kit message's header owns; a body may not use them (they sit side by side).
pub const RESERVED_FIELDS: [&str; 4] = ["v", "epoch", "seq", "kind"];

/// Encodes one kit message from this instance. `body` must serialize as a struct or a map, and
/// none of its fields may be one of [`RESERVED_FIELDS`]: a debug build (`cargo test`) refuses a
/// body that uses one, which would leave a client two `kind`s to choose from.
pub fn message<T: Serialize + ?Sized>(ctx: &Ctx, kind: MessageKind, seq: u64, body: &T) -> Result<Vec<u8>> {
    #[cfg(debug_assertions)]
    if let Ok(fields) = decode::<BTreeMap<String, serde::de::IgnoredAny>>(&encode(body)?)
        && let Some(taken) = RESERVED_FIELDS.iter().find(|f| fields.contains_key(**f))
    {
        return Err(Error::new(format!("a kit message's body may not have a field `{taken}`: the header owns it")));
    }
    let header = Header { v: PROTOCOL, epoch: ctx.epoch, seq, kind };
    encode(&Outgoing { header, body })
}

/// A hub's topics and how far each has got. Keep it in the hub's state, so `seq` carries on
/// after a snapshot; a client that sees another `epoch` asks for a snapshot again anyway.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Publisher {
    #[serde(default)]
    seqs: BTreeMap<String, u64>,
}

impl Publisher {
    pub fn new() -> Self {
        Self::default()
    }

    /// The topic's last `seq`; 0 before its first message.
    pub fn seq(&self, topic: &str) -> u64 {
        self.seqs.get(topic).copied().unwrap_or(0)
    }

    /// Publishes one message on `topic`.
    pub fn publish<T: Serialize + ?Sized>(&mut self, ctx: &Ctx, topic: &str, kind: MessageKind, body: &T) -> Result<()> {
        let seq = self.seq(topic) + 1;
        let bytes = message(ctx, kind, seq, body)?;
        self.seqs.insert(topic.to_owned(), seq);
        ctx.publish(topic, &bytes);
        Ok(())
    }

    /// Publishes the topic's whole state.
    pub fn snapshot<T: Serialize + ?Sized>(&mut self, ctx: &Ctx, topic: &str, body: &T) -> Result<()> {
        self.publish(ctx, topic, MessageKind::Snapshot, body)
    }

    /// Publishes one change to the topic's collection.
    pub fn delta<T: Serialize + ?Sized>(&mut self, ctx: &Ctx, topic: &str, body: &T) -> Result<()> {
        self.publish(ctx, topic, MessageKind::Delta, body)
    }

    /// Tells `who` that `what` changed for them; nothing private goes on the topic. Does
    /// nothing when `who` is empty.
    pub fn changed(&mut self, ctx: &Ctx, topic: &str, who: &[u64], what: &str) -> Result<()> {
        if who.is_empty() {
            return Ok(());
        }
        self.publish(ctx, topic, MessageKind::Changed, &Changed { who: who.to_vec(), what: what.to_owned() })
    }

    /// Publishes something that happened.
    pub fn event<T: Serialize + ?Sized>(&mut self, ctx: &Ctx, topic: &str, body: &T) -> Result<()> {
        self.publish(ctx, topic, MessageKind::Event, body)
    }

    /// The reply to [`SNAPSHOT_METHOD`] for `topic`: its state now, at its last `seq`.
    pub fn snapshot_reply<T: Serialize + ?Sized>(&self, ctx: &Ctx, topic: &str, body: &T) -> Result<Vec<u8>> {
        message(ctx, MessageKind::Snapshot, self.seq(topic), body)
    }

    /// Publishes the model's public changes since the last call ([`Model::public_changes`]) as
    /// deltas on `topic`, one container each, and returns how many went out. Owner and hidden
    /// properties are never published.
    pub fn model_changes(&mut self, ctx: &Ctx, model: &mut Model, schema: &Schema, topic: &str) -> Result<usize> {
        let changes = model.public_changes(schema);
        for p in &changes {
            self.delta(ctx, topic, p)?;
        }
        Ok(changes.len())
    }

    /// The reply to [`SNAPSHOT_METHOD`] for a topic [`Publisher::model_changes`] feeds: every
    /// container's public view, as [`Containers`].
    pub fn model_snapshot_reply(&self, ctx: &Ctx, model: &Model, schema: &Schema, topic: &str) -> Result<Vec<u8>> {
        self.snapshot_reply(ctx, topic, &public_containers(model, schema)?)
    }
}

/// Every container's public view, as a collection snapshot carries it.
pub fn public_containers(model: &Model, schema: &Schema) -> Result<Containers> {
    let mut containers = Vec::new();
    for id in model.ids() {
        let Some(c) = model.container(id) else { continue };
        let props = model.view(schema, id, Viewer::Public)?;
        containers.push(Published { id, type_name: Some(c.type_name.clone()), props: Some(props), removed: false });
    }
    Ok(Containers { containers })
}

/// The `topic` a [`SNAPSHOT_METHOD`] call names, or `default` when it names none.
pub fn snapshot_topic(call: &Call<'_>, default: &str) -> String {
    #[derive(Deserialize)]
    struct Ask {
        #[serde(default)]
        topic: Option<String>,
    }
    decode::<Ask>(call.payload).ok().and_then(|a| a.topic).filter(|t| !t.is_empty()).unwrap_or_else(|| default.to_owned())
}

/// A call's arguments, refused with [`codes::BAD_REQUEST`] when they do not decode.
pub fn args<T: DeserializeOwned>(call: &Call<'_>) -> Result<T> {
    decode(call.payload).map_err(|Error(e)| error(codes::BAD_REQUEST, format!("`{}`: {e}", call.method)))
}

/// A call's arguments when it may send none: an empty payload or a nil (what a client sends for
/// no arguments) is `T::default()`.
pub fn args_or_default<T: DeserializeOwned + Default>(call: &Call<'_>) -> Result<T> {
    match call.payload {
        [] | [0xc0] => Ok(T::default()),
        _ => args(call),
    }
}

// ---- refusals ----

/// The refusal codes kit hubs share. A hub may add its own: lowercase letters, digits and `_`.
pub mod codes {
    pub const BAD_REQUEST: &str = "bad_request";
    pub const NOT_FOUND: &str = "not_found";
    /// The caller may not do this.
    pub const FORBIDDEN: &str = "forbidden";
    /// The state does not allow it now.
    pub const CONFLICT: &str = "conflict";
    pub const FULL: &str = "full";
    /// A bound the hub keeps (how many, how large).
    pub const LIMIT: &str = "limit";
    /// Too soon after the last time.
    pub const COOLDOWN: &str = "cooldown";
    pub const NOT_YOUR_TURN: &str = "not_your_turn";
    /// The caller has not joined.
    pub const NOT_MEMBER: &str = "not_member";
    pub const STARTED: &str = "started";
    pub const FINISHED: &str = "finished";
    pub const NOT_READY: &str = "not_ready";
    pub const RESERVED: &str = "reserved";
    pub const EMPTY: &str = "empty";
    pub const OUT_OF_REACH: &str = "out_of_reach";
    pub const EXPIRED: &str = "expired";
    /// Not enough of something the caller spends.
    pub const INSUFFICIENT: &str = "insufficient";
    /// The action already ran; its reply was too large to keep ([`super::RECEIPT_REPLY_MAX`]).
    pub const ALREADY_DONE: &str = "already_done";
}

/// A refusal with a code: its message is `kit:<code>: <text>`.
pub fn error(code: &str, text: impl std::fmt::Display) -> Error {
    debug_assert!(is_code(code), "a kit code is lowercase letters, digits and `_`: {code}");
    Error(format!("kit:{code}: {text}"))
}

fn is_code(code: &str) -> bool {
    !code.is_empty() && code.len() <= 64 && code.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The code and text of a refusal [`error`] made, or `None` for any other message.
pub fn error_code(message: &str) -> Option<(&str, &str)> {
    let rest = message.strip_prefix("kit:")?;
    let (code, text) = rest.split_once(": ")?;
    is_code(code).then_some((code, text))
}

// ---- receipts ----

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct Receipt {
    who: String,
    id: String,
    at: u64,
    ok: bool,
    /// The reply, or the refusal's message; empty with `kept` false when it was too large.
    reply: Vec<u8>,
    kept: bool,
}

/// What each caller's recent actions were answered, by `action_id`. Keep it in the hub's
/// state: an action sent again after the hub restarted from its last snapshot is answered
/// from it, if that snapshot came after the action.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Receipts {
    #[serde(default)]
    kept: VecDeque<Receipt>,
}

fn caller_key(caller: Caller) -> Option<String> {
    match caller {
        Caller::Player(p) => Some(format!("p{p}")),
        Caller::Developer(u) => Some(format!("d{u}")),
        Caller::Instance(i) => Some(format!("i{i}")),
        Caller::Platform => None,
    }
}

impl Receipts {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `action_id` a call's arguments carry: a map's `action_id` string. `None` when the
    /// arguments are not a map or carry none; refused when it is empty or too long.
    pub fn action_id(payload: &[u8]) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Carries {
            #[serde(default)]
            action_id: Option<String>,
        }
        let Ok(Carries { action_id }) = decode::<Carries>(payload) else { return Ok(None) };
        match action_id {
            Some(id) if id.is_empty() || id.len() > ACTION_ID_MAX => {
                Err(error(codes::BAD_REQUEST, format!("an action_id is 1 to {ACTION_ID_MAX} bytes")))
            }
            other => Ok(other),
        }
    }

    /// How many receipts are kept.
    pub fn len(&self) -> usize {
        self.kept.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }

    /// The answer `caller` got for `action_id`, if it is kept.
    pub fn replay(&self, caller: Caller, action_id: Option<&str>, now: u64) -> Option<Result<Vec<u8>>> {
        let (who, id) = (caller_key(caller)?, action_id?);
        let r = self.kept.iter().rev().find(|r| r.who == who && r.id == id && now.saturating_sub(r.at) < RECEIPT_TTL_MS)?;
        Some(match (r.ok, r.kept) {
            (_, false) => Err(error(codes::ALREADY_DONE, "this action already ran; read the state for its result")),
            (true, true) => Ok(r.reply.clone()),
            (false, true) => Err(Error(String::from_utf8_lossy(&r.reply).into_owned())),
        })
    }

    /// Keeps what `caller` got for `action_id`; nothing without an action id.
    pub fn record(&mut self, caller: Caller, action_id: Option<&str>, result: &Result<Vec<u8>>, now: u64) {
        let (Some(who), Some(id)) = (caller_key(caller), action_id) else { return };
        while self.kept.front().is_some_and(|r| now.saturating_sub(r.at) >= RECEIPT_TTL_MS) || self.kept.len() >= RECEIPTS_MAX {
            self.kept.pop_front();
        }
        let (ok, bytes) = match result {
            Ok(reply) => (true, reply.clone()),
            Err(Error(message)) => (false, message.clone().into_bytes()),
        };
        let kept = bytes.len() <= RECEIPT_REPLY_MAX;
        let reply = if kept { bytes } else { Vec::new() };
        self.kept.push_back(Receipt { who, id: id.to_owned(), at: now, ok, reply, kept });
    }

    /// Runs `act` on `hub` once per (caller, `action_id`): a call sent again gets the first
    /// answer. `receipts` finds the hub's receipts; a call without an action id just runs.
    pub fn run<H>(
        hub: &mut H,
        receipts: fn(&mut H) -> &mut Receipts,
        call: &Call<'_>,
        now: u64,
        act: impl FnOnce(&mut H) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let id = Self::action_id(call.payload)?;
        if let Some(answer) = receipts(hub).replay(call.caller, id.as_deref(), now) {
            return answer;
        }
        let result = act(hub);
        receipts(hub).record(call.caller, id.as_deref(), &result, now);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PropKind, Visibility};

    fn ctx() -> Ctx {
        Ctx { app: 1, instance: 2, epoch: 7, version: 1, kind: crate::Kind::Hub, node_type: "board".into(), key: "k".into() }
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Cells {
        cells: Vec<u8>,
    }

    #[test]
    fn a_message_is_its_body_with_the_header_beside_it() {
        let bytes = message(&ctx(), MessageKind::Snapshot, 3, &Cells { cells: vec![1, 2] }).unwrap();
        let back: Message<Cells> = decode(&bytes).unwrap();
        assert_eq!(back.header, Header { v: PROTOCOL, epoch: 7, seq: 3, kind: MessageKind::Snapshot });
        assert_eq!(back.body, Cells { cells: vec![1, 2] });
        // An older reader of the body alone still reads it.
        assert_eq!(decode::<Cells>(&bytes).unwrap(), Cells { cells: vec![1, 2] });
        assert!(message(&ctx(), MessageKind::Event, 1, &vec![1u8]).is_err(), "a body is a struct or a map");
    }

    #[test]
    fn a_body_may_not_use_a_field_the_header_owns() {
        let e = message(&ctx(), MessageKind::Event, 1, &serde_json::json!({ "type": "timing", "kind": "lap" })).unwrap_err();
        assert!(e.0.contains("`kind`"), "{}", e.0);
        assert!(message(&ctx(), MessageKind::Event, 1, &serde_json::json!({ "type": "timing", "stage": "lap" })).is_ok());
    }

    #[test]
    fn each_topic_counts_its_own_messages_and_a_snapshot_reply_names_the_last() {
        let mut p = Publisher::new();
        assert_eq!(p.seq("board"), 0);
        let reply: Message<Cells> = decode(&p.snapshot_reply(&ctx(), "board", &Cells { cells: vec![] }).unwrap()).unwrap();
        assert_eq!(reply.header.seq, 0);
        p.seqs.insert("board".into(), 4);
        p.seqs.insert("chat".into(), 1);
        assert_eq!((p.seq("board"), p.seq("chat"), p.seq("other")), (4, 1, 0));
    }

    #[test]
    fn codes_lead_a_refusal_and_read_back() {
        let Error(m) = error(codes::NOT_YOUR_TURN, "it is not your turn");
        assert_eq!(m, "kit:not_your_turn: it is not your turn");
        assert_eq!(error_code(&m), Some(("not_your_turn", "it is not your turn")));
        assert_eq!(error_code("decode: invalid type"), None);
        assert_eq!(error_code("kit:Bad Code: x"), None);
    }

    #[test]
    fn a_receipt_answers_an_action_sent_again_and_only_for_its_caller() {
        let mut r = Receipts::new();
        let id = Some("a1");
        assert!(r.replay(Caller::Player(1), id, 0).is_none());
        r.record(Caller::Player(1), id, &Ok(vec![9]), 0);
        r.record(Caller::Player(1), Some("a2"), &Err(error(codes::FULL, "full")), 0);
        assert_eq!(r.replay(Caller::Player(1), id, 10).unwrap().unwrap(), vec![9]);
        assert_eq!(r.replay(Caller::Player(1), Some("a2"), 10).unwrap().unwrap_err(), error(codes::FULL, "full"));
        assert!(r.replay(Caller::Player(2), id, 10).is_none(), "another caller's id is another action");
        assert!(r.replay(Caller::Player(1), id, RECEIPT_TTL_MS).is_none(), "kept for the TTL only");
        r.record(Caller::Platform, id, &Ok(vec![1]), 0);
        r.record(Caller::Player(1), None, &Ok(vec![1]), 0);
        assert_eq!(r.len(), 2, "no receipt without a caller to answer or an id");
    }

    #[test]
    fn receipts_are_bounded_and_a_large_reply_is_not_kept() {
        let mut r = Receipts::new();
        for i in 0..RECEIPTS_MAX + 10 {
            r.record(Caller::Player(1), Some(&i.to_string()), &Ok(vec![1]), 0);
        }
        assert_eq!(r.len(), RECEIPTS_MAX);
        assert!(r.replay(Caller::Player(1), Some("0"), 0).is_none(), "the oldest went first");
        r.record(Caller::Player(1), Some("big"), &Ok(vec![0; RECEIPT_REPLY_MAX + 1]), 0);
        let e = r.replay(Caller::Player(1), Some("big"), 0).unwrap().unwrap_err();
        assert_eq!(error_code(&e.0).map(|c| c.0), Some(codes::ALREADY_DONE));
    }

    #[test]
    fn an_action_id_rides_in_a_map_of_arguments() {
        #[derive(Serialize)]
        struct Act<'a> {
            points: i64,
            action_id: &'a str,
        }
        assert_eq!(Receipts::action_id(&encode(&Act { points: 1, action_id: "x" }).unwrap()).unwrap().as_deref(), Some("x"));
        assert_eq!(Receipts::action_id(&encode(&()).unwrap()).unwrap(), None);
        assert_eq!(Receipts::action_id(&encode(&5u8).unwrap()).unwrap(), None);
        let long = "x".repeat(ACTION_ID_MAX + 1);
        assert!(Receipts::action_id(&encode(&Act { points: 1, action_id: &long }).unwrap()).is_err());
    }

    #[test]
    fn no_arguments_are_the_defaults_and_bad_ones_are_a_bad_request() {
        #[derive(Deserialize, Default, Debug, PartialEq)]
        #[serde(default)]
        struct Open {
            seed: Option<u64>,
        }
        let call = |payload: &'static [u8]| Call { caller: Caller::Player(1), method: "open", payload };
        assert_eq!(args_or_default::<Open>(&call(&[])).unwrap(), Open::default());
        assert_eq!(args_or_default::<Open>(&call(&[0xc0])).unwrap(), Open::default());
        let given = encode(&serde_json::json!({ "seed": 4 })).unwrap();
        let c = Call { caller: Caller::Player(1), method: "open", payload: &given };
        assert_eq!(args_or_default::<Open>(&c).unwrap(), Open { seed: Some(4) });
        let Error(e) = args_or_default::<Open>(&call(&[0x05])).unwrap_err();
        assert_eq!(error_code(&e).map(|c| c.0), Some(codes::BAD_REQUEST));
    }

    #[test]
    fn a_collection_snapshot_holds_every_public_view() {
        let schema = Schema::new()
            .container("Mob", |c| c.prop("hp", PropKind::Int, 10, Visibility::Public).prop("seed", PropKind::Int, 0, Visibility::Hidden));
        let mut model = Model::new();
        model.create(&schema, "Mob", None).unwrap();
        model.create(&schema, "Mob", Some(3)).unwrap();
        let all = public_containers(&model, &schema).unwrap();
        assert_eq!(all.containers.len(), 2);
        assert!(all.containers.iter().all(|c| c.props.as_ref().is_some_and(|p| p.contains_key("hp") && !p.contains_key("seed"))));
    }
}
