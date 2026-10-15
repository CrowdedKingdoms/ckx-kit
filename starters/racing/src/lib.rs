//! Speed and a contested ball, for two node types from one crate. Positions come from the
//! players' clients (`pos { x, z }`, a few times a second), and the movement warden
//! (`ckx_sdk::kit::play::movement`) keeps what the rules allow: the hub times and referees the
//! positions it kept, not the ones a client claimed. Positions are the hub's own units (blocks,
//! say), from the course's or the pitch's origin.
//!
//! - **`race`**, keyed by its race: a course of gates passed in order (`ckx_sdk::kit::play::timing`),
//!   a countdown, laps and splits, places in finishing order, the rest given `finish_grace_ms`
//!   after the winner, and a ghost of the winner's run. With a `board`, the finishers' times go to
//!   that `leaderboard` (configure it with `order: "low"`; the manifest lets `race` call
//!   `leaderboard`). Endpoints: `join`, `leave` (before the start), `start` (the first to join,
//!   or a developer), `pos { x, z }` (answers the position kept), `ghost` (the winner's track),
//!   `state` (anyone). Topic `race`: `snapshot` messages as racers move on, `event` messages
//!   (`type`: `go`, `timing` with its `stage`, `violation`, `finished`); timing is also realtime
//!   event 97 with an `anchor`.
//! - **`ball`**, keyed by its match: two teams, a ball the hub moves (passes, shots, friction,
//!   the side walls), possession (claim a loose ball near it, or take it from an opponent whose
//!   protection is over), a goal in each end wall, first to `score_to_win` or the better score
//!   when the time is up. Endpoints: `join { team }`, `start`, `pos { x, z }`, `claim`,
//!   `pass { to }`, `shoot { x, z }`, `state`. Topic `ball`: `snapshot` messages five times a
//!   second while the ball moves, `event` messages (`type`: `claimed`, `passed`, `shot`, `goal`,
//!   `finished`). With an `anchor`, players near the pitch also see the ball as a server-driven
//!   actor (`kind: "ball"`) and its possession changes as realtime event 100.
//!
//! A type runs as the one its seed names (`{"mode": "ball", ...}`), or as the one its name is.
//! Both speak the kit protocol (`ckx_sdk::kit`): `snapshot { topic }` answers each topic;
//! changes may carry an `action_id`; refusals carry `kit:<code>:` codes.

use ckx_sdk::kit::play::combat::Cooldowns;
use ckx_sdk::kit::play::movement::{Rules, Warden};
use ckx_sdk::kit::play::timing::{Course, Gate, GhostTrack, RunTimer, TimingEvent};
use ckx_sdk::kit::presence::CHUNK_BLOCKS;
use ckx_sdk::kit::wire::{self, ActorState};
use ckx_sdk::kit::{self, Publisher, Receipts, codes};
use ckx_sdk::prelude::*;
use serde_json::{Value, json};

pub const RACE_TOPIC: &str = "race";
pub const BALL_TOPIC: &str = "ball";
pub const TICK_MS: u64 = 100;
const PUBLISH_EVERY: u64 = 2;
const EVENT_DISTANCE: u8 = 4;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Race,
    Ball,
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
        "race" => Ok(Mode::Race),
        "ball" => Ok(Mode::Ball),
        other => Err(Error::new(format!(
            "the racing crate runs a `race` or a `ball`; give `{other}` a seed with its mode: {{\"mode\": \"ball\"}}"
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

#[derive(Deserialize)]
struct Pos {
    x: f64,
    z: f64,
}

// ---- race ----

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct RaceConfig {
    /// The course's name in timing events.
    pub course_id: String,
    pub course: Course,
    pub countdown_ms: u64,
    pub racers_max: usize,
    pub rules: Rules,
    /// How long the rest have once the winner is in.
    pub finish_grace_ms: u64,
    pub board: Option<String>,
    /// The chunk the course's origin is at, for realtime events.
    pub anchor: Option<(i64, i64, i64)>,
}

impl Default for RaceConfig {
    fn default() -> Self {
        let gate = |x: f32, z: f32| Gate { x, z, radius: 3.0 };
        Self {
            course_id: "loop".into(),
            course: Course { gates: vec![gate(0.0, 0.0), gate(30.0, 0.0), gate(15.0, 25.0)], laps: 2 },
            countdown_ms: 3_000,
            racers_max: 8,
            rules: Rules { max_speed: 12.0, teleport: 20.0, bounds: None, grace: 1.25 },
            finish_grace_ms: 30_000,
            board: None,
            anchor: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RacePhase {
    #[default]
    Lobby,
    Countdown,
    Racing,
    Finished,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Racer {
    pub player: u64,
    pub timer: RunTimer,
    pub warden: Warden,
    pub ghost: GhostTrack,
    pub place: Option<u32>,
    pub total_ms: Option<u64>,
    pub did_not_finish: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RacerView {
    pub player: u64,
    pub x: f64,
    pub z: f64,
    pub lap: u32,
    pub next_gate: usize,
    pub best_lap_ms: Option<u64>,
    pub place: Option<u32>,
    pub total_ms: Option<u64>,
    pub did_not_finish: bool,
}

/// The race as everyone sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RaceView {
    pub phase: RacePhase,
    pub course_id: String,
    pub gates: Vec<Gate>,
    pub laps: u32,
    pub go_at: Option<u64>,
    pub closes_at: Option<u64>,
    pub racers: Vec<RacerView>,
}

/// The winner's run, to race against.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Ghost {
    pub player: u64,
    pub total_ms: u64,
    /// `(ms from the start, x, y, z, yaw)`.
    pub frames: Vec<(u32, f32, f32, f32, f32)>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Race {
    pub config: RaceConfig,
    pub phase: RacePhase,
    pub racers: Vec<Racer>,
    pub go_at: Option<u64>,
    pub closes_at: Option<u64>,
    pub ghost: Option<Ghost>,
    #[serde(skip)]
    pub events: Vec<Value>,
    #[serde(skip)]
    pub moved: bool,
}

impl Race {
    pub fn view(&self) -> RaceView {
        RaceView {
            phase: self.phase,
            course_id: self.config.course_id.clone(),
            gates: self.config.course.gates.clone(),
            laps: self.config.course.laps,
            go_at: self.go_at,
            closes_at: self.closes_at,
            racers: self
                .racers
                .iter()
                .map(|r| {
                    let (x, z) = r.warden.position().unwrap_or((0.0, 0.0));
                    RacerView {
                        player: r.player,
                        x,
                        z,
                        lap: r.timer.lap,
                        next_gate: r.timer.next_gate,
                        best_lap_ms: (r.timer.best_lap_ms > 0).then_some(r.timer.best_lap_ms),
                        place: r.place,
                        total_ms: r.total_ms,
                        did_not_finish: r.did_not_finish,
                    }
                })
                .collect(),
        }
    }

    fn start_line(&self) -> (f64, f64) {
        self.config.course.gates.first().map_or((0.0, 0.0), |g| (f64::from(g.x), f64::from(g.z)))
    }

    pub fn join(&mut self, player: u64, now: u64) -> Result<()> {
        if self.phase != RacePhase::Lobby {
            return Err(kit::error(codes::STARTED, "the race is under way"));
        }
        if self.racers.iter().any(|r| r.player == player) {
            return Ok(());
        }
        if self.racers.len() >= self.config.racers_max {
            return Err(kit::error(codes::FULL, "the grid is full"));
        }
        let at = self.start_line();
        self.racers.push(Racer {
            player,
            timer: RunTimer::default(),
            warden: Warden::at(at, now),
            ghost: GhostTrack::default(),
            place: None,
            total_ms: None,
            did_not_finish: false,
        });
        self.moved = true;
        Ok(())
    }

    pub fn leave(&mut self, player: u64) -> Result<()> {
        if self.phase != RacePhase::Lobby {
            return Err(kit::error(codes::STARTED, "the race is under way"));
        }
        self.racers.retain(|r| r.player != player);
        self.moved = true;
        Ok(())
    }

    pub fn start(&mut self, now: u64) -> Result<()> {
        if self.phase != RacePhase::Lobby {
            return Err(kit::error(codes::STARTED, "the race is under way"));
        }
        if self.racers.is_empty() || self.config.course.gates.is_empty() {
            return Err(kit::error(codes::NOT_READY, "a race needs racers and a course"));
        }
        self.phase = RacePhase::Countdown;
        self.go_at = Some(now + self.config.countdown_ms);
        self.moved = true;
        Ok(())
    }

    /// The start: every racer on the line starts the clock now.
    pub fn go(&mut self, now: u64) {
        if self.phase != RacePhase::Countdown {
            return;
        }
        self.phase = RacePhase::Racing;
        self.events.push(with_type("go", json!({ "at": now })));
        for i in 0..self.racers.len() {
            if let Some(at) = self.racers[i].warden.position() {
                self.feed(i, at, now);
            }
        }
        self.moved = true;
    }

    /// A reported position. Returns the position kept.
    pub fn moved(&mut self, player: u64, to: (f64, f64), now: u64) -> Result<(f64, f64)> {
        let i = self.racers.iter().position(|r| r.player == player).ok_or_else(|| kit::error(codes::NOT_MEMBER, "join the race first"))?;
        let verdict = self.racers[i].warden.check(&self.config.rules, to, now);
        if let Some(v) = &verdict.violation {
            self.events.push(with_type("violation", json!({ "player": player, "what": v.kind.as_str(), "detail": v.detail })));
        }
        if self.phase == RacePhase::Racing {
            self.feed(i, verdict.at, now);
        }
        self.moved = true;
        Ok(verdict.at)
    }

    fn feed(&mut self, i: usize, at: (f64, f64), now: u64) {
        let r = &mut self.racers[i];
        if r.place.is_some() || r.did_not_finish {
            return;
        }
        if r.timer.started_at_ms > 0 {
            r.ghost.record(now, at.0 as f32, 0.0, at.1 as f32, 0.0);
        }
        let events = r.timer.feed(&self.config.course, (at.0 as f32, at.1 as f32), now);
        let player = r.player;
        for e in events {
            let (kind, checkpoint, lap, ms) = match e {
                TimingEvent::Started { .. } => {
                    let r = &mut self.racers[i];
                    r.ghost = GhostTrack::default();
                    r.ghost.record(now, at.0 as f32, 0.0, at.1 as f32, 0.0);
                    ("started", 0, 1, 0)
                }
                TimingEvent::Checkpoint { gate, split_ms } => ("checkpoint", gate as u32, self.racers[i].timer.lap, split_ms),
                TimingEvent::LapCompleted { lap, lap_ms, .. } => ("lap", 0, lap, lap_ms),
                TimingEvent::Finished { total_ms, .. } => {
                    self.finish(i, total_ms, now);
                    ("finished", 0, self.config.course.laps, total_ms)
                }
            };
            self.events.push(with_type(
                "timing",
                json!({ "stage": kind, "player": player, "checkpoint": checkpoint, "lap": lap, "elapsed_ms": ms }),
            ));
        }
    }

    fn finish(&mut self, i: usize, total_ms: u64, now: u64) {
        let place = self.racers.iter().filter(|r| r.place.is_some()).count() as u32 + 1;
        let r = &mut self.racers[i];
        r.place = Some(place);
        r.total_ms = Some(total_ms);
        if place == 1 {
            self.ghost = Some(Ghost { player: r.player, total_ms, frames: r.ghost.frames.clone() });
            self.closes_at = Some(now + self.config.finish_grace_ms);
        }
        if self.racers.iter().all(|r| r.place.is_some()) {
            self.close();
        }
    }

    /// Ends the race: whoever has not finished did not.
    pub fn close(&mut self) {
        if self.phase == RacePhase::Finished {
            return;
        }
        self.phase = RacePhase::Finished;
        for r in self.racers.iter_mut().filter(|r| r.place.is_none()) {
            r.did_not_finish = true;
        }
        let results: Vec<Value> =
            self.racers.iter().map(|r| json!({ "player": r.player, "place": r.place, "total_ms": r.total_ms })).collect();
        self.events.push(with_type("finished", json!({ "results": results })));
        self.moved = true;
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct RaceHub {
    #[serde(flatten)]
    pub race: Race,
    #[serde(default)]
    pub kit: Publisher,
    #[serde(default)]
    pub receipts: Receipts,
}

impl RaceHub {
    fn act(&mut self, ctx: &Ctx, call: &Call<'_>, now: u64) -> Result<Vec<u8>> {
        let out = match call.method {
            "join" => {
                self.race.join(call.player()?, now)?;
                encode(&true)
            }
            "leave" => {
                self.race.leave(call.player()?)?;
                encode(&true)
            }
            "start" => {
                let allowed = match call.caller {
                    Caller::Developer(_) => true,
                    Caller::Player(p) => self.race.racers.first().map(|r| r.player) == Some(p),
                    _ => false,
                };
                if !allowed {
                    return Err(kit::error(codes::FORBIDDEN, "the first to join, or a developer, starts it"));
                }
                self.race.start(now)?;
                ctx.timer_after("go", self.race.config.countdown_ms.max(10))?;
                encode(&true)
            }
            "pos" => {
                let p: Pos = kit::args(call)?;
                let kept = self.race.moved(call.player()?, (p.x, p.z), now)?;
                encode(&kept)
            }
            other => return Err(Error::unknown_method(other)),
        };
        self.after(ctx)?;
        out
    }

    fn after(&mut self, ctx: &Ctx) -> Result<()> {
        if let Some(close) = self.race.closes_at
            && self.race.phase == RacePhase::Racing
            && !ctx.timers().iter().any(|(n, _)| n == "close")
        {
            ctx.timer_after("close", close.saturating_sub(ctx.now_ms()).max(10))?;
        }
        let finished = self.race.events.iter().any(|e| e["type"] == "finished");
        for e in std::mem::take(&mut self.race.events) {
            self.kit.event(ctx, RACE_TOPIC, &e)?;
            self.realtime(ctx, &e);
        }
        if finished {
            ctx.cancel_timer("close");
            self.post(ctx);
            ctx.persist_now();
        }
        if std::mem::take(&mut self.race.moved) {
            self.kit.snapshot(ctx, RACE_TOPIC, &self.race.view())?;
        }
        Ok(())
    }

    /// The finishers' times, to the board.
    fn post(&self, ctx: &Ctx) {
        let Some(board) = &self.race.config.board else { return };
        for r in &self.race.racers {
            if let Some(ms) = r.total_ms {
                let submit = json!({ "player": r.player, "score": ms, "action_id": format!("{}:{}", ctx.key, r.player) });
                let sent =
                    encode(&submit).map_err(|Error(e)| e).and_then(|b| ctx.send("leaderboard", board, "submit", &b).map_err(|e| e.message));
                if let Err(why) = sent {
                    ctx.log(Level::Warn, &format!("a time did not reach board {board}: {why}"));
                }
            }
        }
    }

    fn realtime(&self, ctx: &Ctx, e: &Value) {
        let (Some((x, y, z)), Some("timing")) = (self.race.config.anchor, e["type"].as_str()) else { return };
        let body = wire::RaceTiming {
            kind: e["stage"].as_str().unwrap_or_default().into(),
            course_id: self.race.config.course_id.clone(),
            user_id: e["player"].as_u64().unwrap_or(0).to_string(),
            checkpoint: e["checkpoint"].as_u64().unwrap_or(0) as u32,
            lap: e["lap"].as_u64().unwrap_or(0) as u32,
            elapsed_ms: e["elapsed_ms"].as_u64().unwrap_or(0),
        };
        let actor = actor_id(&format!("race:{}", ctx.key));
        if let Err(Error(why)) = wire::emit_event(ctx, ChunkPos::new(x, y, z), EVENT_DISTANCE, &actor, wire::RACE_TIMING, &body) {
            ctx.log(Level::Warn, &format!("a timing event did not go out: {why}"));
        }
    }
}

// ---- ball ----

pub const TEAMS: [&str; 2] = ["red", "blue"];

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct BallConfig {
    pub width: f64,
    pub depth: f64,
    /// The goal's width in each end wall, centred.
    pub goal_width: f64,
    pub team_max: usize,
    pub claim_radius: f64,
    /// How long a new holder cannot be robbed.
    pub protect_ms: u64,
    pub pass_speed: f64,
    pub shoot_speed: f64,
    /// The share of its speed a loose ball loses each second.
    pub friction: f64,
    pub score_to_win: u32,
    pub duration_ms: u64,
    pub max_speed: f64,
    pub teleport: f64,
    /// The chunk the pitch's origin is at: the ball is shown there, in blocks.
    pub anchor: Option<(i64, i64, i64)>,
}

impl Default for BallConfig {
    fn default() -> Self {
        Self {
            width: 60.0,
            depth: 40.0,
            goal_width: 12.0,
            team_max: 5,
            claim_radius: 1.5,
            protect_ms: 1_500,
            pass_speed: 18.0,
            shoot_speed: 28.0,
            friction: 0.8,
            score_to_win: 3,
            duration_ms: 5 * 60_000,
            max_speed: 9.0,
            teleport: 15.0,
            anchor: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchPhase {
    #[default]
    Lobby,
    Playing,
    Finished,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Footballer {
    pub player: u64,
    pub team: usize,
    pub warden: Warden,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct BallState {
    pub x: f64,
    pub z: f64,
    pub vx: f64,
    pub vz: f64,
    pub holder: Option<u64>,
    pub held_since: u64,
    pub last_touch: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlayerView {
    pub player: u64,
    pub team: String,
    pub x: f64,
    pub z: f64,
}

/// The match as everyone sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BallView {
    pub phase: MatchPhase,
    pub width: f64,
    pub depth: f64,
    pub goal_width: f64,
    pub ball: BallState,
    pub players: Vec<PlayerView>,
    /// Red's, then blue's.
    pub score: [u32; 2],
    pub ends_at: Option<u64>,
    pub winner: Option<String>,
}

#[derive(Deserialize)]
struct JoinTeam {
    team: String,
}

#[derive(Deserialize)]
struct Pass {
    to: u64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Match {
    pub config: BallConfig,
    pub phase: MatchPhase,
    pub players: Vec<Footballer>,
    pub ball: BallState,
    pub score: [u32; 2],
    pub ends_at: Option<u64>,
    pub winner: Option<usize>,
    pub cooldowns: Cooldowns,
    pub ticks: u64,
    #[serde(skip)]
    pub events: Vec<Value>,
    #[serde(skip)]
    pub moved: bool,
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

impl Match {
    pub fn view(&self) -> BallView {
        BallView {
            phase: self.phase,
            width: self.config.width,
            depth: self.config.depth,
            goal_width: self.config.goal_width,
            ball: self.ball.clone(),
            players: self
                .players
                .iter()
                .map(|p| {
                    let (x, z) = p.warden.position().unwrap_or((0.0, 0.0));
                    PlayerView { player: p.player, team: TEAMS[p.team].into(), x, z }
                })
                .collect(),
            score: self.score,
            ends_at: self.ends_at,
            winner: self.winner.map(|t| TEAMS[t].to_string()),
        }
    }

    fn rules(&self) -> Rules {
        Rules {
            max_speed: self.config.max_speed,
            teleport: self.config.teleport,
            bounds: Some((0.0, 0.0, self.config.width, self.config.depth)),
            grace: 1.25,
        }
    }

    fn at(&self, player: u64) -> Result<(usize, (f64, f64))> {
        let i =
            self.players.iter().position(|p| p.player == player).ok_or_else(|| kit::error(codes::NOT_MEMBER, "join the match first"))?;
        Ok((i, self.players[i].warden.position().unwrap_or((0.0, 0.0))))
    }

    fn playing(&self) -> Result<()> {
        match self.phase {
            MatchPhase::Playing => Ok(()),
            MatchPhase::Lobby => Err(kit::error(codes::NOT_READY, "the match has not started")),
            MatchPhase::Finished => Err(kit::error(codes::FINISHED, "the match is over")),
        }
    }

    fn kickoff(&mut self) {
        self.ball = BallState { x: self.config.width / 2.0, z: self.config.depth / 2.0, ..BallState::default() };
        self.moved = true;
    }

    pub fn join(&mut self, player: u64, team: &str, now: u64) -> Result<()> {
        if self.phase != MatchPhase::Lobby {
            return Err(kit::error(codes::STARTED, "the match is under way"));
        }
        let t = TEAMS.iter().position(|x| *x == team).ok_or_else(|| kit::error(codes::BAD_REQUEST, "a team is red or blue"))?;
        if self.players.iter().filter(|p| p.team == t && p.player != player).count() >= self.config.team_max {
            return Err(kit::error(codes::FULL, format!("{team} is full")));
        }
        let x = if t == 0 { self.config.width * 0.25 } else { self.config.width * 0.75 };
        let at = (x, self.config.depth / 2.0);
        match self.players.iter_mut().find(|p| p.player == player) {
            Some(p) => p.team = t,
            None => self.players.push(Footballer { player, team: t, warden: Warden::at(at, now) }),
        }
        self.moved = true;
        Ok(())
    }

    pub fn start(&mut self, now: u64) -> Result<()> {
        if self.phase == MatchPhase::Playing {
            return Err(kit::error(codes::STARTED, "the match is under way"));
        }
        if !(0..2).all(|t| self.players.iter().any(|p| p.team == t)) {
            return Err(kit::error(codes::NOT_READY, "each team needs a player"));
        }
        self.phase = MatchPhase::Playing;
        self.score = [0, 0];
        self.winner = None;
        self.ends_at = Some(now + self.config.duration_ms);
        self.kickoff();
        Ok(())
    }

    pub fn moved(&mut self, player: u64, to: (f64, f64), now: u64) -> Result<(f64, f64)> {
        let (i, _) = self.at(player)?;
        let rules = self.rules();
        let verdict = self.players[i].warden.check(&rules, to, now);
        if let Some(v) = &verdict.violation {
            self.events.push(with_type("violation", json!({ "player": player, "what": v.kind.as_str(), "detail": v.detail })));
        }
        if self.ball.holder == Some(player) {
            (self.ball.x, self.ball.z) = verdict.at;
        }
        self.moved = true;
        Ok(verdict.at)
    }

    pub fn claim(&mut self, player: u64, now: u64) -> Result<()> {
        self.playing()?;
        let (i, at) = self.at(player)?;
        if dist(at, (self.ball.x, self.ball.z)) > self.config.claim_radius {
            return Err(kit::error(codes::OUT_OF_REACH, "the ball is too far"));
        }
        if !self.cooldowns.take(&player.to_string(), "claim", 500, now) {
            return Err(kit::error(codes::COOLDOWN, "claiming too fast"));
        }
        let from = self.ball.holder;
        if let Some(h) = from {
            if h == player {
                return Ok(());
            }
            let holder_team = self.players.iter().find(|p| p.player == h).map(|p| p.team);
            if holder_team == Some(self.players[i].team) {
                return Err(kit::error(codes::CONFLICT, "a teammate has it"));
            }
            if now.saturating_sub(self.ball.held_since) < self.config.protect_ms {
                return Err(kit::error(codes::COOLDOWN, "they only just got it"));
            }
        }
        self.ball = BallState { x: at.0, z: at.1, holder: Some(player), held_since: now, last_touch: Some(player), ..BallState::default() };
        self.events.push(with_type("claimed", json!({ "player": player, "team": TEAMS[self.players[i].team], "from": from })));
        self.moved = true;
        Ok(())
    }

    fn release(&mut self, player: u64, toward: (f64, f64), speed: f64) -> Result<()> {
        self.playing()?;
        if self.ball.holder != Some(player) {
            return Err(kit::error(codes::CONFLICT, "you do not have the ball"));
        }
        let (dx, dz) = (toward.0 - self.ball.x, toward.1 - self.ball.z);
        let d = (dx * dx + dz * dz).sqrt();
        let (ux, uz) = if d > 1e-6 { (dx / d, dz / d) } else { (1.0, 0.0) };
        self.ball.vx = ux * speed;
        self.ball.vz = uz * speed;
        self.ball.holder = None;
        self.ball.last_touch = Some(player);
        self.moved = true;
        Ok(())
    }

    pub fn pass(&mut self, player: u64, to: u64) -> Result<()> {
        let (i, _) = self.at(player)?;
        let (j, target) = self.at(to)?;
        if i == j || self.players[i].team != self.players[j].team {
            return Err(kit::error(codes::BAD_REQUEST, "pass to a teammate"));
        }
        self.release(player, target, self.config.pass_speed)?;
        self.events.push(with_type("passed", json!({ "player": player, "to": to, "team": TEAMS[self.players[i].team] })));
        Ok(())
    }

    pub fn shoot(&mut self, player: u64, aim: (f64, f64)) -> Result<()> {
        let (i, _) = self.at(player)?;
        self.release(player, aim, self.config.shoot_speed)?;
        self.events.push(with_type("shot", json!({ "player": player, "team": TEAMS[self.players[i].team] })));
        Ok(())
    }

    /// One tick: the ball moves, the walls turn it, a goal counts, the clock runs. Returns
    /// whether the match ran.
    pub fn tick(&mut self, now: u64) -> bool {
        if self.phase != MatchPhase::Playing {
            return false;
        }
        self.ticks += 1;
        let dt = TICK_MS as f64 / 1000.0;
        let (w, d, half) = (self.config.width, self.config.depth, self.config.goal_width / 2.0);
        if self.ball.holder.is_none() && (self.ball.vx != 0.0 || self.ball.vz != 0.0) {
            let b = &mut self.ball;
            b.x += b.vx * dt;
            b.z += b.vz * dt;
            let keep = (1.0 - self.config.friction * dt).max(0.0);
            b.vx *= keep;
            b.vz *= keep;
            if b.vx.hypot(b.vz) < 0.2 {
                (b.vx, b.vz) = (0.0, 0.0);
            }
            if b.z < 0.0 || b.z > d {
                b.z = b.z.clamp(0.0, d);
                b.vz = -b.vz;
            }
            let in_mouth = (b.z - d / 2.0).abs() <= half;
            let scored = match (b.x < 0.0, b.x > w) {
                (true, _) if in_mouth => Some(1),
                (_, true) if in_mouth => Some(0),
                (true, _) | (_, true) => {
                    b.x = b.x.clamp(0.0, w);
                    b.vx = -b.vx;
                    None
                }
                _ => None,
            };
            self.moved = true;
            if let Some(team) = scored {
                self.score[team] += 1;
                let scorer = self.ball.last_touch;
                self.events.push(with_type("goal", json!({ "team": TEAMS[team], "player": scorer, "score": self.score })));
                self.kickoff();
            }
        }
        let decided = self.score.iter().position(|s| *s >= self.config.score_to_win);
        if decided.is_some() || self.ends_at.is_some_and(|t| now >= t) {
            self.phase = MatchPhase::Finished;
            self.winner = decided.or(match self.score[0].cmp(&self.score[1]) {
                std::cmp::Ordering::Greater => Some(0),
                std::cmp::Ordering::Less => Some(1),
                std::cmp::Ordering::Equal => None,
            });
            self.events.push(with_type("finished", json!({ "winner": self.winner.map(|t| TEAMS[t]), "score": self.score })));
            self.moved = true;
        }
        true
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct BallHub {
    #[serde(flatten)]
    pub game: Match,
    #[serde(default)]
    pub kit: Publisher,
    #[serde(default)]
    pub receipts: Receipts,
}

impl BallHub {
    fn act(&mut self, ctx: &Ctx, call: &Call<'_>, now: u64) -> Result<Vec<u8>> {
        let g = &mut self.game;
        let out = match call.method {
            "join" => {
                let j: JoinTeam = kit::args(call)?;
                g.join(call.player()?, &j.team, now)?;
                encode(&true)
            }
            "start" => {
                let allowed = match call.caller {
                    Caller::Developer(_) => true,
                    Caller::Player(p) => g.players.first().map(|f| f.player) == Some(p),
                    _ => false,
                };
                if !allowed {
                    return Err(kit::error(codes::FORBIDDEN, "the first to join, or a developer, starts it"));
                }
                g.start(now)?;
                ctx.timer_every("tick", TICK_MS)?;
                encode(&true)
            }
            "pos" => {
                let p: Pos = kit::args(call)?;
                let kept = g.moved(call.player()?, (p.x, p.z), now)?;
                self.flush(ctx, false)?;
                return encode(&kept);
            }
            "claim" => {
                g.claim(call.player()?, now)?;
                encode(&true)
            }
            "pass" => {
                let p: Pass = kit::args(call)?;
                g.pass(call.player()?, p.to)?;
                encode(&true)
            }
            "shoot" => {
                let p: Pos = kit::args(call)?;
                g.shoot(call.player()?, (p.x, p.z))?;
                encode(&true)
            }
            other => return Err(Error::unknown_method(other)),
        };
        self.flush(ctx, true)?;
        out
    }

    fn flush(&mut self, ctx: &Ctx, snapshot: bool) -> Result<()> {
        for e in std::mem::take(&mut self.game.events) {
            self.kit.event(ctx, BALL_TOPIC, &e)?;
            self.possession(ctx, &e);
        }
        if snapshot && std::mem::take(&mut self.game.moved) {
            self.kit.snapshot(ctx, BALL_TOPIC, &self.game.view())?;
        }
        Ok(())
    }

    fn anchor(&self) -> Option<(f64, f64, f64)> {
        self.game.config.anchor.map(|(x, y, z)| (x as f64, y as f64, z as f64))
    }

    fn possession(&self, ctx: &Ctx, e: &Value) {
        let Some((x, y, z)) = self.anchor() else { return };
        let kind = match e["type"].as_str() {
            Some("claimed") => "claimed",
            Some("passed") => "passed",
            Some("shot") => "loose",
            Some("goal") => "goal",
            _ => return,
        };
        let body = wire::PossessionChange {
            kind: kind.into(),
            holder_id: if kind == "claimed" { e["player"].as_u64().map(|p| p.to_string()) } else { None },
            team: e["team"].as_str().map(String::from),
        };
        let at = ChunkPos::new(x as i64, y as i64, z as i64);
        if let Err(Error(why)) = wire::emit_event(ctx, at, EVENT_DISTANCE, &actor_id(&format!("ball:{}", ctx.key)), wire::POSSESSION, &body)
        {
            ctx.log(Level::Warn, &format!("a possession event did not go out: {why}"));
        }
    }

    /// The ball as a server-driven actor near the pitch.
    fn show_ball(&self, ctx: &Ctx) {
        let Some((x, y, z)) = self.anchor() else { return };
        let b = &self.game.ball;
        let mut state = ActorState::new("ball", 1, x + b.x / CHUNK_BLOCKS, z + b.z / CHUNK_BLOCKS);
        state.y = y + 0.1;
        state.owner = b.holder;
        if let Err(Error(why)) = wire::emit_actor_state(ctx, &actor_id(&format!("ball:{}", ctx.key)), EVENT_DISTANCE, &state) {
            ctx.log(Level::Warn, &format!("the ball was not shown: {why}"));
        }
    }
}

// ---- the node ----

/// The crate's hub: one of the two types.
#[derive(Serialize, Deserialize, Debug)]
pub enum RacingNode {
    Race(RaceHub),
    Ball(BallHub),
}

impl Hub for RacingNode {
    fn spawn(ctx: &Ctx, seed: &[u8]) -> Result<Self> {
        Ok(match mode_of(&ctx.node_type, seed)? {
            Mode::Race => Self::Race(RaceHub { race: Race { config: config(seed)?, ..Race::default() }, ..RaceHub::default() }),
            Mode::Ball => {
                let mut game = Match { config: config(seed)?, ..Match::default() };
                game.kickoff();
                Self::Ball(BallHub { game, ..BallHub::default() })
            }
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
            Self::Race(h) => match call.method {
                "state" => encode(&h.race.view()),
                "ghost" => encode(&h.race.ghost),
                kit::SNAPSHOT_METHOD => h.kit.snapshot_reply(ctx, RACE_TOPIC, &h.race.view()),
                _ => Receipts::run(h, |h| &mut h.receipts, &call, now, |h| h.act(ctx, &call, now)),
            },
            Self::Ball(h) => match call.method {
                "state" => encode(&h.game.view()),
                kit::SNAPSHOT_METHOD => h.kit.snapshot_reply(ctx, BALL_TOPIC, &h.game.view()),
                _ => Receipts::run(h, |h| &mut h.receipts, &call, now, |h| h.act(ctx, &call, now)),
            },
        }
    }

    fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
        let now = ctx.now_ms();
        match (self, timer) {
            (Self::Race(h), "go") => {
                h.race.go(now);
                h.after(ctx)
            }
            (Self::Race(h), "close") => {
                h.race.close();
                h.after(ctx)
            }
            (Self::Ball(h), "tick") => {
                if !h.game.tick(now) {
                    ctx.cancel_timer("tick");
                    return Ok(());
                }
                let ended = h.game.phase != MatchPhase::Playing;
                if ended {
                    ctx.cancel_timer("tick");
                    ctx.persist_now();
                }
                h.show_ball(ctx);
                let due = ended || !h.game.events.is_empty() || h.game.ticks.is_multiple_of(PUBLISH_EVERY);
                h.flush(ctx, due)
            }
            _ => Ok(()),
        }
    }
}

ckx_sdk::export_hub!(RacingNode);

#[cfg(test)]
mod tests {
    use super::*;
    use ckx_sdk::kit::error_code;
    use ckx_sdk::testing::TestHub;

    fn code(e: &CallError) -> Option<&str> {
        error_code(&e.message).map(|c| c.0)
    }

    #[test]
    fn a_type_runs_as_its_seed_says_or_as_its_name() {
        assert_eq!(mode_of("race", b"").unwrap(), Mode::Race);
        assert_eq!(mode_of("ball", b"").unwrap(), Mode::Ball);
        assert_eq!(mode_of("kart", br#"{"mode":"race","countdown_ms":0}"#).unwrap(), Mode::Race);
        assert!(mode_of("kart", b"").is_err());
    }

    /// Drives `player` around the default loop in steps the warden allows.
    fn lap(hub: &mut TestHub<RacingNode>, player: u64, path: &[(f64, f64)]) {
        for &(x, z) in path {
            hub.advance(1_000).unwrap();
            let _: (f64, f64) = hub.call_typed(Caller::Player(player), "pos", &json!({ "x": x, "z": z })).unwrap();
        }
    }

    const LOOP: [(f64, f64); 9] =
        [(10.0, 0.0), (20.0, 0.0), (30.0, 0.0), (25.0, 8.0), (20.0, 16.0), (15.0, 25.0), (10.0, 16.0), (5.0, 8.0), (0.0, 0.0)];

    #[test]
    fn a_race_counts_down_times_gates_in_order_places_finishers_and_keeps_the_winners_ghost() {
        let mut hub = TestHub::<RacingNode>::spawn_with("race", "r1", br#"{"board":"fastest","finish_grace_ms":5000}"#).unwrap();
        let _: bool = hub.call_typed(Caller::Player(1), "join", &()).unwrap();
        let _: bool = hub.call_typed(Caller::Player(2), "join", &()).unwrap();
        let e = hub.call(Caller::Player(2), "start", &encode(&()).unwrap()).unwrap_err();
        assert_eq!(code(&e), Some(codes::FORBIDDEN));
        let _: bool = hub.call_typed(Caller::Player(1), "start", &()).unwrap();
        let e = hub.call(Caller::Player(3), "join", &encode(&()).unwrap()).unwrap_err();
        assert_eq!(code(&e), Some(codes::STARTED));
        hub.advance(3_000).unwrap();
        let view: RaceView = hub.call_typed(Caller::Player(1), "state", &()).unwrap();
        assert_eq!(view.phase, RacePhase::Racing);
        lap(&mut hub, 1, &LOOP);
        lap(&mut hub, 1, &LOOP);
        let view: RaceView = hub.call_typed(Caller::Player(1), "state", &()).unwrap();
        assert_eq!(view.racers[0].place, Some(1), "{view:?}");
        assert_eq!(view.racers[0].total_ms, Some(18_000));
        let ghost: Option<Ghost> = hub.call_typed(Caller::Player(2), "ghost", &()).unwrap();
        let ghost = ghost.expect("the winner's ghost");
        assert_eq!((ghost.player, ghost.frames.len()), (1, 19));
        // The other racer is given the grace, then did not finish.
        hub.advance(6_000).unwrap();
        let view: RaceView = hub.call_typed(Caller::Player(1), "state", &()).unwrap();
        assert_eq!(view.phase, RacePhase::Finished);
        assert!(view.racers[1].did_not_finish);
        let sent = hub.sent();
        assert_eq!(sent.len(), 1, "only a finisher's time goes to the board");
        assert_eq!((sent[0].node_type.as_str(), sent[0].key.as_str()), ("leaderboard", "fastest"));
    }

    #[test]
    fn the_warden_keeps_a_racer_from_cutting_the_course() {
        let mut race = Race::default();
        race.join(1, 0).unwrap();
        race.start(0).unwrap();
        race.go(3_000);
        let kept = race.moved(1, (30.0, 0.0), 3_100).unwrap();
        assert_eq!(kept, (0.0, 0.0), "a jump across the course is refused");
        assert_eq!(race.racers[0].timer.next_gate, 1, "and passes no gate");
        assert!(race.events.iter().any(|e| e["type"] == "violation"));
    }

    fn pitch() -> Match {
        let mut m = Match::default();
        m.join(1, "red", 0).unwrap();
        m.join(2, "red", 0).unwrap();
        m.join(3, "blue", 0).unwrap();
        m.start(0).unwrap();
        m
    }

    #[test]
    fn possession_is_claimed_near_the_ball_protected_for_a_moment_and_passed_to_a_teammate() {
        let mut m = pitch();
        let e = m.claim(1, 100).unwrap_err();
        assert_eq!(error_code(&e.0).map(|c| c.0), Some(codes::OUT_OF_REACH));
        m.players[0].warden.place((30.0, 20.0), 100);
        m.claim(1, 200).unwrap();
        assert_eq!(m.ball.holder, Some(1));
        m.players[2].warden.place((30.5, 20.0), 300);
        let e = m.claim(3, 400).unwrap_err();
        assert_eq!(error_code(&e.0).map(|c| c.0), Some(codes::COOLDOWN), "protected");
        m.claim(3, 2_000).unwrap();
        assert_eq!(m.ball.holder, Some(3), "taken after the protection");
        let e = m.pass(3, 1).unwrap_err();
        assert_eq!(error_code(&e.0).map(|c| c.0), Some(codes::BAD_REQUEST), "not to an opponent");
        m.players[2].warden.place((30.5, 20.0), 2_000);
        m.claim(1, 4_000).unwrap();
        m.pass(1, 2).unwrap();
        assert!(m.ball.holder.is_none() && m.ball.vx < 0.0, "toward red's other player at (15, 20)");
    }

    #[test]
    fn a_shot_into_the_goal_scores_and_the_first_to_the_target_wins() {
        let mut m = pitch();
        m.config.score_to_win = 1;
        m.players[0].warden.place((30.0, 20.0), 0);
        m.claim(1, 100).unwrap();
        m.shoot(1, (60.0, 20.0)).unwrap();
        let mut now = 100;
        while m.phase == MatchPhase::Playing && now < 10_000 {
            now += TICK_MS;
            m.tick(now);
        }
        assert_eq!((m.score, m.winner), ([1, 0], Some(0)));
        assert!(m.events.iter().any(|e| e["type"] == "goal"));
    }

    #[test]
    fn a_wide_shot_comes_back_off_the_end_wall() {
        let mut m = pitch();
        m.players[0].warden.place((55.0, 2.0), 0);
        m.claim(1, 0).unwrap_err();
        m.ball = BallState { x: 55.0, z: 2.0, ..BallState::default() };
        m.claim(1, 600).unwrap();
        m.shoot(1, (70.0, 2.0)).unwrap();
        for i in 1..10 {
            m.tick(600 + i * TICK_MS);
        }
        assert_eq!(m.score, [0, 0]);
        assert!(m.ball.vx < 0.0, "it bounced: {:?}", m.ball);
    }

    #[test]
    fn an_anchored_pitch_shows_its_ball_and_sends_possession_changes() {
        let mut hub = TestHub::<RacingNode>::spawn_with("ball", "m1", br#"{"anchor":[10,0,10]}"#).unwrap();
        let _: bool = hub.call_typed(Caller::Player(1), "join", &json!({ "team": "red" })).unwrap();
        let _: bool = hub.call_typed(Caller::Player(2), "join", &json!({ "team": "blue" })).unwrap();
        let e = hub.call(Caller::Player(1), "join", &encode(&json!({ "team": "green" })).unwrap()).unwrap_err();
        assert_eq!(code(&e), Some(codes::BAD_REQUEST));
        let _: bool = hub.call_typed(Caller::Player(1), "start", &()).unwrap();
        let _: (f64, f64) = hub.call_typed(Caller::Player(1), "pos", &json!({ "x": 15.0, "z": 20.0 })).unwrap();
        hub.advance(2_000).unwrap();
        for x in [20.0, 25.0, 29.5] {
            let _: (f64, f64) = hub.call_typed(Caller::Player(1), "pos", &json!({ "x": x, "z": 20.0 })).unwrap();
            hub.advance(1_000).unwrap();
        }
        let _: bool = hub.call_typed(Caller::Player(1), "claim", &()).unwrap();
        let emitted = hub.emitted().len();
        assert!(emitted >= 20, "the ball is shown each tick: {emitted}");
        let view: BallView = hub.call_typed(Caller::Player(2), "state", &()).unwrap();
        assert_eq!(view.ball.holder, Some(1));
    }
}
