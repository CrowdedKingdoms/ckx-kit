//! The world's clock, weather and resource nodes, advanced on a timer while the app runs, and
//! the liveops events its developers schedule. It is the root hub, so it hears players enter
//! and leave the app, and other hubs watch its `world` topic: the NPC engine's mobs grow bolder
//! at night and under an event's `aggro` modifier. Replaces the `world-engine` and
//! `liveops-scheduler` templates.
//!
//! Endpoints: `state` (anyone), `harvest { node }` (players), and for the app's developers
//! `schedule { name, start_ms, end_ms, modifiers }` and `cancel { name }`. The spawn seed, if
//! any, is a JSON [`Config`].

use std::collections::BTreeMap;

use ckx_sdk::prelude::*;

pub const TICK_MS: u64 = 1_000;
/// The longest step the clock takes at once, so a world that was stopped resumes where it was.
const MAX_STEP_MS: u64 = 5_000;
const HARVEST_COOLDOWN_MS: u64 = 500;
/// Ticks between `world` pushes when nothing notable changed, so clients keep the clock.
const PUBLISH_EVERY: u64 = 30;
const MAX_EVENTS: usize = 64;
const MAX_MODIFIERS: usize = 16;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Real milliseconds in one game day.
    pub day_ms: u64,
    pub nodes: u32,
    /// What a full resource node holds.
    pub node_amount: u32,
    /// How long an emptied node takes to grow back.
    pub regrow_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self { day_ms: 20 * 60 * 1000, nodes: 12, node_amount: 5, regrow_ms: 60_000 }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Weather {
    #[default]
    Clear,
    Rain,
    Storm,
    Fog,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Node {
    pub id: u32,
    pub amount: u32,
    /// When it grows back, in Unix milliseconds; set while it is empty.
    pub regrow_at: Option<u64>,
}

/// A liveops event: between `start_ms` and `end_ms` (Unix milliseconds) its modifiers apply.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Event {
    pub name: String,
    pub start_ms: u64,
    pub end_ms: u64,
    #[serde(default)]
    pub modifiers: BTreeMap<String, f64>,
}

/// What the `world` topic carries.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Conditions {
    /// 0 at midnight, 0.5 at noon.
    pub time_of_day: f64,
    pub night: bool,
    pub weather: Weather,
    /// The active events' modifiers; where events overlap they multiply.
    pub modifiers: BTreeMap<String, f64>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct View {
    pub conditions: Conditions,
    pub clock_ms: u64,
    pub nodes: Vec<Node>,
    pub events: Vec<Event>,
    pub online: u32,
}

#[derive(Deserialize)]
struct Harvest {
    node: u32,
}

#[derive(Deserialize)]
struct Cancel {
    name: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct World {
    pub config: Config,
    /// Game time since the world began, advanced only while it runs.
    pub clock_ms: u64,
    pub last_tick: u64,
    pub ticks: u64,
    pub weather: Weather,
    /// The game time the weather next changes.
    pub weather_until: u64,
    pub rng: u64,
    pub nodes: Vec<Node>,
    pub events: BTreeMap<String, Event>,
    pub online: u32,
    /// Each player's last harvest, for the cooldown.
    pub harvested: BTreeMap<u64, u64>,
    pub published: Option<Conditions>,
}

impl World {
    pub fn new(config: Config, now: u64) -> Self {
        let nodes = (0..config.nodes).map(|id| Node { id, amount: config.node_amount, regrow_at: None }).collect();
        Self { rng: now | 1, last_tick: now, nodes, config, ..Default::default() }
    }

    fn next_rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Advances the world to `now` (Unix milliseconds).
    pub fn tick(&mut self, now: u64) {
        self.ticks += 1;
        self.clock_ms += now.saturating_sub(self.last_tick).min(MAX_STEP_MS);
        self.last_tick = now;
        if self.clock_ms >= self.weather_until {
            self.weather = match self.next_rand() % 10 {
                0..=5 => Weather::Clear,
                6 | 7 => Weather::Rain,
                8 => Weather::Storm,
                _ => Weather::Fog,
            };
            let hour = (self.config.day_ms / 24).max(1);
            self.weather_until = self.clock_ms + hour * (2 + self.next_rand() % 5);
        }
        let amount = self.config.node_amount;
        for n in &mut self.nodes {
            if n.regrow_at.is_some_and(|at| now >= at) {
                n.amount = amount;
                n.regrow_at = None;
            }
        }
        self.harvested.retain(|_, at| now.saturating_sub(*at) < 60_000);
        self.events.retain(|_, e| e.end_ms > now);
    }

    pub fn conditions(&self, now: u64) -> Conditions {
        let day = self.config.day_ms.max(1);
        let time_of_day = (self.clock_ms % day) as f64 / day as f64;
        let mut modifiers = BTreeMap::new();
        for e in self.events.values().filter(|e| e.start_ms <= now && now < e.end_ms) {
            for (k, v) in &e.modifiers {
                *modifiers.entry(k.clone()).or_insert(1.0) *= v;
            }
        }
        Conditions { time_of_day, night: !(0.25..0.75).contains(&time_of_day), weather: self.weather, modifiers }
    }

    pub fn harvest(&mut self, player: u64, node: u32, now: u64) -> Result<u32> {
        if self.harvested.get(&player).is_some_and(|at| now < at + HARVEST_COOLDOWN_MS) {
            return Err(Error::new("harvesting too fast"));
        }
        let regrow_at = now + self.config.regrow_ms;
        let n = self.nodes.iter_mut().find(|n| n.id == node).ok_or_else(|| Error::new(format!("no resource node {node}")))?;
        if n.amount == 0 {
            return Err(Error::new("that node is empty; it grows back"));
        }
        n.amount -= 1;
        if n.amount == 0 {
            n.regrow_at = Some(regrow_at);
        }
        self.harvested.insert(player, now);
        Ok(n.amount)
    }

    pub fn schedule(&mut self, event: Event, now: u64) -> Result<()> {
        if event.name.is_empty() || event.name.len() > 64 {
            return Err(Error::new("an event name is 1-64 characters"));
        }
        if event.end_ms <= event.start_ms || event.end_ms <= now {
            return Err(Error::new("an event ends after it starts, and in the future"));
        }
        if event.modifiers.len() > MAX_MODIFIERS || event.modifiers.values().any(|v| !v.is_finite() || *v <= 0.0 || *v > 100.0) {
            return Err(Error::new(format!("at most {MAX_MODIFIERS} modifiers, each above 0 and at most 100")));
        }
        if !self.events.contains_key(&event.name) && self.events.len() >= MAX_EVENTS {
            return Err(Error::new(format!("at most {MAX_EVENTS} events are scheduled at once")));
        }
        self.events.insert(event.name.clone(), event);
        Ok(())
    }

    pub fn view(&self, now: u64) -> View {
        View {
            conditions: self.conditions(now),
            clock_ms: self.clock_ms,
            nodes: self.nodes.clone(),
            events: self.events.values().cloned().collect(),
            online: self.online,
        }
    }
}

impl Hub for World {
    fn spawn(ctx: &Ctx, seed: &[u8]) -> Result<Self> {
        let config = if seed.is_empty() {
            Config::default()
        } else {
            serde_json::from_slice(seed).map_err(|e| Error::new(format!("seed is not a JSON config: {e}")))?
        };
        ctx.timer_every("tick", TICK_MS)?;
        Ok(Self::new(config, ctx.now_ms()))
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
            "state" => encode(&self.view(now)),
            "harvest" => {
                let player = call.player()?;
                let req: Harvest = call.decode()?;
                encode(&self.harvest(player, req.node, now)?)
            }
            "schedule" => {
                call.developer()?;
                self.schedule(call.decode()?, now)?;
                ctx.persist_now();
                encode(&true)
            }
            "cancel" => {
                call.developer()?;
                let req: Cancel = call.decode()?;
                encode(&self.events.remove(&req.name).is_some())
            }
            other => Err(Error::unknown_method(other)),
        }
    }

    fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
        if timer != "tick" {
            return Ok(());
        }
        let now = ctx.now_ms();
        self.tick(now);
        let c = self.conditions(now);
        let notable = self.published.as_ref().is_none_or(|p| p.night != c.night || p.weather != c.weather || p.modifiers != c.modifiers);
        if notable || self.ticks.is_multiple_of(PUBLISH_EVERY) {
            ctx.publish("world", &encode(&c)?);
            self.published = Some(c);
        }
        Ok(())
    }

    fn on_presence(&mut self, _ctx: &Ctx, presence: &Presence) -> Result<()> {
        self.online = presence.count;
        Ok(())
    }
}

ckx_sdk::export_hub!(World);

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        World::new(Config { day_ms: 24_000, nodes: 2, node_amount: 2, regrow_ms: 1_000 }, 1_000)
    }

    #[test]
    fn the_clock_steps_at_most_five_seconds_so_a_stopped_world_resumes() {
        let mut w = world();
        w.tick(2_000);
        assert_eq!(w.clock_ms, 1_000);
        w.tick(1_000_000);
        assert_eq!(w.clock_ms, 1_000 + MAX_STEP_MS);
    }

    #[test]
    fn night_is_the_first_and_last_quarter_of_the_day() {
        let mut w = world();
        assert!(w.conditions(0).night);
        w.clock_ms = 12_000;
        assert!(!w.conditions(0).night);
        w.clock_ms = 20_000;
        assert!(w.conditions(0).night);
    }

    #[test]
    fn a_node_empties_cools_down_and_grows_back() {
        let mut w = world();
        assert_eq!(w.harvest(7, 0, 10_000).unwrap(), 1);
        assert!(w.harvest(7, 0, 10_100).is_err(), "cooldown");
        assert_eq!(w.harvest(7, 0, 10_600).unwrap(), 0);
        assert!(w.harvest(8, 0, 10_700).is_err(), "empty");
        assert!(w.harvest(8, 9, 10_700).is_err(), "no such node");
        w.tick(11_700);
        assert_eq!(w.nodes[0].amount, 2);
    }

    #[test]
    fn events_are_bounded_active_in_their_window_and_multiply() {
        let mut w = world();
        let ev = |name: &str, start, end, aggro| Event {
            name: name.into(),
            start_ms: start,
            end_ms: end,
            modifiers: BTreeMap::from([("aggro".to_string(), aggro)]),
        };
        w.schedule(ev("a", 100, 5_000, 2.0), 0).unwrap();
        w.schedule(ev("b", 200, 5_000, 1.5), 0).unwrap();
        assert!(w.schedule(ev("c", 300, 200, 1.0), 0).is_err(), "ends before it starts");
        assert!(w.schedule(ev("d", 300, 400, 0.0), 0).is_err(), "modifier out of range");
        assert_eq!(w.conditions(50).modifiers.get("aggro"), None);
        assert_eq!(w.conditions(250).modifiers.get("aggro"), Some(&3.0));
        w.tick(6_000);
        assert!(w.events.is_empty(), "ended events are dropped");
    }

    #[test]
    fn a_seed_is_a_json_config_with_defaults() {
        let c: Config = serde_json::from_slice(br#"{"nodes": 3}"#).unwrap();
        assert_eq!((c.nodes, c.node_amount), (3, Config::default().node_amount));
    }
}
