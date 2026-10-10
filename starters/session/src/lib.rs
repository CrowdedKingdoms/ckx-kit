//! One game session, keyed by its id, from lobby to result. Players join the lobby; the host
//! (the first to join, then the next connected player if the host goes) starts once everyone
//! is ready; turns go round with a time limit, and a player who runs out of time is skipped;
//! the hub decides each move's points and the winner. A matchmaker can reserve the session for
//! its players first. Replaces the `match-engine` template.
//!
//! Endpoints (players): `join`, `leave`, `ready { ready }`, `start` (host), `act { points }`
//! (the player whose turn it is), `state` (anyone). `configure { .. }` (the host in the lobby,
//! or a developer) and `reserve { players }` (another instance, or a developer). Every change
//! is pushed on the `session` topic.
//!
//! [`points_for`] is the game's rule; replace it with yours.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ckx_sdk::prelude::*;

const LOG_MOVES: usize = 20;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub min_players: usize,
    pub max_players: usize,
    pub turn_ms: u64,
    pub target_score: i64,
    pub max_rounds: u32,
    /// A lobby nobody joins closes after this long.
    pub lobby_idle_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self { min_players: 2, max_players: 8, turn_ms: 30_000, target_score: 10, max_rounds: 20, lobby_idle_ms: 600_000 }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Lobby,
    Playing,
    Finished,
}

/// A move as the client sends it. Replace with your game's action.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Action {
    pub points: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Move {
    pub player: u64,
    pub round: u32,
    /// None when the player ran out of time.
    pub points: Option<i64>,
}

/// The game's rule for what a move scores. The starter's: 0 to 3 points, as asked.
pub fn points_for(action: &Action) -> Result<i64> {
    if (0..=3).contains(&action.points) { Ok(action.points) } else { Err(Error::new("a move scores 0 to 3 points")) }
}

#[derive(Deserialize)]
struct Ready {
    ready: bool,
}

#[derive(Deserialize)]
struct Reserve {
    players: Vec<u64>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct GameSession {
    pub config: Config,
    pub phase: Phase,
    /// Only these players may join, when set (a matchmaker's reservation).
    pub reserved: Vec<u64>,
    pub players: Vec<u64>,
    pub ready: BTreeSet<u64>,
    pub connected: BTreeSet<u64>,
    pub host: Option<u64>,
    pub turn: usize,
    pub round: u32,
    pub scores: BTreeMap<u64, i64>,
    pub winner: Option<u64>,
    pub moves: VecDeque<Move>,
}

impl GameSession {
    fn now_turn(&self) -> Option<u64> {
        (self.phase == Phase::Playing).then(|| self.players.get(self.turn).copied()).flatten()
    }

    pub fn join(&mut self, player: u64) -> Result<()> {
        if self.players.contains(&player) {
            return Ok(());
        }
        if self.phase != Phase::Lobby {
            return Err(Error::new("the session has started"));
        }
        if !self.reserved.is_empty() && !self.reserved.contains(&player) {
            return Err(Error::new("this session is reserved for other players"));
        }
        if self.players.len() >= self.config.max_players {
            return Err(Error::new("the session is full"));
        }
        self.players.push(player);
        self.scores.insert(player, 0);
        self.host.get_or_insert(player);
        Ok(())
    }

    /// Returns whether the turn timer needs re-arming.
    pub fn leave(&mut self, player: u64) -> bool {
        let Some(i) = self.players.iter().position(|p| *p == player) else { return false };
        let was_turn = self.now_turn() == Some(player);
        self.players.remove(i);
        self.ready.remove(&player);
        if self.phase == Phase::Lobby {
            self.scores.remove(&player);
        }
        if self.host == Some(player) {
            self.host = self.players.iter().find(|p| self.connected.contains(p)).or(self.players.first()).copied();
        }
        if self.phase != Phase::Playing {
            return false;
        }
        if self.players.len() < self.config.min_players {
            self.finish();
            return false;
        }
        if i < self.turn {
            self.turn -= 1;
        }
        if self.turn >= self.players.len() {
            self.turn = 0;
            self.round += 1;
        }
        was_turn
    }

    pub fn start(&mut self, player: u64) -> Result<()> {
        if self.phase != Phase::Lobby {
            return Err(Error::new("the session has started"));
        }
        if self.host != Some(player) {
            return Err(Error::new("only the host starts the session"));
        }
        if self.players.len() < self.config.min_players {
            return Err(Error::new(format!("at least {} players are needed", self.config.min_players)));
        }
        if self.players.iter().any(|p| !self.ready.contains(p)) {
            return Err(Error::new("everyone must be ready"));
        }
        self.phase = Phase::Playing;
        self.turn = 0;
        self.round = 1;
        Ok(())
    }

    /// The current player's move, or a timeout (`action` None). Returns whether play goes on.
    pub fn play(&mut self, player: u64, action: Option<&Action>) -> Result<bool> {
        if self.now_turn() != Some(player) {
            return Err(Error::new("it is not your turn"));
        }
        let points = action.map(points_for).transpose()?;
        let score = self.scores.entry(player).or_insert(0);
        *score += points.unwrap_or(0);
        let reached = *score >= self.config.target_score;
        self.moves.push_back(Move { player, round: self.round, points });
        while self.moves.len() > LOG_MOVES {
            self.moves.pop_front();
        }
        self.turn += 1;
        if self.turn >= self.players.len() {
            self.turn = 0;
            self.round += 1;
        }
        if reached || self.round > self.config.max_rounds {
            self.finish();
            return Ok(false);
        }
        Ok(true)
    }

    fn finish(&mut self) {
        self.phase = Phase::Finished;
        // The highest score; the earlier player wins a tie.
        self.winner = self.players.iter().copied().max_by_key(|p| (self.scores.get(p).copied().unwrap_or(0), std::cmp::Reverse(*p)));
    }

    fn arm_turn(&self, ctx: &Ctx) -> Result<()> {
        if self.phase == Phase::Playing {
            ctx.timer_after("turn", self.config.turn_ms)
        } else {
            ctx.cancel_timer("turn");
            Ok(())
        }
    }
}

impl Hub for GameSession {
    fn spawn(ctx: &Ctx, _seed: &[u8]) -> Result<Self> {
        let s = Self::default();
        ctx.timer_after("idle", s.config.lobby_idle_ms)?;
        Ok(s)
    }

    fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
        decode(snapshot)
    }

    fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
        encode(self)
    }

    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        let out = match call.method {
            "state" => return encode(&*self),
            "join" => {
                self.join(call.player()?)?;
                ctx.cancel_timer("idle");
                encode(&true)
            }
            "leave" => {
                if self.leave(call.player()?) {
                    self.arm_turn(ctx)?;
                }
                encode(&true)
            }
            "ready" => {
                let player = call.player()?;
                let req: Ready = call.decode()?;
                if self.phase != Phase::Lobby || !self.players.contains(&player) {
                    return Err(Error::new("join the lobby first"));
                }
                if req.ready {
                    self.ready.insert(player);
                } else {
                    self.ready.remove(&player);
                }
                encode(&req.ready)
            }
            "start" => {
                self.start(call.player()?)?;
                self.arm_turn(ctx)?;
                encode(&true)
            }
            "act" => {
                let player = call.player()?;
                let action: Action = call.decode()?;
                let more = self.play(player, Some(&action))?;
                self.arm_turn(ctx)?;
                encode(&more)
            }
            "configure" => {
                let allowed = match call.caller {
                    Caller::Developer(_) => true,
                    Caller::Player(p) => self.host == Some(p),
                    _ => false,
                };
                if !allowed || self.phase != Phase::Lobby {
                    return Err(Error::new("the host configures the session in the lobby"));
                }
                let config: Config = call.decode()?;
                if config.min_players < 1 || config.max_players < config.min_players || config.max_players > 64 {
                    return Err(Error::new("1 <= min_players <= max_players <= 64"));
                }
                if config.turn_ms < 1_000 {
                    return Err(Error::new("turn_ms is at least 1000"));
                }
                self.config = config;
                encode(&true)
            }
            "reserve" => {
                if !matches!(call.caller, Caller::Instance(_) | Caller::Developer(_)) {
                    return Err(Error::new("reserve is for the matchmaker"));
                }
                let req: Reserve = call.decode()?;
                if self.phase != Phase::Lobby || !self.players.is_empty() {
                    return Err(Error::new("the session is already in use"));
                }
                if req.players.is_empty() || req.players.len() > self.config.max_players {
                    return Err(Error::new("reserve 1 to max_players players"));
                }
                self.reserved = req.players;
                encode(&true)
            }
            other => return Err(Error::unknown_method(other)),
        };
        ctx.publish("session", &encode(&*self)?);
        ctx.persist_now();
        out
    }

    fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
        match timer {
            "turn" => {
                if let Some(player) = self.now_turn() {
                    self.play(player, None)?;
                    self.arm_turn(ctx)?;
                }
            }
            "idle" if self.phase == Phase::Lobby && self.players.is_empty() => self.phase = Phase::Finished,
            _ => return Ok(()),
        }
        ctx.publish("session", &encode(&*self)?);
        Ok(())
    }

    fn on_session(&mut self, _ctx: &Ctx, player: u64, session: Session) -> Result<()> {
        match session {
            Session::Joined => {
                self.connected.insert(player);
            }
            Session::Left => {
                self.connected.remove(&player);
                if self.host == Some(player)
                    && let Some(next) = self.players.iter().find(|p| self.connected.contains(p))
                {
                    self.host = Some(*next);
                }
            }
        }
        Ok(())
    }
}

ckx_sdk::export_hub!(GameSession);

#[cfg(test)]
mod tests {
    use super::*;

    fn lobby(players: &[u64]) -> GameSession {
        let mut s = GameSession::default();
        for p in players {
            s.join(*p).unwrap();
            s.ready.insert(*p);
        }
        s
    }

    #[test]
    fn the_first_to_join_hosts_and_only_the_host_starts_when_all_are_ready() {
        let mut s = lobby(&[1, 2]);
        assert_eq!(s.host, Some(1));
        s.ready.remove(&2);
        assert!(s.start(1).is_err(), "not everyone is ready");
        s.ready.insert(2);
        assert!(s.start(2).is_err(), "not the host");
        s.start(1).unwrap();
        assert_eq!((s.phase, s.round, s.now_turn()), (Phase::Playing, 1, Some(1)));
        assert!(s.join(3).is_err(), "started");
    }

    #[test]
    fn turns_go_round_timeouts_skip_and_the_target_wins() {
        let mut s = lobby(&[1, 2]);
        s.config.target_score = 4;
        s.start(1).unwrap();
        assert!(s.play(2, Some(&Action { points: 1 })).is_err(), "not your turn");
        assert!(s.play(1, Some(&Action { points: 9 })).is_err(), "the rule refuses it");
        assert!(s.play(1, Some(&Action { points: 3 })).unwrap());
        assert!(s.play(2, None).unwrap(), "a timeout scores nothing");
        assert_eq!(s.round, 2);
        assert!(!s.play(1, Some(&Action { points: 1 })).unwrap());
        assert_eq!((s.phase, s.winner), (Phase::Finished, Some(1)));
        assert_eq!(s.moves.iter().filter(|m| m.points.is_none()).count(), 1);
    }

    #[test]
    fn a_reserved_session_admits_only_its_players() {
        let mut s = GameSession { reserved: vec![1, 2], ..Default::default() };
        assert!(s.join(3).is_err());
        s.join(2).unwrap();
        assert_eq!(s.host, Some(2));
    }

    #[test]
    fn leaving_hands_on_the_host_and_the_turn_and_too_few_players_ends_it() {
        let mut s = lobby(&[1, 2, 3]);
        s.connected.extend([2, 3]);
        s.start(1).unwrap();
        assert!(s.leave(1), "it was their turn");
        assert_eq!((s.host, s.now_turn()), (Some(2), Some(2)));
        s.scores.insert(3, 5);
        assert!(!s.leave(2));
        assert_eq!((s.phase, s.winner), (Phase::Finished, Some(3)));
    }
}
