//! An arena, keyed by its id: players fight in real time with the abilities of the class they
//! pick, and the hub referees every hit. The shape of a brawler, an arena shooter, a skirmish:
//! clients report where their player is (`move { x, z }`, a few times a second) and the movement
//! warden (`ckx_sdk::kit::play::movement`) keeps what the class's speed allows; a cast is checked
//! for its cooldown, resource and range (`ckx_sdk::kit::play::abilities`) and lands as the hub
//! decides: a hit-scan strike on the fighter nearest its aim, a projectile the hub flies a tick
//! at a time, an area that goes off after its delay; statuses tick (`ckx_sdk::kit::play::combat`);
//! the fallen come back after `respawn_ms`; the first to `kill_target` kills, or the most kills
//! when the time runs out, wins. Replace the [`Config`]'s classes with your game's.
//!
//! Positions are the arena's own units, `(0, 0)` to `(width, depth)`. With an `anchor` (the
//! chunk the arena is at in the world), casts, impacts and the warden's flags also go out as
//! realtime events (94 and 95) to the players around it.
//!
//! Endpoints (players): `join { class }`, `start` (the first to join, or a developer), `move
//! { x, z }` (answers the position kept), `cast { ability, x, z }` (aimed at a point), `leave`,
//! `state` (anyone). It speaks the kit protocol (`ckx_sdk::kit`): the `arena` topic carries the
//! fighters (health, resource, statuses, kills, positions), projectiles and pending areas as
//! `snapshot` messages up to five times a second, and `event` messages (`type`: `cast`,
//! `impact`, `kill`, `respawn`, `violation`, `removed`, `finished`) as they happen;
//! `snapshot { topic: "arena" }` answers it; changes may carry an `action_id`; refusals carry
//! `kit:<code>:` codes.

use ckx_sdk::kit::play::abilities::{AbilityKind, AbilitySpec, CasterBook, PendingAoe, Projectile, ProjectileStep, validate_cast};
use ckx_sdk::kit::play::combat::{Combatant, RespawnBook};
use ckx_sdk::kit::play::movement::{Rules, Warden};
use ckx_sdk::kit::{self, Publisher, Receipts, codes, wire};
use ckx_sdk::prelude::*;
use serde_json::{Value, json};

pub const TOPIC: &str = "arena";
pub const TICK_MS: u64 = 100;
/// Ticks between snapshots while nothing else publishes.
const PUBLISH_EVERY: u64 = 2;
/// How near its aim a hit-scan strike finds its victim.
const AIM_RADIUS: f64 = 1.5;
const EVENT_DISTANCE: u8 = 3;

/// A status an ability leaves on its victim.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Affliction {
    pub kind: String,
    pub per_tick: i64,
    pub every_ms: u64,
    pub duration_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Ability {
    #[serde(flatten)]
    pub spec: AbilitySpec,
    #[serde(default)]
    pub status: Option<Affliction>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Class {
    pub id: String,
    pub max_hp: i64,
    /// Units a second.
    pub speed: f64,
    pub abilities: Vec<Ability>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub width: f64,
    pub depth: f64,
    pub spawns: Vec<(f64, f64)>,
    pub classes: Vec<Class>,
    pub players_max: usize,
    pub kill_target: u32,
    pub duration_ms: u64,
    pub respawn_ms: u64,
    /// A reported move this far at once is refused.
    pub teleport: f64,
    /// The warden's strikes (in its window) that remove a player; 0 never does.
    pub remove_after_strikes: u32,
    /// The chunk the arena is at, for realtime events.
    pub anchor: Option<(i64, i64, i64)>,
}

fn ability(id: &str, cooldown_ms: u64, cost: i64, range: f32, kind: AbilityKind, status: Option<Affliction>) -> Ability {
    Ability { spec: AbilitySpec { ability_id: id.into(), cooldown_ms, resource_cost: cost, range, kind }, status }
}

impl Default for Config {
    fn default() -> Self {
        let warrior = Class {
            id: "warrior".into(),
            max_hp: 150,
            speed: 6.0,
            abilities: vec![
                ability("strike", 600, 0, 2.5, AbilityKind::Instant { damage: 18 }, None),
                ability("slam", 4_000, 30, 4.0, AbilityKind::Aoe { radius: 3.0, damage: 25, delay_ms: 300, falloff: true }, None),
            ],
        };
        let mage = Class {
            id: "mage".into(),
            max_hp: 100,
            speed: 6.5,
            abilities: vec![
                ability("bolt", 800, 10, 18.0, AbilityKind::Projectile { speed: 20.0, damage: 22, max_range: 18.0, hit_radius: 0.8 }, None),
                ability(
                    "ignite",
                    5_000,
                    25,
                    10.0,
                    AbilityKind::Instant { damage: 6 },
                    Some(Affliction { kind: "burning".into(), per_tick: 4, every_ms: 500, duration_ms: 3_000 }),
                ),
            ],
        };
        Self {
            width: 40.0,
            depth: 40.0,
            spawns: vec![(5.0, 5.0), (35.0, 35.0), (5.0, 35.0), (35.0, 5.0)],
            classes: vec![warrior, mage],
            players_max: 10,
            kill_target: 10,
            duration_ms: 5 * 60_000,
            respawn_ms: 3_000,
            teleport: 12.0,
            remove_after_strikes: 8,
            anchor: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Waiting,
    Running,
    Finished,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Fighter {
    pub player: u64,
    pub class: String,
    pub combatant: Combatant,
    pub caster: CasterBook,
    pub warden: Warden,
    pub spawn: (f64, f64),
}

impl Fighter {
    pub fn at(&self) -> (f64, f64) {
        self.warden.position().unwrap_or(self.spawn)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FighterView {
    pub player: u64,
    pub class: String,
    pub x: f64,
    pub z: f64,
    pub hp: i64,
    pub max_hp: i64,
    pub resource: i64,
    pub statuses: Vec<String>,
    pub kills: u32,
    pub deaths: u32,
    pub down: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Shot {
    pub ability: String,
    pub caster: u64,
    pub x: f64,
    pub z: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Blast {
    pub ability: String,
    pub caster: u64,
    pub x: f64,
    pub z: f64,
    pub radius: f64,
    pub at: u64,
}

/// The arena as everyone sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ArenaView {
    pub phase: Phase,
    pub width: f64,
    pub depth: f64,
    pub fighters: Vec<FighterView>,
    pub projectiles: Vec<Shot>,
    pub areas: Vec<Blast>,
    pub kill_target: u32,
    pub ends_at: Option<u64>,
    pub winner: Option<u64>,
}

#[derive(Deserialize)]
struct Join {
    class: String,
}

#[derive(Deserialize)]
struct Move {
    x: f64,
    z: f64,
}

#[derive(Deserialize)]
struct Cast {
    ability: String,
    x: f64,
    z: f64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Arena {
    pub config: Config,
    pub phase: Phase,
    pub fighters: Vec<Fighter>,
    pub projectiles: Vec<Projectile>,
    pub areas: Vec<PendingAoe>,
    pub respawns: RespawnBook,
    pub ends_at: Option<u64>,
    pub winner: Option<u64>,
    pub ticks: u64,
    /// Events since they were last published.
    #[serde(skip)]
    pub events: Vec<Value>,
    /// Whether the view changed since it was last published.
    #[serde(skip)]
    pub dirty: bool,
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

impl Arena {
    pub fn view(&self) -> ArenaView {
        ArenaView {
            phase: self.phase,
            width: self.config.width,
            depth: self.config.depth,
            fighters: self
                .fighters
                .iter()
                .map(|f| {
                    let (x, z) = f.at();
                    FighterView {
                        player: f.player,
                        class: f.class.clone(),
                        x,
                        z,
                        hp: f.combatant.health.current,
                        max_hp: f.combatant.health.max,
                        resource: f.caster.resource,
                        statuses: f.combatant.statuses.iter().map(|s| s.kind.clone()).collect(),
                        kills: f.combatant.kills,
                        deaths: f.combatant.deaths,
                        down: self.respawns.is_down(&f.player.to_string()),
                    }
                })
                .collect(),
            projectiles: self
                .projectiles
                .iter()
                .map(|p| Shot {
                    ability: p.ability_id.clone(),
                    caster: p.caster_id.parse().unwrap_or(0),
                    x: f64::from(p.x),
                    z: f64::from(p.z),
                })
                .collect(),
            areas: self
                .areas
                .iter()
                .map(|a| Blast {
                    ability: a.ability_id.clone(),
                    caster: a.caster_id.parse().unwrap_or(0),
                    x: f64::from(a.x),
                    z: f64::from(a.z),
                    radius: f64::from(a.radius),
                    at: a.detonate_at_ms,
                })
                .collect(),
            kill_target: self.config.kill_target,
            ends_at: self.ends_at,
            winner: self.winner,
        }
    }

    fn event(&mut self, kind: &str, extra: Value) {
        let mut e = json!({ "type": kind });
        if let (Some(e), Some(x)) = (e.as_object_mut(), extra.as_object()) {
            e.extend(x.clone());
        }
        self.events.push(e);
        self.dirty = true;
    }

    fn index(&self, player: u64) -> Result<usize> {
        self.fighters.iter().position(|f| f.player == player).ok_or_else(|| kit::error(codes::NOT_MEMBER, "join the arena first"))
    }

    fn rules(&self, class: &str) -> Rules {
        let speed = self.config.classes.iter().find(|c| c.id == class).map_or(6.0, |c| c.speed);
        Rules {
            max_speed: speed,
            teleport: self.config.teleport,
            bounds: Some((0.0, 0.0, self.config.width, self.config.depth)),
            grace: 1.25,
        }
    }

    pub fn join(&mut self, player: u64, class: &str, now: u64) -> Result<()> {
        let Some(c) = self.config.classes.iter().find(|c| c.id == class).cloned() else {
            let names: Vec<&str> = self.config.classes.iter().map(|c| c.id.as_str()).collect();
            return Err(kit::error(codes::BAD_REQUEST, format!("a class is one of {}", names.join(", "))));
        };
        if let Ok(i) = self.index(player) {
            if self.phase == Phase::Running {
                return Err(kit::error(codes::STARTED, "classes are picked before the fight"));
            }
            self.fighters[i].class = c.id;
            self.fighters[i].combatant = Combatant::new(c.max_hp);
            self.dirty = true;
            return Ok(());
        }
        if self.fighters.len() >= self.config.players_max {
            return Err(kit::error(codes::FULL, "the arena is full"));
        }
        let spawn = self.config.spawns.get(self.fighters.len() % self.config.spawns.len().max(1)).copied().unwrap_or((0.0, 0.0));
        self.fighters.push(Fighter {
            player,
            class: c.id,
            combatant: Combatant::new(c.max_hp),
            caster: CasterBook::default(),
            warden: Warden::at(spawn, now),
            spawn,
        });
        self.dirty = true;
        Ok(())
    }

    pub fn leave(&mut self, player: u64) -> Result<()> {
        let i = self.index(player)?;
        self.fighters.remove(i);
        self.respawns.kills.remove(&player.to_string());
        self.dirty = true;
        Ok(())
    }

    pub fn start(&mut self, now: u64) -> Result<()> {
        if self.phase == Phase::Running {
            return Err(kit::error(codes::STARTED, "the fight is on"));
        }
        if self.fighters.len() < 2 {
            return Err(kit::error(codes::NOT_READY, "a fight takes two"));
        }
        self.phase = Phase::Running;
        self.ends_at = Some(now + self.config.duration_ms);
        self.winner = None;
        self.projectiles.clear();
        self.areas.clear();
        self.respawns = RespawnBook::default();
        let classes = self.config.classes.clone();
        for f in &mut self.fighters {
            let max = classes.iter().find(|c| c.id == f.class).map_or(100, |c| c.max_hp);
            f.combatant = Combatant::new(max);
            f.caster = CasterBook::default();
            f.warden = Warden::at(f.spawn, now);
        }
        self.dirty = true;
        Ok(())
    }

    fn standing(&self, i: usize) -> Result<()> {
        if self.phase != Phase::Running {
            return Err(kit::error(codes::NOT_READY, "the fight has not started"));
        }
        if self.respawns.is_down(&self.fighters[i].player.to_string()) {
            return Err(kit::error(codes::CONFLICT, "you are down"));
        }
        Ok(())
    }

    /// A reported position: the warden keeps what the class allows. Returns the position kept.
    pub fn moved(&mut self, player: u64, to: (f64, f64), now: u64) -> Result<(f64, f64)> {
        let i = self.index(player)?;
        self.standing(i)?;
        let rules = self.rules(&self.fighters[i].class);
        let verdict = self.fighters[i].warden.check(&rules, to, now);
        if let Some(v) = verdict.violation {
            self.event("violation", json!({ "player": player, "what": v.kind.as_str(), "detail": v.detail }));
            let limit = self.config.remove_after_strikes;
            if limit > 0 && self.fighters[i].warden.strikes(now) >= limit {
                self.fighters.remove(i);
                self.respawns.kills.remove(&player.to_string());
                self.event("removed", json!({ "player": player, "why": "movement" }));
                return Err(kit::error(codes::FORBIDDEN, "removed for movement the warden refused"));
            }
        }
        self.dirty = true;
        Ok(verdict.at)
    }

    /// A cast aimed at `aim`. Returns whether it hit someone at once (a hit-scan strike).
    pub fn cast(&mut self, player: u64, ability_id: &str, aim: (f64, f64), now: u64) -> Result<bool> {
        let i = self.index(player)?;
        self.standing(i)?;
        let class = self.fighters[i].class.clone();
        let Some(ability) = self
            .config
            .classes
            .iter()
            .find(|c| c.id == class)
            .and_then(|c| c.abilities.iter().find(|a| a.spec.ability_id == ability_id))
            .cloned()
        else {
            return Err(kit::error(codes::NOT_FOUND, format!("a {class} has no ability `{ability_id}`")));
        };
        let from = self.fighters[i].at();
        let to_f32 = |p: (f64, f64)| (p.0 as f32, p.1 as f32);
        let cast = validate_cast(&ability.spec, &mut self.fighters[i].caster, to_f32(from), to_f32(aim), now).map_err(|why| match why {
            "ability on cooldown" => kit::error(codes::COOLDOWN, why),
            "not enough resource" => kit::error(codes::INSUFFICIENT, why),
            _ => kit::error(codes::OUT_OF_REACH, why),
        })?;
        self.event("cast", json!({ "ability": ability_id, "caster": player, "x": aim.0, "z": aim.1 }));
        match ability.spec.kind {
            AbilityKind::Instant { damage } => {
                let victim = self
                    .fighters
                    .iter()
                    .enumerate()
                    .filter(|(j, f)| *j != i && !self.respawns.is_down(&f.player.to_string()) && dist(f.at(), aim) <= AIM_RADIUS)
                    .min_by(|a, b| dist(a.1.at(), aim).total_cmp(&dist(b.1.at(), aim)))
                    .map(|(j, _)| j);
                if let Some(j) = victim {
                    self.damage(player, j, ability_id, damage, ability.status.as_ref(), now);
                    return Ok(true);
                }
                Ok(false)
            }
            AbilityKind::Projectile { .. } => {
                self.projectiles.extend(Projectile::launch(&ability.spec, &player.to_string(), to_f32(from), &cast, now));
                Ok(false)
            }
            AbilityKind::Aoe { .. } => {
                self.areas.extend(PendingAoe::from_cast(&ability.spec, &player.to_string(), &cast, now));
                Ok(false)
            }
        }
    }

    fn damage(&mut self, caster: u64, victim: usize, ability_id: &str, amount: i64, status: Option<&Affliction>, now: u64) {
        let f = &mut self.fighters[victim];
        let target = f.player;
        let outcome = f.combatant.hit(amount);
        if let Some(s) = status.filter(|_| !outcome.killed) {
            f.combatant.afflict(&s.kind, s.per_tick, s.every_ms, s.duration_ms, Some(caster.to_string()), now);
        }
        self.event("impact", json!({ "ability": ability_id, "caster": caster, "victim": target, "damage": outcome.dealt }));
        if outcome.killed {
            self.killed(caster, victim, now);
        }
    }

    fn killed(&mut self, killer: u64, victim: usize, now: u64) {
        let target = self.fighters[victim].player;
        if killer != target
            && let Some(k) = self.fighters.iter_mut().find(|f| f.player == killer)
        {
            k.combatant.kills += 1;
        }
        self.respawns.record_kill(&target.to_string(), &killer.to_string(), now, self.config.respawn_ms);
        self.event("kill", json!({ "killer": killer, "victim": target }));
    }

    /// One tick: projectiles, areas, statuses, respawns, the end. Returns whether it ran.
    pub fn tick(&mut self, now: u64) -> bool {
        if self.phase != Phase::Running {
            return false;
        }
        self.ticks += 1;
        let mut shots = std::mem::take(&mut self.projectiles);
        let positions: Vec<(String, (f64, f64), bool)> =
            self.fighters.iter().map(|f| (f.player.to_string(), f.at(), self.respawns.is_down(&f.player.to_string()))).collect();
        let mut hits = Vec::new();
        shots.retain_mut(|p| {
            let caster = p.caster_id.clone();
            let mut hit_test = |x: f32, z: f32, r: f32| {
                positions
                    .iter()
                    .filter(|(id, _, down)| *id != caster && !down)
                    .find(|(_, at, _)| dist(*at, (f64::from(x), f64::from(z))) <= f64::from(r))
                    .map(|(id, _, _)| id.clone())
            };
            let (w, d) = (self.config.width as f32, self.config.depth as f32);
            let mut blocked = |x: f32, z: f32| x < 0.0 || z < 0.0 || x > w || z > d;
            match p.step(TICK_MS, &mut hit_test, &mut blocked) {
                ProjectileStep::Flying => true,
                ProjectileStep::Hit { target_id } => {
                    hits.push((p.caster_id.clone(), target_id, p.ability_id.clone(), p.damage));
                    false
                }
                ProjectileStep::Expired => false,
            }
        });
        self.projectiles = shots;
        if !self.projectiles.is_empty() || !hits.is_empty() {
            self.dirty = true;
        }
        for (caster, target, ability_id, damage) in hits {
            if let (Ok(caster), Some(j)) = (caster.parse::<u64>(), self.fighters.iter().position(|f| f.player.to_string() == target)) {
                let status = self.status_of(&ability_id);
                self.damage(caster, j, &ability_id, damage, status.as_ref(), now);
            }
        }
        let (ready, waiting): (Vec<PendingAoe>, Vec<PendingAoe>) = std::mem::take(&mut self.areas).into_iter().partition(|a| a.ready(now));
        self.areas = waiting;
        for blast in ready {
            let victims: Vec<(String, (f32, f32))> = self
                .fighters
                .iter()
                .filter(|f| !self.respawns.is_down(&f.player.to_string()))
                .map(|f| (f.player.to_string(), (f.at().0 as f32, f.at().1 as f32)))
                .collect();
            let caster: u64 = blast.caster_id.parse().unwrap_or(0);
            let status = self.status_of(&blast.ability_id);
            for (id, damage) in blast.resolve(&victims) {
                if let Some(j) = self.fighters.iter().position(|f| f.player.to_string() == id) {
                    self.damage(caster, j, &blast.ability_id, damage, status.as_ref(), now);
                }
            }
            self.dirty = true;
        }
        for i in 0..self.fighters.len() {
            for effect in self.fighters[i].combatant.tick(now) {
                self.dirty = true;
                if effect.outcome.killed {
                    let killer = effect.source.as_deref().and_then(|s| s.parse().ok()).unwrap_or(self.fighters[i].player);
                    self.killed(killer, i, now);
                }
            }
        }
        for id in self.respawns.due(now) {
            if let Some(f) = self.fighters.iter_mut().find(|f| f.player.to_string() == id) {
                f.combatant.health.revive();
                f.combatant.statuses.clear();
                f.warden.place(f.spawn, now);
                let player = f.player;
                self.event("respawn", json!({ "player": player }));
            }
        }
        let leader = self.fighters.iter().max_by_key(|f| f.combatant.kills).map(|f| (f.player, f.combatant.kills));
        let tied = leader.is_some_and(|(p, k)| self.fighters.iter().any(|f| f.player != p && f.combatant.kills == k));
        let reached = leader.is_some_and(|(_, k)| k >= self.config.kill_target);
        if reached || self.ends_at.is_some_and(|t| now >= t) {
            self.phase = Phase::Finished;
            self.winner = leader.filter(|_| reached || !tied).map(|(p, _)| p);
            self.event("finished", json!({ "winner": self.winner }));
        }
        true
    }

    fn status_of(&self, ability_id: &str) -> Option<Affliction> {
        self.config.classes.iter().flat_map(|c| &c.abilities).find(|a| a.spec.ability_id == ability_id).and_then(|a| a.status.clone())
    }
}

/// The hub: the arena, and what the kit protocol keeps beside it.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ArenaHub {
    #[serde(flatten)]
    pub arena: Arena,
    #[serde(default)]
    pub kit: Publisher,
    #[serde(default)]
    pub receipts: Receipts,
}

impl ArenaHub {
    fn act(&mut self, ctx: &Ctx, call: &Call<'_>, now: u64) -> Result<Vec<u8>> {
        let a = &mut self.arena;
        let out = match call.method {
            "join" => {
                let j: Join = kit::args(call)?;
                a.join(call.player()?, &j.class, now)?;
                encode(&true)
            }
            "leave" => {
                a.leave(call.player()?)?;
                encode(&true)
            }
            "start" => {
                let allowed = match call.caller {
                    Caller::Developer(_) => true,
                    Caller::Player(p) => a.fighters.first().map(|f| f.player) == Some(p),
                    _ => false,
                };
                if !allowed {
                    return Err(kit::error(codes::FORBIDDEN, "the first to join, or a developer, starts it"));
                }
                a.start(now)?;
                ctx.timer_every("tick", TICK_MS)?;
                encode(&true)
            }
            "move" => {
                let m: Move = kit::args(call)?;
                let kept = a.moved(call.player()?, (m.x, m.z), now);
                self.flush(ctx, false)?;
                return encode(&kept?);
            }
            "cast" => {
                let c: Cast = kit::args(call)?;
                encode(&a.cast(call.player()?, &c.ability, (c.x, c.z), now)?)
            }
            other => return Err(Error::unknown_method(other)),
        };
        self.flush(ctx, true)?;
        out
    }

    /// Publishes the events since the last time, and the arena when `snapshot` says so.
    fn flush(&mut self, ctx: &Ctx, snapshot: bool) -> Result<()> {
        let events = std::mem::take(&mut self.arena.events);
        for e in &events {
            self.kit.event(ctx, TOPIC, e)?;
            self.realtime(ctx, e);
        }
        if snapshot {
            self.arena.dirty = false;
            self.kit.snapshot(ctx, TOPIC, &self.arena.view())?;
        }
        Ok(())
    }

    /// Casts, impacts and the warden's flags as realtime events, when the arena is anchored.
    fn realtime(&self, ctx: &Ctx, e: &Value) {
        let Some((x, y, z)) = self.arena.config.anchor else { return };
        let at = ChunkPos::new(x, y, z);
        let actor = actor_id(&format!("arena:{}", ctx.key));
        let id = |k: &str| e[k].as_u64().map(|v| v.to_string());
        let sent = match e["type"].as_str() {
            Some(kind @ ("cast" | "impact")) => {
                let body = wire::AbilityUse {
                    kind: kind.into(),
                    ability_id: e["ability"].as_str().unwrap_or_default().into(),
                    caster_id: id("caster").unwrap_or_default(),
                    victim_id: id("victim"),
                    damage: e["damage"].as_i64().unwrap_or(0),
                };
                wire::emit_event(ctx, at, EVENT_DISTANCE, &actor, wire::ABILITY, &body)
            }
            Some("violation") => {
                let body = wire::MovementViolation {
                    kind: e["what"].as_str().unwrap_or_default().into(),
                    user_id: id("player").unwrap_or_default(),
                    detail: e["detail"].as_str().unwrap_or_default().into(),
                };
                wire::emit_event(ctx, at, EVENT_DISTANCE, &actor, wire::MOVEMENT_VIOLATION, &body)
            }
            _ => Ok(()),
        };
        if let Err(Error(why)) = sent {
            ctx.log(Level::Warn, &format!("a realtime event did not go out: {why}"));
        }
    }
}

impl Hub for ArenaHub {
    fn spawn(_ctx: &Ctx, seed: &[u8]) -> Result<Self> {
        let mut hub = Self::default();
        if !seed.is_empty() {
            hub.arena.config = serde_json::from_slice(seed).map_err(|e| Error::new(format!("seed is not a JSON config: {e}")))?;
        }
        Ok(hub)
    }

    fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
        decode(snapshot)
    }

    fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
        encode(self)
    }

    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        let now = ctx.now_ms();
        match call.method {
            "state" => encode(&self.arena.view()),
            kit::SNAPSHOT_METHOD => self.kit.snapshot_reply(ctx, TOPIC, &self.arena.view()),
            _ => Receipts::run(self, |h| &mut h.receipts, &call, now, |h| h.act(ctx, &call, now)),
        }
    }

    fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
        if timer != "tick" {
            return Ok(());
        }
        let now = ctx.now_ms();
        if !self.arena.tick(now) {
            ctx.cancel_timer("tick");
            return Ok(());
        }
        let ended = self.arena.phase != Phase::Running;
        if ended {
            ctx.cancel_timer("tick");
            ctx.persist_now();
        }
        let due = self.arena.dirty && (ended || self.arena.ticks.is_multiple_of(PUBLISH_EVERY));
        self.flush(ctx, due)
    }
}

ckx_sdk::export_hub!(ArenaHub);

#[cfg(test)]
mod tests {
    use super::*;
    use ckx_sdk::kit::{Message, error_code};
    use ckx_sdk::testing::TestHub;

    fn code(e: &CallError) -> Option<&str> {
        error_code(&e.message).map(|c| c.0)
    }

    fn duel() -> Arena {
        let mut a = Arena { config: Config { spawns: vec![(10.0, 10.0), (12.0, 10.0)], ..Config::default() }, ..Default::default() };
        a.join(1, "warrior", 0).unwrap();
        a.join(2, "mage", 0).unwrap();
        a.start(0).unwrap();
        a
    }

    #[test]
    fn a_strike_lands_on_the_fighter_nearest_its_aim_within_range_and_cooldown() {
        let mut a = duel();
        assert!(a.cast(1, "strike", (12.0, 10.0), 1_000).unwrap());
        assert_eq!(a.fighters[1].combatant.health.current, 100 - 18);
        let Error(e) = a.cast(1, "strike", (12.0, 10.0), 1_100).unwrap_err();
        assert_eq!(error_code(&e).map(|c| c.0), Some(codes::COOLDOWN));
        let Error(e) = a.cast(1, "strike", (30.0, 10.0), 2_000).unwrap_err();
        assert_eq!(error_code(&e).map(|c| c.0), Some(codes::OUT_OF_REACH));
        assert!(!a.cast(1, "strike", (10.0, 12.0), 3_000).unwrap(), "a miss spends the strike");
        let Error(e) = a.cast(1, "bolt", (12.0, 10.0), 4_000).unwrap_err();
        assert_eq!(error_code(&e).map(|c| c.0), Some(codes::NOT_FOUND));
    }

    #[test]
    fn a_bolt_flies_and_lands_and_a_burn_ticks_until_it_kills_with_credit() {
        let mut a = duel();
        a.fighters[0].warden.place((20.0, 10.0), 0);
        a.cast(2, "bolt", (20.0, 10.0), 1_000).unwrap();
        assert_eq!(a.projectiles.len(), 1);
        let mut now = 1_000;
        while !a.projectiles.is_empty() && now < 3_000 {
            now += TICK_MS;
            a.tick(now);
        }
        assert_eq!(a.fighters[0].combatant.health.current, 150 - 22);
        a.fighters[0].combatant.health.current = 10;
        a.fighters[0].warden.place((14.0, 10.0), now);
        assert!(a.cast(2, "ignite", (14.0, 10.0), now).unwrap());
        assert!(a.fighters[0].combatant.has("burning"));
        for _ in 0..40 {
            now += TICK_MS;
            a.tick(now);
        }
        assert_eq!(a.fighters[1].combatant.kills, 1, "the burn's source gets the kill");
        assert_eq!(a.fighters[0].combatant.health.current, 150, "back after the respawn");
        assert!(a.events.iter().any(|e| e["type"] == "respawn"));
    }

    #[test]
    fn a_slam_goes_off_after_its_delay_and_the_first_to_the_target_wins() {
        let mut a = duel();
        a.config.kill_target = 1;
        a.fighters[1].combatant.health.current = 10;
        a.cast(1, "slam", (12.0, 10.0), 1_000).unwrap();
        a.tick(1_100);
        assert_eq!(a.fighters[1].combatant.health.current, 10, "not yet");
        a.tick(1_400);
        assert_eq!(a.phase, Phase::Finished);
        assert_eq!(a.winner, Some(1));
    }

    #[test]
    fn the_warden_cuts_fast_moves_refuses_teleports_and_removes_a_cheat() {
        let mut a = duel();
        let kept = a.moved(1, (11.0, 10.0), 500).unwrap();
        assert_eq!(kept, (11.0, 10.0));
        let kept = a.moved(1, (30.0, 10.0), 600).unwrap();
        assert_eq!(kept, (11.0, 10.0), "a teleport is refused");
        let kept = a.moved(1, (18.0, 10.0), 1_100).unwrap();
        assert!(kept.0 < 18.0 && kept.0 > 11.0, "a fast move is cut: {kept:?}");
        a.config.remove_after_strikes = 3;
        let Error(e) = a.moved(1, (40.0, 40.0), 1_200).unwrap_err();
        assert_eq!(error_code(&e).map(|c| c.0), Some(codes::FORBIDDEN));
        assert!(a.fighters.iter().all(|f| f.player != 1));
    }

    #[test]
    fn the_topic_carries_snapshots_and_events_and_a_time_out_decides_by_kills() {
        let mut hub = TestHub::<ArenaHub>::spawn_with("arena", "pit", br#"{"duration_ms":2000,"spawns":[[10,10],[11,10]]}"#).unwrap();
        let _: bool = hub.call_typed(Caller::Player(1), "join", &json!({ "class": "warrior" })).unwrap();
        let _: bool = hub.call_typed(Caller::Player(2), "join", &json!({ "class": "mage" })).unwrap();
        let e = hub.call(Caller::Player(3), "join", &encode(&json!({ "class": "bard" })).unwrap()).unwrap_err();
        assert_eq!(code(&e), Some(codes::BAD_REQUEST));
        let e = hub.call(Caller::Player(2), "start", &encode(&()).unwrap()).unwrap_err();
        assert_eq!(code(&e), Some(codes::FORBIDDEN));
        let _: bool = hub.call_typed(Caller::Player(1), "start", &()).unwrap();
        let hit: bool = hub.call_typed(Caller::Player(1), "cast", &json!({ "ability": "strike", "x": 11.0, "z": 10.0 })).unwrap();
        assert!(hit);
        hub.advance(2_500).unwrap();
        let view: ArenaView = hub.call_typed(Caller::Player(1), "state", &()).unwrap();
        assert_eq!((view.phase, view.winner), (Phase::Finished, None), "no kills: no winner");
        #[derive(Deserialize)]
        struct K {
            kind: ckx_sdk::kit::MessageKind,
        }
        let events: Vec<Message<Value>> = hub
            .published()
            .iter()
            .filter(|(_, b)| decode::<K>(b).unwrap().kind == ckx_sdk::kit::MessageKind::Event)
            .map(|(_, b)| decode(b).unwrap())
            .collect();
        let types: Vec<&str> = events.iter().filter_map(|m| m.body["type"].as_str()).collect();
        assert_eq!(types, vec!["cast", "impact", "finished"]);
        assert!(hub.timers().is_empty());
    }

    #[test]
    fn an_anchored_arena_sends_its_casts_as_realtime_events() {
        let mut hub = TestHub::<ArenaHub>::spawn_with("arena", "pit", br#"{"anchor":[4,0,4],"spawns":[[10,10],[11,10]]}"#).unwrap();
        let _: bool = hub.call_typed(Caller::Player(1), "join", &json!({ "class": "warrior" })).unwrap();
        let _: bool = hub.call_typed(Caller::Player(2), "join", &json!({ "class": "mage" })).unwrap();
        let _: bool = hub.call_typed(Caller::Player(1), "start", &()).unwrap();
        let _: bool = hub.call_typed(Caller::Player(1), "cast", &json!({ "ability": "strike", "x": 11.0, "z": 10.0 })).unwrap();
        assert_eq!(hub.emitted().len(), 2, "the cast and its impact");
    }
}
