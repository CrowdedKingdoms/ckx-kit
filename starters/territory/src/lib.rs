//! Space players hold, for two node types from one crate. The hub reads where players are with
//! the node API (`ckx_sdk::kit::presence`, scope `world.read`): a client never says it is on a
//! point or inside the circle.
//!
//! - **`territory`**, keyed by its map: control points that the factions capture by having more
//!   members standing on them than any other faction has, with capture progress that decays when
//!   nobody presses it, income for each point held, and a winning score. The shape of a
//!   conquest mode, a guild war, a king-of-the-hill. Endpoints: `join { faction }` (once; a
//!   player keeps their faction), `reset` (developers: a new war), `state` (anyone). Topic
//!   `territory`: `snapshot` messages when a point moves, `event` messages (`type`: `captured`,
//!   `income`, `won`); a point changing hands is also realtime event 96 at the point.
//! - **`zone`**, keyed by its match: a circle that holds, then shrinks, phase by phase
//!   (`ckx_sdk::kit::sim::zones`), and wears down every player outside it until one is left. The
//!   battle royale's circle; pair it with an `arena` for the fighting. Endpoints: `join`, `start`
//!   (the first to join, or a developer), `state` (anyone). Topic `zone`: `snapshot` messages
//!   each scan, `event` messages (`type`: `warning`, `shrinking`, `settled`, `final`,
//!   `eliminated`, `won`); the circle's moves are also realtime event 98 at its centre. It sees
//!   players within about ten chunks of its centre (nine probes of the node API's largest
//!   radius), so its first circle is at most that.
//!
//! A type runs as the one its seed names (`{"mode": "zone", ...}`), or as the one its name is:
//! name a type something else and give it a seed with its `mode`. Both speak the kit protocol
//! (`ckx_sdk::kit`): `snapshot { topic }` answers each topic; changes may carry an `action_id`;
//! refusals carry `kit:<code>:` codes.

use std::collections::BTreeMap;

use ckx_sdk::kit::presence::{self, Seen};
use ckx_sdk::kit::sim::zones::{ShrinkPhase, ShrinkingZone, ZoneEvent};
use ckx_sdk::kit::{self, Publisher, Receipts, codes, wire};
use ckx_sdk::prelude::*;
use serde_json::{Value, json};

pub const TERRITORY_TOPIC: &str = "territory";
pub const ZONE_TOPIC: &str = "zone";
const SCAN: &str = "scan";
const INCOME: &str = "income";
/// The node API's largest radius, and the spacing of the zone's probes (each sees 7 chunks
/// across).
const PROBE_RADIUS: u8 = 3;
const PROBE_STEP: i64 = 7;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Territory,
    Zone,
}

/// The type this instance runs as: its seed's `mode`, or its name.
pub fn mode_of(node_type: &str, seed: &[u8]) -> Result<Mode> {
    #[derive(Deserialize)]
    struct Named {
        #[serde(default)]
        mode: Option<Mode>,
    }
    if !seed.is_empty() {
        let named: Named = serde_json::from_slice(seed).map_err(|e| Error::new(format!("seed is not a JSON config: {e}")))?;
        if let Some(mode) = named.mode {
            return Ok(mode);
        }
    }
    match node_type {
        "territory" => Ok(Mode::Territory),
        "zone" => Ok(Mode::Zone),
        other => Err(Error::new(format!(
            "the territory crate runs a `territory` or a `zone`; give `{other}` a seed with its mode: {{\"mode\": \"zone\"}}"
        ))),
    }
}

fn config<T: serde::de::DeserializeOwned + Default>(seed: &[u8]) -> Result<T> {
    if seed.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(seed).map_err(|e| Error::new(format!("seed is not a JSON config: {e}")))
}

fn with_type(kind: &str, extra: Value) -> Value {
    let mut e = json!({ "type": kind });
    if let (Some(e), Some(x)) = (e.as_object_mut(), extra.as_object()) {
        e.extend(x.clone());
    }
    e
}

// ---- territory ----

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Point {
    pub id: String,
    pub x: i64,
    #[serde(default)]
    pub y: i64,
    pub z: i64,
    /// Chunks around it that count as on it (at most 3).
    #[serde(default = "default_radius")]
    pub radius: u8,
}

fn default_radius() -> u8 {
    1
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TerritoryConfig {
    pub points: Vec<Point>,
    pub factions: Vec<String>,
    /// Capture progress a second for each member more than the next faction has there.
    pub capture_per_sec: f64,
    /// Progress lost a second when nobody presses the capture.
    pub decay_per_sec: f64,
    pub scan_ms: u64,
    pub income_ms: u64,
    pub income_per_point: i64,
    /// The score that wins the war; none for a war without end.
    pub win_score: Option<i64>,
    pub members_max: usize,
}

impl Default for TerritoryConfig {
    fn default() -> Self {
        let point = |id: &str, x: i64| Point { id: id.into(), x, y: 0, z: 0, radius: 1 };
        Self {
            points: vec![point("mill", -8), point("bridge", 0), point("tower", 8)],
            factions: vec!["red".into(), "blue".into()],
            capture_per_sec: 0.1,
            decay_per_sec: 0.05,
            scan_ms: 2_000,
            income_ms: 60_000,
            income_per_point: 10,
            win_score: Some(500),
            members_max: 500,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PointState {
    pub id: String,
    pub owner: Option<String>,
    /// The faction pressing a capture, and how far it is, 0 to 1.
    pub capturing: Option<String>,
    pub progress: f64,
    /// Members on the point at the last scan, by faction.
    pub present: BTreeMap<String, u32>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarPhase {
    #[default]
    Open,
    Won,
}

/// The war as everyone sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TerritoryView {
    pub phase: WarPhase,
    pub points: Vec<PointState>,
    pub scores: BTreeMap<String, i64>,
    pub members: BTreeMap<String, u32>,
    pub winner: Option<String>,
}

#[derive(Deserialize)]
struct Join {
    faction: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Territory {
    pub config: TerritoryConfig,
    pub phase: WarPhase,
    pub members: BTreeMap<u64, String>,
    pub points: Vec<PointState>,
    pub scores: BTreeMap<String, i64>,
    pub winner: Option<String>,
    pub armed: bool,
    #[serde(skip)]
    pub events: Vec<Value>,
    #[serde(skip)]
    pub moved: bool,
}

impl Territory {
    pub fn new(config: TerritoryConfig) -> Self {
        let mut t = Self { config, ..Self::default() };
        t.reset();
        t
    }

    pub fn reset(&mut self) {
        self.phase = WarPhase::Open;
        self.winner = None;
        self.points = self.config.points.iter().map(|p| PointState { id: p.id.clone(), ..PointState::default() }).collect();
        self.scores = self.config.factions.iter().map(|f| (f.clone(), 0)).collect();
        self.moved = true;
    }

    pub fn view(&self) -> TerritoryView {
        let mut members: BTreeMap<String, u32> = self.config.factions.iter().map(|f| (f.clone(), 0)).collect();
        for f in self.members.values() {
            *members.entry(f.clone()).or_insert(0) += 1;
        }
        TerritoryView { phase: self.phase, points: self.points.clone(), scores: self.scores.clone(), members, winner: self.winner.clone() }
    }

    pub fn join(&mut self, player: u64, faction: &str) -> Result<()> {
        if !self.config.factions.iter().any(|f| f == faction) {
            return Err(kit::error(codes::BAD_REQUEST, format!("a faction is one of {}", self.config.factions.join(", "))));
        }
        match self.members.get(&player) {
            Some(f) if f == faction => Ok(()),
            Some(f) => Err(kit::error(codes::CONFLICT, format!("you fight for {f}"))),
            None if self.members.len() >= self.config.members_max => Err(kit::error(codes::FULL, "the war has all the members it takes")),
            None => {
                self.members.insert(player, faction.to_owned());
                self.moved = true;
                Ok(())
            }
        }
    }

    /// One scan: `seen[i]` is who stands on point `i`. Moves the captures over `dt_ms`.
    pub fn scan(&mut self, seen: &[Vec<Seen>], dt_ms: u64) {
        if self.phase != WarPhase::Open {
            return;
        }
        let dt = dt_ms as f64 / 1000.0;
        for (i, point) in self.points.iter_mut().enumerate() {
            let mut present: BTreeMap<String, u32> = BTreeMap::new();
            for s in seen.get(i).map(Vec::as_slice).unwrap_or_default() {
                if let Some(f) = self.members.get(&s.player) {
                    *present.entry(f.clone()).or_insert(0) += 1;
                }
            }
            let mut counts: Vec<(&String, u32)> = present.iter().map(|(f, n)| (f, *n)).collect();
            counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            let lead = counts.first().map(|(f, n)| ((*f).clone(), n - counts.get(1).map_or(0, |c| c.1)));
            let before = (point.owner.clone(), point.capturing.clone(), point.progress);
            match lead {
                Some((leader, margin)) if margin > 0 && point.owner.as_ref() != Some(&leader) => {
                    if point.capturing.as_ref() != Some(&leader) {
                        point.capturing = Some(leader.clone());
                        point.progress = 0.0;
                    }
                    point.progress += self.config.capture_per_sec * f64::from(margin) * dt;
                    if point.progress >= 1.0 {
                        let previous = point.owner.replace(leader.clone());
                        point.capturing = None;
                        point.progress = 0.0;
                        self.events.push(with_type("captured", json!({ "point": point.id, "owner": leader, "previous": previous })));
                    }
                }
                // Contested: the capture holds where it is.
                Some((_, 0)) => {}
                _ => {
                    point.progress = (point.progress - self.config.decay_per_sec * dt).max(0.0);
                    if point.progress == 0.0 {
                        point.capturing = None;
                    }
                }
            }
            if before != (point.owner.clone(), point.capturing.clone(), point.progress) || point.present != present {
                self.moved = true;
            }
            point.present = present;
        }
    }

    /// Income for each point held; the first faction to the winning score wins.
    pub fn income(&mut self) {
        if self.phase != WarPhase::Open {
            return;
        }
        let mut paid: BTreeMap<String, i64> = BTreeMap::new();
        for p in &self.points {
            if let Some(owner) = &p.owner {
                *paid.entry(owner.clone()).or_insert(0) += self.config.income_per_point;
            }
        }
        if paid.is_empty() {
            return;
        }
        for (faction, gold) in &paid {
            *self.scores.entry(faction.clone()).or_insert(0) += gold;
        }
        self.events.push(with_type("income", json!({ "paid": paid })));
        self.moved = true;
        if let Some(target) = self.config.win_score
            && let Some((faction, _)) = self.scores.iter().filter(|(_, s)| **s >= target).max_by_key(|(_, s)| **s)
        {
            self.phase = WarPhase::Won;
            self.winner = Some(faction.clone());
            self.events.push(with_type("won", json!({ "faction": faction })));
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct TerritoryHub {
    #[serde(flatten)]
    pub war: Territory,
    #[serde(default)]
    pub kit: Publisher,
    #[serde(default)]
    pub receipts: Receipts,
}

impl TerritoryHub {
    fn act(&mut self, ctx: &Ctx, call: &Call<'_>) -> Result<Vec<u8>> {
        let out = match call.method {
            "join" => {
                let j: Join = kit::args(call)?;
                self.war.join(call.player()?, &j.faction)?;
                self.arm(ctx)?;
                encode(&true)
            }
            "reset" => {
                call.developer().map_err(|_| kit::error(codes::FORBIDDEN, "developers start a new war"))?;
                self.war.reset();
                self.arm(ctx)?;
                encode(&true)
            }
            other => return Err(Error::unknown_method(other)),
        };
        self.flush(ctx)?;
        out
    }

    fn arm(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.war.armed {
            ctx.timer_every(SCAN, self.war.config.scan_ms.max(500))?;
            ctx.timer_every(INCOME, self.war.config.income_ms.max(1_000))?;
            self.war.armed = true;
        }
        Ok(())
    }

    fn flush(&mut self, ctx: &Ctx) -> Result<()> {
        for e in std::mem::take(&mut self.war.events) {
            self.kit.event(ctx, TERRITORY_TOPIC, &e)?;
            if e["type"] == "captured" {
                self.realtime(ctx, &e);
            }
        }
        if std::mem::take(&mut self.war.moved) {
            self.kit.snapshot(ctx, TERRITORY_TOPIC, &self.war.view())?;
        }
        Ok(())
    }

    fn realtime(&self, ctx: &Ctx, e: &Value) {
        let Some(point) = self.war.config.points.iter().find(|p| e["point"] == p.id.as_str()) else { return };
        let body = wire::ControlPointChange {
            point_id: point.id.clone(),
            owner: e["owner"].as_str().unwrap_or_default().into(),
            previous_owner: e["previous"].as_str().unwrap_or_default().into(),
            progress: 1.0,
        };
        let actor = actor_id(&format!("point:{}", point.id));
        if let Err(Error(why)) = wire::emit_event(ctx, ChunkPos::new(point.x, point.y, point.z), 4, &actor, wire::CONTROL_POINT, &body) {
            ctx.log(Level::Warn, &format!("a capture's realtime event did not go out: {why}"));
        }
    }

    fn on_scan(&mut self, ctx: &Ctx) -> Result<()> {
        let mut seen = Vec::with_capacity(self.war.config.points.len());
        for p in &self.war.config.points {
            match presence::nearby(ctx, ChunkPos::new(p.x, p.y, p.z), p.radius.min(PROBE_RADIUS)) {
                Ok(s) => seen.push(s),
                Err(e) => {
                    ctx.log(Level::Warn, &format!("point {} was not scanned: {}", p.id, e.message));
                    return Ok(());
                }
            }
        }
        self.war.scan(&seen, self.war.config.scan_ms);
        self.flush(ctx)
    }
}

// ---- zone ----

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ZoneConfig {
    /// The circle's centre, in chunks.
    pub center: (f64, f64),
    pub y: i64,
    /// The first circle's radius, in chunks (at most about ten).
    pub radius: f64,
    pub phases: Vec<ShrinkPhase>,
    pub hp: i64,
    /// What a second outside the circle costs.
    pub damage_per_sec: i64,
    pub scan_ms: u64,
    pub players_max: usize,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        Self {
            center: (0.5, 0.5),
            y: 0,
            radius: 10.0,
            phases: vec![
                ShrinkPhase { radius: 6.0, hold_ms: 60_000, shrink_ms: 30_000 },
                ShrinkPhase { radius: 3.0, hold_ms: 45_000, shrink_ms: 20_000 },
                ShrinkPhase { radius: 1.0, hold_ms: 30_000, shrink_ms: 15_000 },
            ],
            hp: 100,
            damage_per_sec: 5,
            scan_ms: 1_000,
            players_max: 64,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchPhase {
    #[default]
    Lobby,
    Running,
    Finished,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Contender {
    pub player: u64,
    pub hp: i64,
    /// Outside the circle at the last scan (or nowhere the probes saw).
    pub outside: bool,
    /// 1 for the winner; set as each player is eliminated.
    pub place: Option<u32>,
}

/// The match as everyone sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ZoneView {
    pub phase: MatchPhase,
    pub center: (f64, f64),
    pub radius: f64,
    /// The phase under way, and the radius it shrinks to.
    pub stage: Option<usize>,
    pub next_radius: Option<f64>,
    pub players: Vec<Contender>,
    pub alive: u32,
    pub winner: Option<u64>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Zone {
    pub config: ZoneConfig,
    pub phase: MatchPhase,
    pub circle: ShrinkingZone,
    pub players: Vec<Contender>,
    pub stage: Option<usize>,
    pub winner: Option<u64>,
    #[serde(skip)]
    pub events: Vec<Value>,
}

impl Zone {
    pub fn view(&self, now: u64) -> ZoneView {
        let radius = if self.phase == MatchPhase::Lobby { self.config.radius } else { f64::from(self.circle.radius(now)) };
        ZoneView {
            phase: self.phase,
            center: self.config.center,
            radius,
            stage: self.stage,
            next_radius: self.stage.and_then(|s| self.config.phases.get(s)).map(|p| f64::from(p.radius)),
            players: self.players.clone(),
            alive: self.alive() as u32,
            winner: self.winner,
        }
    }

    fn alive(&self) -> usize {
        self.players.iter().filter(|p| p.place.is_none()).count()
    }

    pub fn join(&mut self, player: u64) -> Result<()> {
        if self.phase != MatchPhase::Lobby {
            return Err(kit::error(codes::STARTED, "the match is under way"));
        }
        if self.players.iter().any(|p| p.player == player) {
            return Ok(());
        }
        if self.players.len() >= self.config.players_max {
            return Err(kit::error(codes::FULL, "the match is full"));
        }
        self.players.push(Contender { player, hp: self.config.hp, outside: false, place: None });
        Ok(())
    }

    pub fn start(&mut self, now: u64) -> Result<()> {
        if self.phase != MatchPhase::Lobby {
            return Err(kit::error(codes::STARTED, "the match is under way"));
        }
        if self.players.len() < 2 {
            return Err(kit::error(codes::NOT_READY, "a match takes two"));
        }
        let c = &self.config;
        self.circle = ShrinkingZone::start((c.center.0 as f32, c.center.1 as f32), c.radius as f32, c.phases.clone(), now);
        self.phase = MatchPhase::Running;
        Ok(())
    }

    /// Where the probes go: the centre, and a ring around it when the circle is wider than one.
    pub fn probes(&self) -> Vec<ChunkPos> {
        let (cx, cz) = (self.config.center.0.floor() as i64, self.config.center.1.floor() as i64);
        let reach: &[i64] = if self.config.radius > f64::from(PROBE_RADIUS) + 0.5 { &[-PROBE_STEP, 0, PROBE_STEP] } else { &[0] };
        let mut out = Vec::new();
        for dx in reach {
            for dz in reach {
                out.push(ChunkPos::new(cx + dx, self.config.y, cz + dz));
            }
        }
        out
    }

    /// One scan: who the probes saw, `dt_ms` since the last.
    pub fn scan(&mut self, seen: &[Seen], now: u64, dt_ms: u64) {
        if self.phase != MatchPhase::Running {
            return;
        }
        for e in self.circle.poll(now) {
            let (kind, body) = match e {
                ZoneEvent::Warning { phase, to_radius, shrink_starts_at_ms } => {
                    self.stage = Some(phase);
                    ("warning", json!({ "phase": phase, "to_radius": to_radius, "shrinks_at": shrink_starts_at_ms }))
                }
                ZoneEvent::Shrinking { phase, to_radius } => ("shrinking", json!({ "phase": phase, "to_radius": to_radius })),
                ZoneEvent::Settled { phase, radius } => ("settled", json!({ "phase": phase, "radius": radius })),
                ZoneEvent::Final { radius } => ("final", json!({ "radius": radius })),
            };
            self.events.push(with_type(kind, body));
        }
        let cost = (self.config.damage_per_sec * dt_ms as i64 + 999) / 1000;
        let before: BTreeMap<u64, i64> = self.players.iter().map(|p| (p.player, p.hp)).collect();
        let mut fallen = Vec::new();
        for p in self.players.iter_mut().filter(|p| p.place.is_none()) {
            let inside = seen.iter().find(|s| s.player == p.player).is_some_and(|s| self.circle.contains((s.x as f32, s.z as f32), now));
            p.outside = !inside;
            if !inside {
                p.hp = (p.hp - cost).max(0);
                if p.hp == 0 {
                    fallen.push(p.player);
                }
            }
        }
        let alive_before = self.alive();
        if fallen.len() == alive_before {
            // Everyone left fell at once: the one who had the most stands.
            let best = fallen.iter().map(|p| before[p]).max().unwrap_or(0);
            let top: Vec<u64> = fallen.iter().copied().filter(|p| before[p] == best).collect();
            if top.len() == 1 {
                fallen.retain(|p| *p != top[0]);
                if let Some(c) = self.players.iter_mut().find(|c| c.player == top[0]) {
                    c.hp = 1;
                }
            }
        }
        let mut place = alive_before as u32;
        for player in fallen {
            if let Some(c) = self.players.iter_mut().find(|c| c.player == player) {
                c.place = Some(place);
                self.events.push(with_type("eliminated", json!({ "player": player, "place": place })));
                place -= 1;
            }
        }
        if self.alive() <= 1 {
            self.phase = MatchPhase::Finished;
            self.winner = self.players.iter().find(|p| p.place.is_none()).map(|p| p.player);
            if let Some(w) = self.players.iter_mut().find(|p| p.place.is_none()) {
                w.place = Some(1);
            }
            self.events.push(with_type("won", json!({ "player": self.winner })));
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ZoneHub {
    #[serde(flatten)]
    pub zone: Zone,
    #[serde(default)]
    pub kit: Publisher,
    #[serde(default)]
    pub receipts: Receipts,
}

impl ZoneHub {
    fn act(&mut self, ctx: &Ctx, call: &Call<'_>, now: u64) -> Result<Vec<u8>> {
        let out = match call.method {
            "join" => {
                self.zone.join(call.player()?)?;
                encode(&true)
            }
            "start" => {
                let allowed = match call.caller {
                    Caller::Developer(_) => true,
                    Caller::Player(p) => self.zone.players.first().map(|c| c.player) == Some(p),
                    _ => false,
                };
                if !allowed {
                    return Err(kit::error(codes::FORBIDDEN, "the first to join, or a developer, starts it"));
                }
                self.zone.start(now)?;
                ctx.timer_every(SCAN, self.zone.config.scan_ms.max(250))?;
                encode(&true)
            }
            other => return Err(Error::unknown_method(other)),
        };
        self.flush(ctx, now)?;
        out
    }

    fn flush(&mut self, ctx: &Ctx, now: u64) -> Result<()> {
        for e in std::mem::take(&mut self.zone.events) {
            self.kit.event(ctx, ZONE_TOPIC, &e)?;
            self.realtime(ctx, &e, now);
        }
        self.kit.snapshot(ctx, ZONE_TOPIC, &self.zone.view(now))
    }

    fn realtime(&self, ctx: &Ctx, e: &Value, now: u64) {
        let kind = match e["type"].as_str() {
            Some("warning" | "settled") => "hold",
            Some("shrinking") => "shrink",
            Some("final") => "closed",
            _ => return,
        };
        let c = &self.zone.config;
        let body = wire::ZoneChange {
            kind: kind.into(),
            phase: e["phase"].as_u64().map(|p| p as u32),
            radius_now: f64::from(self.zone.circle.radius(now)),
            center_x: c.center.0,
            center_z: c.center.1,
        };
        let at = ChunkPos::new(c.center.0.floor() as i64, c.y, c.center.1.floor() as i64);
        let actor = actor_id(&format!("zone:{}", ctx.key));
        if let Err(Error(why)) = wire::emit_event(ctx, at, 8, &actor, wire::ZONE_CHANGE, &body) {
            ctx.log(Level::Warn, &format!("the circle's realtime event did not go out: {why}"));
        }
    }

    fn on_scan(&mut self, ctx: &Ctx) -> Result<()> {
        let now = ctx.now_ms();
        let mut seen: Vec<Seen> = Vec::new();
        for at in self.zone.probes() {
            match presence::nearby(ctx, at, PROBE_RADIUS) {
                Ok(found) => {
                    for s in found {
                        if !seen.iter().any(|x| x.player == s.player) {
                            seen.push(s);
                        }
                    }
                }
                Err(e) => {
                    ctx.log(Level::Warn, &format!("the circle was not scanned: {}", e.message));
                    return Ok(());
                }
            }
        }
        self.zone.scan(&seen, now, self.zone.config.scan_ms);
        if self.zone.phase == MatchPhase::Finished {
            ctx.cancel_timer(SCAN);
            ctx.persist_now();
        }
        self.flush(ctx, now)
    }
}

// ---- the node ----

/// The crate's hub: one of the two types.
#[derive(Serialize, Deserialize, Debug)]
pub enum TerritoryNode {
    Territory(TerritoryHub),
    Zone(ZoneHub),
}

impl Hub for TerritoryNode {
    fn spawn(ctx: &Ctx, seed: &[u8]) -> Result<Self> {
        Ok(match mode_of(&ctx.node_type, seed)? {
            Mode::Territory => Self::Territory(TerritoryHub { war: Territory::new(config(seed)?), ..TerritoryHub::default() }),
            Mode::Zone => Self::Zone(ZoneHub { zone: Zone { config: config(seed)?, ..Zone::default() }, ..ZoneHub::default() }),
        })
    }

    fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
        decode(snapshot)
    }

    fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
        encode(self)
    }

    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        let now = ctx.now_ms();
        match self {
            Self::Territory(h) => match call.method {
                "state" => encode(&h.war.view()),
                kit::SNAPSHOT_METHOD => h.kit.snapshot_reply(ctx, TERRITORY_TOPIC, &h.war.view()),
                _ => Receipts::run(h, |h| &mut h.receipts, &call, now, |h| h.act(ctx, &call)),
            },
            Self::Zone(h) => match call.method {
                "state" => encode(&h.zone.view(now)),
                kit::SNAPSHOT_METHOD => h.kit.snapshot_reply(ctx, ZONE_TOPIC, &h.zone.view(now)),
                _ => Receipts::run(h, |h| &mut h.receipts, &call, now, |h| h.act(ctx, &call, now)),
            },
        }
    }

    fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
        match (self, timer) {
            (Self::Territory(h), SCAN) => h.on_scan(ctx),
            (Self::Territory(h), INCOME) => {
                h.war.income();
                h.flush(ctx)
            }
            (Self::Zone(h), SCAN) => h.on_scan(ctx),
            _ => Ok(()),
        }
    }
}

ckx_sdk::export_hub!(TerritoryNode);

#[cfg(test)]
mod tests {
    use super::*;
    use ckx_sdk::kit::{Message, MessageKind, error_code};
    use ckx_sdk::testing::TestHub;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn seen(player: u64, x: f64, z: f64) -> Seen {
        Seen { player, uuid: format!("u{player}"), x, y: 0.5, z, precise: true }
    }

    /// Answers `world.actors_radius` with the players in `world` within the probe's square.
    fn world_of(hub: &TestHub<TerritoryNode>) -> Rc<RefCell<Vec<(u64, i64, i64)>>> {
        let world: Rc<RefCell<Vec<(u64, i64, i64)>>> = Rc::default();
        let w = world.clone();
        hub.answer_node("world.actors_radius", move |body| {
            let n = |k: &str| body[k].as_str().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            let r = body["radiusXz"].as_i64().unwrap_or(0);
            let actors: Vec<Value> = w
                .borrow()
                .iter()
                .filter(|(_, x, z)| (x - n("x")).abs() <= r && (z - n("z")).abs() <= r)
                .map(|(p, x, z)| json!({ "uuid": format!("u{p}"), "userId": p.to_string(), "chunkX": x.to_string(), "chunkY": "0", "chunkZ": z.to_string() }))
                .collect();
            Ok(json!({ "actors": actors }))
        });
        world
    }

    #[test]
    fn a_type_runs_as_its_seed_says_or_as_its_name() {
        assert_eq!(mode_of("zone", b"").unwrap(), Mode::Zone);
        assert_eq!(mode_of("territory", b"").unwrap(), Mode::Territory);
        assert_eq!(mode_of("br_circle", br#"{"mode":"zone","hp":50}"#).unwrap(), Mode::Zone);
        assert!(mode_of("conquest", b"").is_err());
    }

    #[test]
    fn the_faction_with_more_on_a_point_captures_it_and_a_contest_holds_it() {
        let mut war = Territory::new(TerritoryConfig { capture_per_sec: 0.25, ..TerritoryConfig::default() });
        war.join(1, "red").unwrap();
        war.join(2, "red").unwrap();
        war.join(3, "blue").unwrap();
        let Error(e) = war.join(1, "blue").unwrap_err();
        assert_eq!(error_code(&e).map(|c| c.0), Some(codes::CONFLICT));
        // Two red on the mill against nobody: 0.5 a second, captured in two.
        war.scan(&[vec![seen(1, -8.5, 0.5), seen(2, -8.2, 0.4)], vec![], vec![]], 1_000);
        assert_eq!((war.points[0].capturing.as_deref(), war.points[0].progress), (Some("red"), 0.5));
        war.scan(&[vec![seen(1, -8.5, 0.5), seen(2, -8.2, 0.4)], vec![], vec![]], 1_000);
        assert_eq!(war.points[0].owner.as_deref(), Some("red"));
        assert!(war.events.iter().any(|e| e["type"] == "captured"));
        // One blue alone on the bridge starts it; one red joining contests it and it holds.
        war.scan(&[vec![], vec![seen(3, 0.5, 0.5)], vec![]], 2_000);
        let p = war.points[1].progress;
        war.scan(&[vec![], vec![seen(3, 0.5, 0.5), seen(1, 0.4, 0.4)], vec![]], 2_000);
        assert_eq!(war.points[1].progress, p);
        // Nobody: it decays.
        war.scan(&[vec![], vec![], vec![]], 2_000);
        assert!(war.points[1].progress < p);
        // Income for the mill, then the win.
        war.config.win_score = Some(20);
        war.income();
        war.income();
        assert_eq!((war.phase, war.winner.as_deref()), (WarPhase::Won, Some("red")));
    }

    #[test]
    fn the_territory_hub_scans_where_players_stand_and_sends_a_capture_as_a_realtime_event() {
        let mut hub = TestHub::<TerritoryNode>::spawn_with("territory", "valley", br#"{"capture_per_sec":0.5,"scan_ms":1000}"#).unwrap();
        let world = world_of(&hub);
        let _: bool = hub.call_typed(Caller::Player(1), "join", &json!({ "faction": "blue" })).unwrap();
        world.borrow_mut().push((1, 8, 0));
        hub.advance(2_500).unwrap();
        let view: TerritoryView = hub.call_typed(Caller::Player(1), "state", &()).unwrap();
        assert_eq!(view.points[2].owner.as_deref(), Some("blue"), "{view:?}");
        assert_eq!(view.members["blue"], 1);
        assert_eq!(hub.emitted().len(), 1, "the capture went out as event 96");
        let e = hub.call(Caller::Player(1), "reset", &encode(&()).unwrap()).unwrap_err();
        assert_eq!(error_code(&e.message).map(|c| c.0), Some(codes::FORBIDDEN));
        let _: bool = hub.call_typed(Caller::Developer(9), "reset", &()).unwrap();
        let snap: Message<TerritoryView> =
            decode(&hub.call(Caller::Player(1), kit::SNAPSHOT_METHOD, &encode(&json!({ "topic": TERRITORY_TOPIC })).unwrap()).unwrap())
                .unwrap();
        assert_eq!(snap.body.points[2].owner, None);
    }

    #[test]
    fn the_circle_wears_down_the_players_outside_it_until_one_is_left() {
        let config = ZoneConfig {
            radius: 6.0,
            phases: vec![ShrinkPhase { radius: 2.0, hold_ms: 2_000, shrink_ms: 2_000 }],
            hp: 10,
            damage_per_sec: 5,
            ..ZoneConfig::default()
        };
        let mut zone = Zone { config, ..Zone::default() };
        zone.join(1).unwrap();
        zone.join(2).unwrap();
        zone.join(3).unwrap();
        zone.start(0).unwrap();
        let inner = seen(1, 0.5, 0.5);
        let edge = seen(2, 4.5, 0.5);
        // 3 is nowhere the probes see: outside.
        zone.scan(&[inner.clone(), edge.clone()], 1_000, 1_000);
        assert_eq!(zone.players.iter().map(|p| p.hp).collect::<Vec<_>>(), vec![10, 10, 5]);
        zone.scan(&[inner.clone(), edge.clone()], 2_000, 1_000);
        assert_eq!(zone.players[2].place, Some(3));
        // The circle shrinks to 2: the edge is outside now.
        for t in [3_000, 4_000, 5_000] {
            zone.scan(&[inner.clone(), edge.clone()], t, 1_000);
        }
        assert_eq!((zone.phase, zone.winner), (MatchPhase::Finished, Some(1)));
        assert_eq!(zone.players[1].place, Some(2));
        let kinds: Vec<&str> = zone.events.iter().filter_map(|e| e["type"].as_str()).collect();
        assert!(
            kinds.contains(&"warning") && kinds.contains(&"shrinking") && kinds.contains(&"eliminated") && kinds.contains(&"won"),
            "{kinds:?}"
        );
    }

    #[test]
    fn the_zone_hub_probes_around_its_centre_and_publishes_each_scan() {
        let mut hub = TestHub::<TerritoryNode>::spawn("zone", "match-1").unwrap();
        let world = world_of(&hub);
        world.borrow_mut().extend([(1, 0, 0), (2, 9, 9)]);
        let _: bool = hub.call_typed(Caller::Player(1), "join", &()).unwrap();
        let _: bool = hub.call_typed(Caller::Player(2), "join", &()).unwrap();
        let _: bool = hub.call_typed(Caller::Player(1), "start", &()).unwrap();
        hub.advance(1_000).unwrap();
        let probes = hub.node_requests().iter().filter(|(op, _)| op == "world.actors_radius").count();
        assert_eq!(probes, 9);
        let view: ZoneView = hub.call_typed(Caller::Player(1), "state", &()).unwrap();
        assert_eq!(view.players.iter().map(|p| p.outside).collect::<Vec<_>>(), vec![false, true]);
        #[derive(Deserialize)]
        struct K {
            kind: MessageKind,
        }
        assert!(hub.published().iter().any(|(_, b)| decode::<K>(b).unwrap().kind == MessageKind::Event), "the first warning");
        assert!(!hub.emitted().is_empty(), "the circle's warning went out as event 98");
    }
}
