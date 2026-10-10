//! The mobs and NPCs of one area of the world, keyed `x,z` (the area's centre chunk), moved on a
//! timer and shown to nearby players as server-driven actors. Mobs wander near home, chase the
//! nearest player in range, go home past their leash, take players' hits and respawn; NPCs
//! walk their routes and talk. Mobs see further at night and under an event's `aggro`
//! modifier, from the root `world` hub's topic. Mobs and NPCs are typed containers
//! ([`ckx_sdk::model`]), so players read only their public properties. Replaces the
//! `mob-engine` and `npc-engine` templates.
//!
//! Endpoints: `state` (anyone), `attack { mob }` and `talk { npc }` (players). The type needs
//! the `world.read` scope (where players are) and may call `world` (for its topic). Changes to
//! public properties go out on the `area` topic once a second.

use std::collections::BTreeMap;

use ckx_sdk::prelude::*;
use serde_json::{Value, json};

pub const TICK_MS: u64 = 250;
/// Ticks between scans of where players are, and between `area` pushes.
const SCAN_EVERY: u64 = 4;
const MOBS: u64 = 6;
/// Distances are in chunks, speeds in chunks a second.
const AGGRO: f64 = 2.5;
const LEASH: f64 = 6.0;
const MOB_SPEED: f64 = 0.5;
const NPC_SPEED: f64 = 0.25;
/// How near a player must be to hit a mob.
const REACH: f64 = 1.5;
/// The server decides a hit's damage, never the client.
const HIT: i64 = 10;
const HIT_COOLDOWN_MS: u64 = 500;
const RESPAWN_MS: u64 = 10_000;
/// Players within this many chunks of a mob see it move.
const SHOW_DISTANCE: u8 = 2;

pub fn schema() -> Schema {
    Schema::new()
        .container("Mob", |c| {
            c.prop("kind", PropKind::Str, "wolf", Visibility::Public)
                .prop("hp", PropKind::Int, 100, Visibility::Public)
                .prop("max_hp", PropKind::Int, 100, Visibility::Public)
                .prop("x", PropKind::Float, 0.0, Visibility::Public)
                .prop("z", PropKind::Float, 0.0, Visibility::Public)
                .prop("mode", PropKind::Str, "idle", Visibility::Public)
                .prop("home_x", PropKind::Float, 0.0, Visibility::Hidden)
                .prop("home_z", PropKind::Float, 0.0, Visibility::Hidden)
                .prop("respawn_at", PropKind::Int, 0, Visibility::Hidden)
        })
        .container("Npc", |c| {
            c.prop("name", PropKind::Str, "", Visibility::Public)
                .prop("line", PropKind::Str, "Well met.", Visibility::Public)
                .prop("x", PropKind::Float, 0.0, Visibility::Public)
                .prop("z", PropKind::Float, 0.0, Visibility::Public)
                .prop("route", PropKind::Json, json!([]), Visibility::Hidden)
                .prop("leg", PropKind::Int, 0, Visibility::Hidden)
        })
}

/// What players near an actor receive as its state.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ActorState {
    pub kind: String,
    pub id: u64,
    pub x: f64,
    pub z: f64,
    pub hp: Option<i64>,
}

#[derive(Deserialize)]
struct Conditions {
    night: bool,
    #[serde(default)]
    modifiers: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct Attack {
    mob: u64,
}

#[derive(Deserialize)]
struct Talk {
    npc: u64,
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// `from` moved toward `to` by at most `step`.
fn toward(from: (f64, f64), to: (f64, f64), step: f64) -> (f64, f64) {
    let d = dist(from, to);
    if d <= step || d == 0.0 {
        return to;
    }
    (from.0 + (to.0 - from.0) * step / d, from.1 + (to.1 - from.1) * step / d)
}

#[derive(Serialize, Deserialize)]
pub struct Area {
    #[serde(skip, default = "schema")]
    schema: Schema,
    pub model: Model,
    pub center: (i64, i64),
    pub ticks: u64,
    pub night: bool,
    pub aggro_scale: f64,
    /// Where each nearby player was at the last scan, in chunks.
    pub players: BTreeMap<u64, (f64, f64)>,
    pub last_hit: BTreeMap<u64, u64>,
}

impl Area {
    pub fn new(key: &str) -> Result<Self> {
        let center = key.split_once(',').and_then(|(x, z)| Some((x.trim().parse().ok()?, z.trim().parse().ok()?))).unwrap_or((0, 0));
        let mut area = Self {
            schema: schema(),
            model: Model::new(),
            center,
            ticks: 0,
            night: false,
            aggro_scale: 1.0,
            players: BTreeMap::new(),
            last_hit: BTreeMap::new(),
        };
        let (cx, cz) = (center.0 as f64 + 0.5, center.1 as f64 + 0.5);
        for i in 0..MOBS {
            let angle = i as f64 * std::f64::consts::TAU / MOBS as f64;
            let home = (cx + 3.0 * angle.cos(), cz + 3.0 * angle.sin());
            let id = area.model.create(&area.schema, "Mob", None)?;
            for (k, v) in [("x", home.0), ("z", home.1), ("home_x", home.0), ("home_z", home.1)] {
                area.model.set(&area.schema, id, k, v)?;
            }
        }
        for (name, line, route) in [
            (
                "Warden",
                "Mind the wolves after dark.",
                json!([[cx - 2.0, cz - 2.0], [cx + 2.0, cz - 2.0], [cx + 2.0, cz + 2.0], [cx - 2.0, cz + 2.0]]),
            ),
            ("Trader", "Fair prices, mostly.", json!([[cx - 1.0, cz], [cx + 1.0, cz]])),
        ] {
            let id = area.model.create(&area.schema, "Npc", None)?;
            area.model.set(&area.schema, id, "name", name)?;
            area.model.set(&area.schema, id, "line", line)?;
            area.model.set(&area.schema, id, "x", route[0][0].as_f64().unwrap_or(cx))?;
            area.model.set(&area.schema, id, "z", route[0][1].as_f64().unwrap_or(cz))?;
            area.model.set(&area.schema, id, "route", route)?;
        }
        Ok(area)
    }

    fn pos(&self, id: u64) -> Result<(f64, f64)> {
        Ok((self.model.get_f64(&self.schema, id, "x")?, self.model.get_f64(&self.schema, id, "z")?))
    }

    fn place(&mut self, id: u64, to: (f64, f64)) -> Result<()> {
        if dist(self.pos(id)?, to) > 0.01 {
            self.model.set(&self.schema, id, "x", to.0)?;
            self.model.set(&self.schema, id, "z", to.1)?;
        }
        Ok(())
    }

    fn set_mode(&mut self, id: u64, mode: &str) -> Result<()> {
        if self.model.get_str(&self.schema, id, "mode")? != mode {
            self.model.set(&self.schema, id, "mode", mode)?;
        }
        Ok(())
    }

    pub fn step_mobs(&mut self, now: u64, dt: f64) -> Result<()> {
        let range = AGGRO * self.aggro_scale * if self.night { 1.5 } else { 1.0 };
        let ids: Vec<u64> = self.model.of_type("Mob").collect();
        for id in ids {
            let home = (self.model.get_f64(&self.schema, id, "home_x")?, self.model.get_f64(&self.schema, id, "home_z")?);
            let respawn_at = self.model.get_i64(&self.schema, id, "respawn_at")?;
            if respawn_at > 0 {
                if now < respawn_at as u64 {
                    continue;
                }
                let max_hp = self.model.get_i64(&self.schema, id, "max_hp")?;
                self.model.set(&self.schema, id, "hp", max_hp)?;
                self.model.set(&self.schema, id, "respawn_at", 0)?;
                self.place(id, home)?;
            }
            let at = self.pos(id)?;
            let prey = self
                .players
                .values()
                .map(|p| (dist(at, *p), *p))
                .filter(|(d, p)| *d <= range && dist(home, *p) <= LEASH)
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, p)| p);
            let (goal, mode) = if dist(at, home) > LEASH {
                (home, "return")
            } else if let Some(p) = prey {
                (p, "chase")
            } else {
                // Idle mobs circle their home slowly.
                let t = self.ticks as f64 * 0.02 + id as f64;
                ((home.0 + t.cos(), home.1 + t.sin()), "idle")
            };
            self.place(id, toward(at, goal, MOB_SPEED * dt))?;
            self.set_mode(id, mode)?;
        }
        Ok(())
    }

    pub fn step_npcs(&mut self, dt: f64) -> Result<()> {
        let ids: Vec<u64> = self.model.of_type("Npc").collect();
        for id in ids {
            let route = self.model.get(&self.schema, id, "route")?;
            let points: Vec<(f64, f64)> = route
                .as_array()
                .map(|a| a.iter().filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?))).collect())
                .unwrap_or_default();
            if points.is_empty() {
                continue;
            }
            let leg = self.model.get_i64(&self.schema, id, "leg")?.rem_euclid(points.len() as i64) as usize;
            let next = toward(self.pos(id)?, points[leg], NPC_SPEED * dt);
            self.place(id, next)?;
            if dist(next, points[leg]) < 0.05 {
                self.model.set(&self.schema, id, "leg", ((leg + 1) % points.len()) as i64)?;
            }
        }
        Ok(())
    }

    /// A player's hit on a mob: in reach at the last scan, not too fast, fixed damage.
    pub fn attack(&mut self, player: u64, mob: u64, now: u64) -> Result<i64> {
        if self.model.container(mob).is_none_or(|c| c.type_name != "Mob") {
            return Err(Error::new(format!("no mob {mob}")));
        }
        if self.last_hit.get(&player).is_some_and(|at| now < at + HIT_COOLDOWN_MS) {
            return Err(Error::new("attacking too fast"));
        }
        let at = *self.players.get(&player).ok_or_else(|| Error::new("you are not in this area"))?;
        if dist(at, self.pos(mob)?) > REACH {
            return Err(Error::new("out of reach"));
        }
        if self.model.get_i64(&self.schema, mob, "respawn_at")? > 0 {
            return Err(Error::new("that mob is down"));
        }
        self.last_hit.insert(player, now);
        let hp = self.model.add(&self.schema, mob, "hp", -HIT)?;
        if hp <= 0 {
            self.model.set(&self.schema, mob, "hp", 0)?;
            self.model.set(&self.schema, mob, "respawn_at", (now + RESPAWN_MS) as i64)?;
            self.set_mode(mob, "down")?;
        }
        Ok(hp.max(0))
    }

    pub fn state(&self) -> Result<Vec<BTreeMap<String, Value>>> {
        let ids = self.model.of_type("Mob").chain(self.model.of_type("Npc"));
        ids.map(|id| {
            let mut v = self.model.view(&self.schema, id, Viewer::Public)?;
            v.insert("id".into(), json!(id));
            Ok(v)
        })
        .collect()
    }

    fn scan(&mut self, ctx: &Ctx) {
        match ctx.world().actors_near(ChunkPos::new(self.center.0, 0, self.center.1), 4, 2) {
            Ok(actors) => {
                self.players =
                    actors.into_iter().filter_map(|a| Some((a.player?, (a.chunk.x as f64 + 0.5, a.chunk.z as f64 + 0.5)))).collect();
            }
            Err(e) if self.ticks % (SCAN_EVERY * 60) == 1 => ctx.log(Level::Warn, &format!("scanning for players: {e}")),
            Err(_) => {}
        }
    }

    fn show(&self, ctx: &Ctx) -> Result<()> {
        for type_name in ["Mob", "Npc"] {
            for id in self.model.of_type(type_name) {
                let hp = if type_name == "Mob" {
                    if self.model.get_i64(&self.schema, id, "respawn_at")? > 0 {
                        continue;
                    }
                    Some(self.model.get_i64(&self.schema, id, "hp")?)
                } else {
                    None
                };
                let (x, z) = self.pos(id)?;
                let actor = actor_id(&format!("{}{},{}:{id}", &type_name[..1], self.center.0, self.center.1));
                let state = ActorState { kind: type_name.to_lowercase(), id, x, z, hp };
                let _ = ctx.emit_actor(&actor, ChunkPos::new(x.floor() as i64, 0, z.floor() as i64), SHOW_DISTANCE, &encode(&state)?);
            }
        }
        Ok(())
    }
}

impl Hub for Area {
    fn spawn(ctx: &Ctx, _seed: &[u8]) -> Result<Self> {
        let area = Self::new(&ctx.key)?;
        ctx.timer_every("tick", TICK_MS)?;
        // The root hub's key is empty. Without a `world` type (or leave to call it) the area
        // runs without day and night.
        if let Err(e) = ctx.subscribe("world", "", "world") {
            ctx.log(Level::Info, &format!("not watching the world: {e}"));
        }
        Ok(area)
    }

    fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
        decode(snapshot)
    }

    fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
        encode(self)
    }

    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        match call.method {
            "state" => encode(&self.state()?),
            "attack" => {
                let req: Attack = call.decode()?;
                encode(&self.attack(call.player()?, req.mob, ctx.now_ms())?)
            }
            "talk" => {
                call.player()?;
                let req: Talk = call.decode()?;
                if self.model.container(req.npc).is_none_or(|c| c.type_name != "Npc") {
                    return Err(Error::new(format!("no npc {}", req.npc)));
                }
                encode(&self.model.view(&self.schema, req.npc, Viewer::Public)?)
            }
            other => Err(Error::unknown_method(other)),
        }
    }

    fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
        if timer != "tick" {
            return Ok(());
        }
        self.ticks += 1;
        if self.ticks % SCAN_EVERY == 1 {
            self.scan(ctx);
        }
        let dt = TICK_MS as f64 / 1_000.0;
        self.step_mobs(ctx.now_ms(), dt)?;
        self.step_npcs(dt)?;
        if !self.players.is_empty() {
            self.show(ctx)?;
        }
        if self.ticks.is_multiple_of(SCAN_EVERY) {
            self.last_hit.retain(|_, at| ctx.now_ms().saturating_sub(*at) < 60_000);
            self.model.publish_changes(ctx, &self.schema, "area");
        }
        Ok(())
    }

    fn on_topic(&mut self, _ctx: &Ctx, msg: TopicMsg<'_>) -> Result<()> {
        if (msg.node_type, msg.topic) == ("world", "world") {
            let c: Conditions = msg.decode()?;
            self.night = c.night;
            self.aggro_scale = c.modifiers.get("aggro").copied().unwrap_or(1.0).clamp(0.1, 10.0);
        }
        Ok(())
    }
}

ckx_sdk::export_hub!(Area);

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Area {
        Area::new("10,-4").unwrap()
    }

    fn first_mob(a: &Area) -> u64 {
        a.model.of_type("Mob").next().unwrap()
    }

    #[test]
    fn the_key_is_the_centre_chunk_and_the_area_is_populated() {
        let a = area();
        assert_eq!(a.center, (10, -4));
        assert_eq!(a.model.of_type("Mob").count(), MOBS as usize);
        assert_eq!(a.model.of_type("Npc").count(), 2);
        assert_eq!(Area::new("not a key").unwrap().center, (0, 0));
    }

    #[test]
    fn a_mob_chases_a_near_player_and_goes_home_past_its_leash() {
        let mut a = area();
        let m = first_mob(&a);
        let at = a.pos(m).unwrap();
        a.players.insert(7, (at.0 + 1.0, at.1));
        a.step_mobs(0, 1.0).unwrap();
        assert_eq!(a.model.get_str(&a.schema, m, "mode").unwrap(), "chase");
        assert!(dist(a.pos(m).unwrap(), (at.0 + 1.0, at.1)) < 0.6);
        a.players.clear();
        a.place(m, (at.0 + LEASH + 5.0, at.1)).unwrap();
        a.step_mobs(0, 1.0).unwrap();
        assert_eq!(a.model.get_str(&a.schema, m, "mode").unwrap(), "return");
    }

    #[test]
    fn night_and_an_aggro_event_widen_the_range() {
        let mut a = area();
        let m = first_mob(&a);
        let at = a.pos(m).unwrap();
        a.players.insert(7, (at.0 + AGGRO + 0.5, at.1));
        a.step_mobs(0, 0.0).unwrap();
        assert_eq!(a.model.get_str(&a.schema, m, "mode").unwrap(), "idle");
        a.night = true;
        a.step_mobs(0, 0.0).unwrap();
        assert_eq!(a.model.get_str(&a.schema, m, "mode").unwrap(), "chase");
    }

    #[test]
    fn hits_need_reach_respect_the_cooldown_and_a_downed_mob_respawns() {
        let mut a = area();
        let m = first_mob(&a);
        assert!(a.attack(7, m, 0).is_err(), "not in the area");
        let at = a.pos(m).unwrap();
        a.players.insert(7, (at.0 + REACH + 1.0, at.1));
        assert!(a.attack(7, m, 0).is_err(), "out of reach");
        a.players.insert(7, at);
        assert_eq!(a.attack(7, m, 0).unwrap(), 90);
        assert!(a.attack(7, m, 100).is_err(), "cooldown");
        let mut now = 0;
        while a.model.get_i64(&a.schema, m, "hp").unwrap() > 0 {
            now += HIT_COOLDOWN_MS;
            a.attack(7, m, now).unwrap();
        }
        assert!(a.attack(7, m, now + HIT_COOLDOWN_MS).is_err(), "down");
        a.step_mobs(now + RESPAWN_MS, 0.0).unwrap();
        assert_eq!(a.model.get_i64(&a.schema, m, "hp").unwrap(), 100);
        let npc = a.model.of_type("Npc").next().unwrap();
        assert!(a.attack(7, npc, now + RESPAWN_MS).is_err(), "not a mob");
    }

    #[test]
    fn npcs_walk_their_route_and_players_see_only_public_properties() {
        let mut a = area();
        let warden = a.model.of_type("Npc").next().unwrap();
        let route: Vec<(f64, f64)> = a
            .model
            .get(&a.schema, warden, "route")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect();
        let mut reached = vec![false; route.len()];
        // 50 chunks of walking: three times round a 16-chunk loop.
        for _ in 0..200 {
            a.step_npcs(1.0).unwrap();
            let at = a.pos(warden).unwrap();
            for (i, corner) in route.iter().enumerate() {
                reached[i] |= dist(at, *corner) < 0.05;
            }
        }
        assert_eq!(reached, vec![true; 4]);
        let views = a.state().unwrap();
        assert!(views.iter().all(|v| !v.contains_key("route") && !v.contains_key("home_x") && !v.contains_key("respawn_at")));
        assert!(views.iter().any(|v| v.get("name") == Some(&json!("Warden"))));
    }
}
