//! A card table, keyed by its id, with true hidden information: the deck's order and every
//! hand live in the hub's state, and a player reads only their own hand, with a call the hub
//! answers for its caller. The starter's game is crazy eights: match the top card's suit or
//! rank, eights are wild and name a suit, draw when you cannot (once a turn), and the first to
//! empty their hand wins the points left in the others'. Replace [`playable`] and
//! [`card_points`] with your game's rules.
//!
//! Endpoints (players): `join`, `leave`, `start` (the host, from two players), `play { card,
//! suit? }`, `draw`, `pass` (after drawing), `hand` (the caller's own), `state` (anyone). It
//! speaks the kit protocol (`ckx_sdk::kit`): the `table` topic carries the public table as
//! `snapshot` messages and a `changed` message naming the players whose hand changed
//! (`what`: `hand`), who read it with `hand`; `snapshot { topic: "table" }` answers the table;
//! changes may carry an `action_id`; refusals carry `kit:<code>:` codes.

use std::collections::BTreeMap;

use ckx_sdk::kit::play::cards::{CardId, CardTable, standard_52};
use ckx_sdk::kit::{self, Publisher, Receipts, Rng, codes};
use ckx_sdk::prelude::*;

pub const TOPIC: &str = "table";
pub const MIN_PLAYERS: usize = 2;
pub const MAX_PLAYERS: usize = 6;
const HAND: usize = 5;
const TURN_MS: u64 = 30_000;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Lobby,
    Playing,
    Finished,
}

/// A card's rank and suit: `"10H"` is rank `10`, suit `H`.
pub fn split(card: &str) -> (&str, &str) {
    card.split_at(card.len().saturating_sub(1))
}

/// Whether `card` may go on `top` with `suit` the suit in play. The starter's rule: same suit,
/// same rank, or an eight.
pub fn playable(card: &str, top: &str, suit: &str) -> bool {
    let (rank, s) = split(card);
    rank == "8" || s == suit || rank == split(top).0
}

/// What a card left in a hand is worth to the winner: eights 50, faces 10, aces 1.
pub fn card_points(card: &str) -> i64 {
    match split(card).0 {
        "8" => 50,
        "J" | "Q" | "K" | "10" => 10,
        "A" => 1,
        n => n.parse().unwrap_or(0),
    }
}

/// A move, as the table shows it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Played {
    pub player: u64,
    /// The card, or none for a draw or a pass.
    pub card: Option<CardId>,
    pub action: String,
}

/// The table as everyone sees it: never a hidden card.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TableView {
    pub phase: Phase,
    pub players: Vec<u64>,
    pub host: Option<u64>,
    /// Cards in each player's hand, by player id.
    pub hand_counts: BTreeMap<u64, usize>,
    pub top: Option<CardId>,
    /// The suit in play (an eight names one).
    pub suit: Option<String>,
    pub draw_count: usize,
    pub turn: Option<u64>,
    pub turn_ends: Option<u64>,
    pub last: Option<Played>,
    pub winner: Option<u64>,
    pub scores: BTreeMap<u64, i64>,
}

/// What `hand` answers its caller.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HandView {
    pub cards: Vec<CardId>,
    /// The cards that may go on the table now (empty when it is not the caller's turn).
    pub playable: Vec<CardId>,
    pub drawn: bool,
}

#[derive(Deserialize)]
struct Play {
    card: CardId,
    #[serde(default)]
    suit: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Table {
    pub phase: Phase,
    pub players: Vec<u64>,
    pub host: Option<u64>,
    pub cards: CardTable,
    pub suit: String,
    pub turn: usize,
    pub turn_ends: u64,
    /// The current player drew this turn.
    pub drawn: bool,
    pub last: Option<Played>,
    pub winner: Option<u64>,
    pub scores: BTreeMap<u64, i64>,
}

fn id(p: u64) -> String {
    p.to_string()
}

impl Table {
    pub fn current(&self) -> Option<u64> {
        (self.phase == Phase::Playing).then(|| self.players.get(self.turn).copied()).flatten()
    }

    fn top(&self) -> Option<&CardId> {
        self.cards.discard.cards.last()
    }

    pub fn view(&self) -> TableView {
        TableView {
            phase: self.phase,
            players: self.players.clone(),
            host: self.host,
            hand_counts: self.players.iter().map(|p| (*p, self.cards.hand(&id(*p)).len())).collect(),
            top: self.top().cloned(),
            suit: (self.phase != Phase::Lobby).then(|| self.suit.clone()),
            draw_count: self.cards.draw.len(),
            turn: self.current(),
            turn_ends: (self.phase == Phase::Playing).then_some(self.turn_ends),
            last: self.last.clone(),
            winner: self.winner,
            scores: self.scores.clone(),
        }
    }

    pub fn hand(&self, player: u64) -> HandView {
        let cards = self.cards.hand(&id(player)).to_vec();
        let mine = self.current() == Some(player);
        let top = self.top().cloned().unwrap_or_default();
        let playable = if mine { cards.iter().filter(|c| playable(c, &top, &self.suit)).cloned().collect() } else { Vec::new() };
        HandView { cards, playable, drawn: mine && self.drawn }
    }

    pub fn join(&mut self, player: u64) -> Result<()> {
        if self.players.contains(&player) {
            return Ok(());
        }
        if self.phase != Phase::Lobby {
            return Err(kit::error(codes::STARTED, "the game has started"));
        }
        if self.players.len() >= MAX_PLAYERS {
            return Err(kit::error(codes::FULL, "the table is full"));
        }
        self.players.push(player);
        self.host.get_or_insert(player);
        Ok(())
    }

    pub fn leave(&mut self, player: u64, now: u64) -> Vec<u64> {
        let Some(i) = self.players.iter().position(|p| *p == player) else { return Vec::new() };
        if self.phase == Phase::Playing {
            // Their cards go to the bottom of the deck.
            let hand = self.cards.hands.remove(&id(player)).unwrap_or_default();
            for c in hand {
                self.cards.draw.cards.insert(0, c);
            }
        }
        self.players.remove(i);
        if self.host == Some(player) {
            self.host = self.players.first().copied();
        }
        if self.phase == Phase::Playing {
            if self.players.len() < MIN_PLAYERS {
                return self.finish(self.players.first().copied());
            }
            if i < self.turn || self.turn >= self.players.len() {
                self.turn = self.turn.saturating_sub(1) % self.players.len();
            }
            self.turn_ends = now + TURN_MS;
            self.drawn = false;
        }
        Vec::new()
    }

    /// Deals and turns the first card; returns the players whose hand changed.
    pub fn start(&mut self, player: u64, seed: u64, now: u64) -> Result<Vec<u64>> {
        if self.phase != Phase::Lobby {
            return Err(kit::error(codes::STARTED, "the game has started"));
        }
        if self.host != Some(player) {
            return Err(kit::error(codes::FORBIDDEN, "only the host starts the game"));
        }
        if self.players.len() < MIN_PLAYERS {
            return Err(kit::error("too_few_players", format!("at least {MIN_PLAYERS} players are needed")));
        }
        self.cards = CardTable::new(standard_52(), seed);
        let ids: Vec<String> = self.players.iter().map(|p| id(*p)).collect();
        self.cards.deal(&ids, HAND);
        let first = self.cards.draw.draw().ok_or_else(|| kit::error(codes::EMPTY, "the deck is empty"))?;
        self.suit = split(&first).1.to_owned();
        self.cards.discard.put(first);
        self.phase = Phase::Playing;
        self.turn = 0;
        self.turn_ends = now + TURN_MS;
        self.drawn = false;
        self.scores = self.players.iter().map(|p| (*p, 0)).collect();
        Ok(self.players.clone())
    }

    fn mine(&self, player: u64) -> Result<()> {
        if self.phase != Phase::Playing {
            return Err(kit::error(codes::CONFLICT, "the game is not being played"));
        }
        if self.current() != Some(player) {
            return Err(kit::error(codes::NOT_YOUR_TURN, "it is not your turn"));
        }
        Ok(())
    }

    fn next(&mut self, now: u64) {
        self.turn = (self.turn + 1) % self.players.len().max(1);
        self.turn_ends = now + TURN_MS;
        self.drawn = false;
    }

    fn finish(&mut self, winner: Option<u64>) -> Vec<u64> {
        self.phase = Phase::Finished;
        self.winner = winner;
        if let Some(w) = winner {
            let won: i64 =
                self.players.iter().filter(|p| **p != w).flat_map(|p| self.cards.hand(&id(*p)).to_vec()).map(|c| card_points(&c)).sum();
            *self.scores.entry(w).or_insert(0) += won;
        }
        Vec::new()
    }

    /// Plays a card; returns the players whose hand changed.
    pub fn play(&mut self, player: u64, card: &str, suit: Option<&str>, now: u64) -> Result<Vec<u64>> {
        self.mine(player)?;
        if !self.cards.hand(&id(player)).iter().any(|c| c == card) {
            return Err(kit::error(codes::NOT_FOUND, "that card is not in your hand"));
        }
        let top = self.top().cloned().unwrap_or_default();
        if !playable(card, &top, &self.suit) {
            return Err(kit::error("not_playable", format!("{card} does not go on {top}")));
        }
        let (rank, card_suit) = split(card);
        let next_suit = if rank == "8" {
            match suit {
                Some(s @ ("S" | "H" | "D" | "C")) => s.to_owned(),
                _ => return Err(kit::error(codes::BAD_REQUEST, "an eight names the next suit: S, H, D or C")),
            }
        } else {
            card_suit.to_owned()
        };
        self.cards.play(&id(player), card, "table").map_err(|e| kit::error(codes::NOT_FOUND, e))?;
        for (_, c) in self.cards.zones.remove("table").unwrap_or_default() {
            self.cards.discard.put(c);
        }
        self.suit = next_suit;
        self.last = Some(Played { player, card: Some(card.to_owned()), action: "play".into() });
        if self.cards.hand(&id(player)).is_empty() {
            self.finish(Some(player));
            return Ok(vec![player]);
        }
        self.next(now);
        Ok(vec![player])
    }

    /// Draws one card (once a turn); returns the players whose hand changed.
    pub fn draw(&mut self, player: u64) -> Result<Vec<u64>> {
        self.mine(player)?;
        if self.drawn {
            return Err(kit::error(codes::CONFLICT, "you drew this turn: play or pass"));
        }
        // Recycling the discard keeps its top card on the table.
        let top = self.cards.discard.draw();
        let drew = self.cards.draw_to_hand(&id(player));
        if let Some(t) = top {
            self.cards.discard.put(t);
        }
        drew.ok_or_else(|| kit::error(codes::EMPTY, "no cards left to draw"))?;
        self.drawn = true;
        self.last = Some(Played { player, card: None, action: "draw".into() });
        Ok(vec![player])
    }

    pub fn pass(&mut self, player: u64, now: u64) -> Result<()> {
        self.mine(player)?;
        if !self.drawn {
            return Err(kit::error(codes::CONFLICT, "draw before you pass"));
        }
        self.last = Some(Played { player, card: None, action: "pass".into() });
        self.next(now);
        Ok(())
    }

    /// A turn that ran out: the player draws (if they had not) and passes.
    pub fn timeout(&mut self, now: u64) -> Vec<u64> {
        let Some(player) = self.current() else { return Vec::new() };
        if now < self.turn_ends {
            return Vec::new();
        }
        let changed = if self.drawn { Vec::new() } else { self.draw(player).unwrap_or_default() };
        self.last = Some(Played { player, card: None, action: "timeout".into() });
        self.next(now);
        changed
    }
}

/// The hub: the table, and what the kit protocol keeps beside it.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct TableHub {
    #[serde(flatten)]
    pub table: Table,
    #[serde(default)]
    pub kit: Publisher,
    #[serde(default)]
    pub receipts: Receipts,
}

impl TableHub {
    fn act(&mut self, ctx: &Ctx, call: &Call<'_>, now: u64) -> Result<Vec<u8>> {
        let player = call.player()?;
        let t = &mut self.table;
        let (out, changed) = match call.method {
            "join" => (t.join(player).map(|_| encode(&true))?, Vec::new()),
            "leave" => {
                let changed = t.leave(player, now);
                (encode(&true), changed)
            }
            "start" => {
                let seed = Rng::seed_from(&ctx.key) ^ now ^ ctx.epoch;
                let changed = t.start(player, seed, now)?;
                (encode(&true), changed)
            }
            "play" => {
                let p: Play = kit::args(call)?;
                let changed = t.play(player, &p.card, p.suit.as_deref(), now)?;
                (encode(&(t.phase == Phase::Playing)), changed)
            }
            "draw" => {
                let changed = t.draw(player)?;
                (encode(&t.hand(player)), changed)
            }
            "pass" => {
                t.pass(player, now)?;
                (encode(&true), Vec::new())
            }
            other => return Err(Error::unknown_method(other)),
        };
        self.arm(ctx)?;
        self.publish(ctx, &changed)?;
        ctx.persist_now();
        out
    }

    fn arm(&self, ctx: &Ctx) -> Result<()> {
        if self.table.phase == Phase::Playing {
            ctx.timer_after("turn", self.table.turn_ends.saturating_sub(ctx.now_ms()).max(1))
        } else {
            ctx.cancel_timer("turn");
            Ok(())
        }
    }

    fn publish(&mut self, ctx: &Ctx, changed: &[u64]) -> Result<()> {
        self.kit.snapshot(ctx, TOPIC, &self.table.view())?;
        self.kit.changed(ctx, TOPIC, changed, "hand")
    }
}

impl Hub for TableHub {
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
        match call.method {
            "state" => encode(&self.table.view()),
            "hand" => encode(&self.table.hand(call.player()?)),
            kit::SNAPSHOT_METHOD => self.kit.snapshot_reply(ctx, TOPIC, &self.table.view()),
            _ => Receipts::run(self, |h| &mut h.receipts, &call, now, |h| h.act(ctx, &call, now)),
        }
    }

    fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
        if timer != "turn" {
            return Ok(());
        }
        let changed = self.table.timeout(ctx.now_ms());
        self.arm(ctx)?;
        self.publish(ctx, &changed)
    }
}

ckx_sdk::export_hub!(TableHub);

#[cfg(test)]
mod tests {
    use super::*;
    use ckx_sdk::kit::{Changed, Message, MessageKind, error_code};
    use ckx_sdk::testing::TestHub;

    fn started() -> Table {
        let mut t = Table::default();
        t.join(1).unwrap();
        t.join(2).unwrap();
        t.start(1, 7, 0).unwrap();
        t
    }

    #[test]
    fn cards_match_suit_or_rank_and_eights_are_wild() {
        assert!(playable("5H", "9H", "H"));
        assert!(playable("9S", "9H", "H"));
        assert!(playable("8C", "2D", "D"));
        assert!(!playable("5S", "9H", "H"));
        assert_eq!(split("10H"), ("10", "H"));
        assert_eq!(card_points("8S") + card_points("KD") + card_points("3C") + card_points("AH"), 64);
    }

    #[test]
    fn hands_are_dealt_hidden_and_only_the_current_player_moves() {
        let mut t = started();
        assert_eq!(t.view().hand_counts, BTreeMap::from([(1, HAND), (2, HAND)]));
        assert_eq!(t.view().draw_count, 52 - 2 * HAND - 1);
        assert!(t.hand(2).playable.is_empty(), "not their turn");
        let Error(why) = t.draw(2).unwrap_err();
        assert_eq!(error_code(&why).map(|c| c.0), Some(codes::NOT_YOUR_TURN));
        t.draw(1).unwrap();
        assert!(t.draw(1).is_err(), "once a turn");
        t.pass(1, 10).unwrap();
        assert_eq!(t.current(), Some(2));
        assert_eq!(t.hand(1).cards.len(), HAND + 1);
    }

    #[test]
    fn playing_out_a_hand_wins_the_points_left_in_the_others() {
        let mut t = started();
        let mut guard = 0;
        while t.phase == Phase::Playing && guard < 500 {
            guard += 1;
            let p = t.current().unwrap();
            let h = t.hand(p);
            if let Some(card) = h.playable.first() {
                let suit = (split(card).0 == "8").then_some("H");
                t.play(p, card, suit, guard).unwrap();
            } else if !h.drawn && t.draw(p).is_ok() {
            } else {
                t.pass(p, guard).unwrap_or(());
                if t.current() == Some(p) {
                    t.timeout(u64::MAX);
                }
            }
        }
        assert_eq!(t.phase, Phase::Finished);
        let w = t.winner.unwrap();
        assert!(t.hand(w).cards.is_empty());
        assert!(t.scores[&w] > 0);
    }

    #[test]
    fn the_table_topic_is_public_and_a_changed_message_names_who_reads_their_hand() {
        let mut hub = TestHub::<TableHub>::spawn("deck", "t1").unwrap();
        for p in [1, 2] {
            let _: bool = hub.call_typed(Caller::Player(p), "join", &()).unwrap();
        }
        hub.clear();
        let _: bool = hub.call_typed(Caller::Player(1), "start", &()).unwrap();
        #[derive(Deserialize)]
        struct Kind {
            kind: MessageKind,
        }
        let published_now = hub.published();
        let kinds: Vec<MessageKind> = published_now.iter().map(|(_, b)| decode::<Kind>(b).unwrap().kind).collect();
        assert_eq!(kinds, vec![MessageKind::Snapshot, MessageKind::Changed]);
        let changed: Message<Changed> = decode(&published_now[1].1).unwrap();
        let changed = changed.body;
        assert_eq!((changed.who, changed.what.as_str()), (vec![1, 2], "hand"));
        let published = format!("{:?}", hub.published());
        let hand: HandView = hub.call_typed(Caller::Player(2), "hand", &()).unwrap();
        assert_eq!(hand.cards.len(), HAND);
        assert!(hand.cards.iter().all(|c| !published.contains(&format!("\"{c}\""))), "no hidden card is ever published");
        hub.advance(TURN_MS).unwrap();
        let view: TableView = hub.call_typed(Caller::Player(2), "state", &()).unwrap();
        assert_eq!((view.turn, view.hand_counts[&1]), (Some(2), HAND + 1), "a timeout draws and passes");
    }
}
