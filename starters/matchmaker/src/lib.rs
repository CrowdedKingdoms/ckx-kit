//! A matchmaking queue, keyed by its name (`ranked`, `casual`, ...). Players join with a
//! rating; the window of ratings they accept widens the longer they wait; a match is proposed
//! when enough players fall in each other's windows, and every one of them must accept in
//! time. When all do, the queue reserves a `session` hub for them (the manifest lets this type
//! call `session`) and tells each player its key. Replaces the `matchmaking` template.
//!
//! Endpoints (players): `join { rating }`, `leave`, `accept`, `decline`, `status` (your own
//! place: queued, proposed or matched). `configure { .. }` (developers). The `queue` topic
//! carries how many are waiting, never who.

use std::collections::{BTreeMap, BTreeSet};

use ckx_sdk::prelude::*;

pub const TICK_MS: u64 = 1_000;
/// How long a player's match stays in `status` after it is made.
const MATCHED_KEEP_MS: u64 = 120_000;
const MAX_QUEUE: usize = 10_000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub players_per_match: usize,
    /// The rating difference a player accepts at first, and how it widens each second.
    pub window: u32,
    pub widen_per_sec: u32,
    pub max_window: u32,
    pub accept_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self { players_per_match: 2, window: 100, widen_per_sec: 10, max_window: 500, accept_ms: 15_000 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub player: u64,
    pub rating: u32,
    pub since: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Proposal {
    pub id: u64,
    pub entries: Vec<Entry>,
    pub accepted: BTreeSet<u64>,
    pub expires: u64,
}

impl Proposal {
    fn has(&self, player: u64) -> bool {
        self.entries.iter().any(|e| e.player == player)
    }

    pub fn players(&self) -> Vec<u64> {
        self.entries.iter().map(|e| e.player).collect()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Matched {
    pub session: String,
    pub at: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Place {
    Idle,
    Queued { rating: u32, waited_ms: u64, window: u32 },
    Proposed { players: Vec<u64>, accepted: Vec<u64>, expires_in_ms: u64 },
    Matched { session: String },
}

#[derive(Deserialize)]
struct Join {
    rating: u32,
}

#[derive(Serialize)]
struct Reserve<'a> {
    players: &'a [u64],
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Matchmaker {
    pub config: Config,
    pub queue: Vec<Entry>,
    pub proposals: BTreeMap<u64, Proposal>,
    pub matched: BTreeMap<u64, Matched>,
    pub next_id: u64,
}

impl Matchmaker {
    fn window(&self, e: &Entry, now: u64) -> u32 {
        let waited = (now.saturating_sub(e.since) / 1_000) as u32;
        self.config.window.saturating_add(self.config.widen_per_sec.saturating_mul(waited)).min(self.config.max_window)
    }

    fn proposal_of(&self, player: u64) -> Option<u64> {
        self.proposals.values().find(|p| p.has(player)).map(|p| p.id)
    }

    pub fn join(&mut self, player: u64, rating: u32, now: u64) -> Result<()> {
        if self.proposal_of(player).is_some() {
            return Err(Error::new("you have a match to accept"));
        }
        self.matched.remove(&player);
        if let Some(e) = self.queue.iter_mut().find(|e| e.player == player) {
            e.rating = rating;
            return Ok(());
        }
        if self.queue.len() >= MAX_QUEUE {
            return Err(Error::new("the queue is full"));
        }
        self.queue.push(Entry { player, rating, since: now });
        Ok(())
    }

    pub fn leave(&mut self, player: u64) {
        self.queue.retain(|e| e.player != player);
        if let Some(id) = self.proposal_of(player) {
            self.decline(player, id);
        }
    }

    /// Proposes matches from the queue, oldest entries first.
    pub fn propose(&mut self, now: u64) -> Vec<u64> {
        let n = self.config.players_per_match.max(1);
        let mut waiting = std::mem::take(&mut self.queue);
        waiting.sort_by_key(|e| e.since);
        let mut made = Vec::new();
        let mut i = 0;
        while i < waiting.len() {
            let a = waiting[i].clone();
            let picked: Vec<usize> = std::iter::once(i)
                .chain(
                    (i + 1..waiting.len())
                        .filter(|&j| {
                            let b = &waiting[j];
                            a.rating.abs_diff(b.rating) <= self.window(&a, now).min(self.window(b, now))
                        })
                        .take(n - 1),
                )
                .collect();
            if picked.len() < n {
                i += 1;
                continue;
            }
            // Highest index first, so the lower ones stay valid; then back into queue order.
            let mut entries: Vec<Entry> = picked.iter().rev().map(|&j| waiting.remove(j)).collect();
            entries.reverse();
            self.next_id += 1;
            let id = self.next_id;
            self.proposals.insert(id, Proposal { id, entries, accepted: BTreeSet::new(), expires: now + self.config.accept_ms });
            made.push(id);
        }
        self.queue = waiting;
        made
    }

    /// Returns the proposal when this acceptance completes it.
    pub fn accept(&mut self, player: u64) -> Result<Option<Proposal>> {
        let id = self.proposal_of(player).ok_or_else(|| Error::new("you have no match to accept"))?;
        let p = self.proposals.get_mut(&id).expect("found above");
        p.accepted.insert(player);
        if p.accepted.len() < p.entries.len() {
            return Ok(None);
        }
        Ok(self.proposals.remove(&id))
    }

    /// The decliner leaves the queue; the others go back, keeping their place.
    pub fn decline(&mut self, player: u64, id: u64) {
        if let Some(p) = self.proposals.remove(&id) {
            self.queue.extend(p.entries.into_iter().filter(|e| e.player != player));
        }
    }

    /// Proposals past their time: those who accepted go back, the others leave.
    pub fn expire(&mut self, now: u64) {
        let late: Vec<u64> = self.proposals.values().filter(|p| p.expires <= now).map(|p| p.id).collect();
        for id in late {
            if let Some(p) = self.proposals.remove(&id) {
                self.queue.extend(p.entries.into_iter().filter(|e| p.accepted.contains(&e.player)));
            }
        }
        self.matched.retain(|_, m| now.saturating_sub(m.at) < MATCHED_KEEP_MS);
    }

    pub fn status(&self, player: u64, now: u64) -> Place {
        if let Some(e) = self.queue.iter().find(|e| e.player == player) {
            return Place::Queued { rating: e.rating, waited_ms: now.saturating_sub(e.since), window: self.window(e, now) };
        }
        if let Some(p) = self.proposal_of(player).and_then(|id| self.proposals.get(&id)) {
            return Place::Proposed {
                players: p.players(),
                accepted: p.accepted.iter().copied().collect(),
                expires_in_ms: p.expires.saturating_sub(now),
            };
        }
        match self.matched.get(&player) {
            Some(m) => Place::Matched { session: m.session.clone() },
            None => Place::Idle,
        }
    }

    fn busy(&self) -> bool {
        !self.queue.is_empty() || !self.proposals.is_empty()
    }
}

impl Hub for Matchmaker {
    fn spawn(_ctx: &Ctx, _seed: &[u8]) -> Result<Self> {
        Ok(Self::default())
    }

    fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
        decode(snapshot)
    }

    fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
        encode(self)
    }

    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        let now = ctx.now_ms();
        let out = match call.method {
            "status" => return encode(&self.status(call.player()?, now)),
            "join" => {
                let req: Join = call.decode()?;
                self.join(call.player()?, req.rating, now)?;
                encode(&self.status(call.player()?, now))
            }
            "leave" => {
                self.leave(call.player()?);
                encode(&true)
            }
            "accept" => {
                let player = call.player()?;
                if let Some(p) = self.accept(player)? {
                    // The epoch rises on every placement, so a queue restarted without its
                    // snapshot never hands out a key an earlier match used.
                    let session = format!("{}-{}-{}", ctx.key, ctx.epoch, p.id);
                    let players = p.players();
                    match ctx.call_typed::<_, bool>("session", &session, "reserve", &Reserve { players: &players }) {
                        Ok(_) => {
                            for player in players {
                                self.matched.insert(player, Matched { session: session.clone(), at: now });
                            }
                        }
                        Err(e) => {
                            ctx.log(Level::Warn, &format!("reserving session {session}: {e}"));
                            self.queue.extend(p.entries);
                        }
                    }
                }
                encode(&self.status(player, now))
            }
            "decline" => {
                let player = call.player()?;
                if let Some(id) = self.proposal_of(player) {
                    self.decline(player, id);
                }
                encode(&true)
            }
            "configure" => {
                call.developer()?;
                let config: Config = call.decode()?;
                if !(1..=64).contains(&config.players_per_match) || config.accept_ms < 1_000 {
                    return Err(Error::new("1 <= players_per_match <= 64, and accept_ms is at least 1000"));
                }
                self.config = config;
                encode(&true)
            }
            other => return Err(Error::unknown_method(other)),
        };
        if self.busy() && !ctx.timers().iter().any(|(name, _)| name == "match") {
            ctx.timer_every("match", TICK_MS)?;
        }
        out
    }

    fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
        if timer != "match" {
            return Ok(());
        }
        let now = ctx.now_ms();
        self.expire(now);
        self.propose(now);
        ctx.publish("queue", &encode(&(self.queue.len(), self.proposals.len()))?);
        if !self.busy() {
            ctx.cancel_timer("match");
        }
        Ok(())
    }
}

ckx_sdk::export_hub!(Matchmaker);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_ratings_match_at_once_and_far_ones_after_the_window_widens() {
        let mut m = Matchmaker::default();
        m.join(1, 1000, 0).unwrap();
        m.join(2, 1050, 0).unwrap();
        m.join(3, 1400, 0).unwrap();
        let first = m.propose(0);
        assert_eq!(first.len(), 1);
        assert_eq!(m.proposals[&first[0]].players(), vec![1, 2]);
        m.join(4, 1700, 0).unwrap();
        assert!(m.propose(10_000).is_empty(), "300 apart, windows are 200");
        assert_eq!(m.propose(20_000).len(), 1, "windows reach 300 after 20 s");
    }

    #[test]
    fn everyone_must_accept_and_a_decline_requeues_the_others() {
        let mut m = Matchmaker::default();
        m.join(1, 1000, 0).unwrap();
        m.join(2, 1000, 5).unwrap();
        m.propose(10);
        assert!(m.accept(1).unwrap().is_none());
        assert!(matches!(m.status(2, 10), Place::Proposed { .. }));
        let id = m.proposal_of(2).unwrap();
        m.decline(2, id);
        assert_eq!(m.status(1, 10), Place::Queued { rating: 1000, waited_ms: 10, window: 100 }, "keeps its place");
        assert_eq!(m.status(2, 10), Place::Idle);
        m.join(2, 1000, 20).unwrap();
        m.propose(20);
        m.accept(1).unwrap();
        let done = m.accept(2).unwrap().expect("complete");
        assert_eq!(done.players(), vec![1, 2]);
    }

    #[test]
    fn an_expired_proposal_requeues_only_who_accepted() {
        let mut m = Matchmaker::default();
        m.join(1, 1000, 0).unwrap();
        m.join(2, 1000, 0).unwrap();
        m.propose(0);
        m.accept(1).unwrap();
        m.expire(m.config.accept_ms);
        assert!(m.proposals.is_empty());
        assert_eq!(m.queue.iter().map(|e| e.player).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn a_player_with_a_match_to_accept_cannot_rejoin() {
        let mut m = Matchmaker::default();
        m.join(1, 1000, 0).unwrap();
        m.join(2, 1000, 0).unwrap();
        m.propose(0);
        assert!(m.join(1, 900, 1).is_err());
        m.leave(1);
        assert_eq!(m.queue.iter().map(|e| e.player).collect::<Vec<_>>(), vec![2]);
    }
}
