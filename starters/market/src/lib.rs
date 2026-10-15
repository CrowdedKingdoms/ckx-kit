//! A market, keyed by its name (`town`, `harbor`): vendors with fixed prices whose stock
//! restocks on a schedule, an order book for each item it lists, and trades between two players.
//! It holds nothing itself. What a player puts up waits in a hold in their own `player` hub
//! (`ckx_sdk::kit::econ::holds`), and the market drives each transfer, a settle from the giver's
//! hold then a deposit to the taker, as steps their hubs answer once by reference; it retries a
//! step until its hub answers. It is not a ledger (`ckx_sdk::kit::econ`).
//!
//! Endpoints (players): `vendors`, `buy { vendor, item, count }`, `sell { vendor, item, count }`,
//! `book { item }`, `order { item, side, price, count }`, `cancel { order }`, `orders`,
//! `trade { with }`, `offer { trade, bundle }`, `accept { trade }`, `decline { trade }`,
//! `trades`, `state` (anyone). A change that needs a player's hub and could not hear from it is
//! refused `unavailable`: send it again with the same `action_id`, and it finishes what it began.
//! It speaks the kit protocol (`ckx_sdk::kit`): the `market` topic carries the vendors' stock and
//! each book's best prices as `snapshot` messages, at most once a second, and `changed` messages
//! naming the players whose orders or trades moved (`what`: `orders`, `trades`);
//! `snapshot { topic: "market" }` answers it; refusals carry `kit:<code>:` codes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ckx_sdk::kit::econ::Bundle;
use ckx_sdk::kit::econ::orderbook::{OrderBook, Side};
use ckx_sdk::kit::econ::vendor::{Offer, Shelf, Vendor};
use ckx_sdk::kit::{self, Publisher, Receipts, codes};
use ckx_sdk::prelude::*;
use serde_json::{Value, json};

pub const TOPIC: &str = "market";
const PUBLISH_MS: u64 = 1_000;
/// How often unfinished steps are tried again.
const STEPS_MS: u64 = 1_000;
/// Steps one call drives before it answers; the timer drives the rest.
const STEPS_PER_CALL: usize = 16;
/// A hold whose outcome is unknown, never finished by a resend, is released after this long.
const PENDING_MS: u64 = 60_000;
/// How long a released reference is refused, so a late resend starts a new order.
const RELEASED_MS: u64 = 24 * 3_600_000;
const DEPTH: usize = 10;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub vendors: Vec<Vendor>,
    /// The items with an order book, and the currency they trade in.
    pub book_items: Vec<String>,
    pub book_currency: String,
    pub orders_max: usize,
    /// How long a trade stays open.
    pub trade_ms: u64,
    /// The node type of the players' hubs.
    pub player_type: String,
}

impl Default for Config {
    fn default() -> Self {
        let offer = |item: &str, price: i64, stock: Option<u64>, per_player: Option<u64>, buys_for: Option<i64>| Offer {
            item: item.into(),
            price: Bundle::coins("gold", price),
            stock,
            per_player,
            buys_for: buys_for.map(|p| Bundle::coins("gold", p)),
        };
        Self {
            vendors: vec![Vendor {
                id: "smith".into(),
                offers: vec![offer("sword", 50, Some(5), Some(1), Some(20)), offer("potion", 5, None, None, Some(2))],
                restock: Some("0 * * * *".into()),
            }],
            book_items: vec!["wood".into(), "plank".into(), "ore".into(), "ingot".into()],
            book_currency: "gold".into(),
            orders_max: 20,
            trade_ms: 5 * 60_000,
            player_type: "player".into(),
        }
    }
}

/// An order the market keeps beside its book entry.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct OrderMeta {
    pub id: u64,
    pub player: u64,
    pub item: String,
    pub side: Side,
    pub price: i64,
    pub count: u64,
    /// What is still open.
    pub open: u64,
    /// The hold that backs it, in the player's hub.
    pub hold: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TradeState {
    Open,
    Settling,
    Done,
    Cancelled,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Side_ {
    pub bundle: Bundle,
    pub hold: Option<String>,
    pub accepted: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Trade {
    pub id: u64,
    pub players: [u64; 2],
    pub sides: [Side_; 2],
    pub state: TradeState,
    pub expires_at: u64,
    /// Offers made so far, for the next hold's reference.
    pub offers: u64,
}

impl Trade {
    fn side(&self, player: u64) -> Option<usize> {
        self.players.iter().position(|p| *p == player)
    }
}

/// One step of a transfer, for a player's hub.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    /// Takes `bundle` out of `hold` in `from`'s hub, then gives it to `to`.
    Transfer { from: u64, to: u64, hold: String, bundle: Bundle, r#ref: String, settled: bool, trade: Option<u64> },
    /// Gives what is left of `hold` back to `player`.
    Release { player: u64, hold: String, r#ref: String, trade: Option<u64> },
}

impl Step {
    fn player(&self) -> u64 {
        match self {
            Step::Transfer { from, to, settled, .. } => {
                if *settled {
                    *to
                } else {
                    *from
                }
            }
            Step::Release { player, .. } => *player,
        }
    }

    fn trade(&self) -> Option<u64> {
        match self {
            Step::Transfer { trade, .. } | Step::Release { trade, .. } => *trade,
        }
    }
}

/// Who to tell, and what changed for them.
type Tell = Vec<(u64, &'static str)>;

/// What a player's hub said.
enum Heard {
    Done(Vec<u8>),
    Refused(String),
    /// It may or may not have run: ask again.
    Unknown(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Market {
    pub config: Config,
    pub shelves: BTreeMap<String, Shelf>,
    pub books: BTreeMap<String, OrderBook>,
    pub orders: BTreeMap<u64, OrderMeta>,
    pub trades: BTreeMap<u64, Trade>,
    pub steps: VecDeque<Step>,
    /// Holds asked for whose outcome is unknown: reference to (player, when).
    pub pending: BTreeMap<String, (u64, u64)>,
    /// References released after they were left pending, to when.
    pub released: BTreeMap<String, u64>,
    /// The last trade price of each item.
    pub last: BTreeMap<String, i64>,
    pub next_id: u64,
    /// Steps a hub refused, which nothing will retry; an operator reads `state`.
    pub stuck: u64,
    pub dirty: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct OfferView {
    pub item: String,
    pub price: Bundle,
    /// Left on the shelf; none for no limit.
    pub stock: Option<u64>,
    pub per_player: Option<u64>,
    pub buys_for: Option<Bundle>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VendorView {
    pub id: String,
    pub offers: Vec<OfferView>,
    pub restocked_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    pub bid: Option<i64>,
    pub ask: Option<i64>,
    pub last: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MarketView {
    pub vendors: Vec<VendorView>,
    pub currency: String,
    pub quotes: BTreeMap<String, Quote>,
    pub open_trades: usize,
    pub pending_steps: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BookView {
    pub item: String,
    pub currency: String,
    pub bids: Vec<(i64, u64)>,
    pub asks: Vec<(i64, u64)>,
    pub last: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub order: u64,
    /// `(price, count)` it traded at once.
    pub fills: Vec<(i64, u64)>,
    /// What rests on the book.
    pub open: u64,
}

#[derive(Deserialize)]
struct Shop {
    vendor: String,
    item: String,
    #[serde(default = "one")]
    count: u64,
}

fn one() -> u64 {
    1
}

#[derive(Deserialize)]
struct Item {
    item: String,
}

#[derive(Deserialize)]
struct Place {
    item: String,
    side: Side,
    price: i64,
    count: u64,
}

#[derive(Deserialize)]
struct Cancel {
    order: u64,
}

#[derive(Deserialize)]
struct With {
    with: u64,
}

#[derive(Deserialize)]
struct OfferArgs {
    trade: u64,
    #[serde(default)]
    bundle: Bundle,
}

#[derive(Deserialize)]
struct TradeArgs {
    trade: u64,
}

fn definite(status: Status) -> bool {
    matches!(status, Status::AppError | Status::NotFound | Status::Denied | Status::BadRequest)
}

impl Market {
    pub fn new(config: Config, now: u64) -> Self {
        let mut m = Market { config, dirty: true, ..Market::default() };
        for v in m.config.vendors.clone() {
            m.shelves.entry(v.id.clone()).or_default().restock(&v, now);
        }
        m
    }

    fn vendor(&self, id: &str) -> Result<&Vendor> {
        self.config.vendors.iter().find(|v| v.id == id).ok_or_else(|| kit::error(codes::NOT_FOUND, format!("there is no vendor `{id}`")))
    }

    pub fn view(&self) -> MarketView {
        MarketView {
            vendors: self.vendors(),
            currency: self.config.book_currency.clone(),
            quotes: self
                .config
                .book_items
                .iter()
                .map(|i| {
                    let book = self.books.get(i);
                    (
                        i.clone(),
                        Quote {
                            bid: book.and_then(|b| b.bids.first()).map(|o| o.price),
                            ask: book.and_then(|b| b.asks.first()).map(|o| o.price),
                            last: self.last.get(i).copied(),
                        },
                    )
                })
                .collect(),
            open_trades: self.trades.values().filter(|t| t.state == TradeState::Open).count(),
            pending_steps: self.steps.len(),
        }
    }

    pub fn vendors(&self) -> Vec<VendorView> {
        self.config
            .vendors
            .iter()
            .map(|v| {
                let shelf = self.shelves.get(&v.id);
                VendorView {
                    id: v.id.clone(),
                    offers: v
                        .offers
                        .iter()
                        .map(|o| OfferView {
                            item: o.item.clone(),
                            price: o.price.clone(),
                            stock: o.stock.map(|_| shelf.and_then(|s| s.stock.get(&o.item)).copied().unwrap_or(0)),
                            per_player: o.per_player,
                            buys_for: o.buys_for.clone(),
                        })
                        .collect(),
                    restocked_at: shelf.map(|s| s.restocked_at).unwrap_or(0),
                }
            })
            .collect()
    }

    pub fn book(&self, item: &str) -> BookView {
        let book = self.books.get(item).cloned().unwrap_or_default();
        BookView {
            item: item.to_owned(),
            currency: self.config.book_currency.clone(),
            bids: book.depth(Side::Buy, DEPTH),
            asks: book.depth(Side::Sell, DEPTH),
            last: self.last.get(item).copied(),
        }
    }

    pub fn orders_of(&self, player: u64) -> Vec<OrderMeta> {
        self.orders.values().filter(|o| o.player == player && o.open > 0).cloned().collect()
    }

    pub fn trades_of(&self, player: u64) -> Vec<Trade> {
        self.trades
            .values()
            .filter(|t| t.players.contains(&player) && matches!(t.state, TradeState::Open | TradeState::Settling))
            .cloned()
            .collect()
    }

    /// The reference of a change: from its `action_id`, so a resend gets the same one.
    fn reference(&mut self, ctx: &Ctx, call: &Call<'_>, what: &str, player: u64) -> Result<String> {
        let r = match Receipts::action_id(call.payload)? {
            Some(a) => format!("{}:{what}:{player}:{a}", ctx.key),
            None => {
                self.next_id += 1;
                format!("{}:{what}:{player}:n{}", ctx.key, self.next_id)
            }
        };
        if self.released.contains_key(&r) {
            return Err(kit::error(codes::EXPIRED, "that change was given up; send it again as a new one"));
        }
        Ok(r)
    }

    fn hear(&self, ctx: &Ctx, player: u64, method: &str, args: &Value) -> Heard {
        let payload = match encode(args) {
            Ok(p) => p,
            Err(e) => return Heard::Refused(e.0),
        };
        match ctx.call(&self.config.player_type, &player.to_string(), method, &payload) {
            Ok(reply) => Heard::Done(reply),
            Err(e) if definite(e.status) => Heard::Refused(e.message),
            Err(e) => Heard::Unknown(e.message),
        }
    }

    /// Asks a player's hub for something a client's call needs now: its refusal is the call's,
    /// and an unknown outcome refuses the call `unavailable`, for a resend to finish.
    fn ask(&mut self, ctx: &Ctx, player: u64, method: &str, args: &Value) -> Result<Vec<u8>> {
        match self.hear(ctx, player, method, args) {
            Heard::Done(reply) => Ok(reply),
            Heard::Refused(message) => Err(Error::new(message)),
            Heard::Unknown(message) => {
                Err(kit::error(codes::UNAVAILABLE, format!("the player's hub did not answer ({message}); send it again")))
            }
        }
    }

    /// Holds `bundle` in `player`'s hub under `reference`; an unknown outcome is remembered, so a
    /// hold nobody finishes is given back.
    fn hold(&mut self, ctx: &Ctx, player: u64, reference: &str, bundle: &Bundle, now: u64) -> Result<()> {
        let result = self.ask(ctx, player, "hold", &json!({ "ref": reference, "bundle": bundle }));
        match &result {
            Err(e) if kit::error_code(&e.0).is_some_and(|(c, _)| c == codes::UNAVAILABLE) => {
                self.pending.insert(reference.to_owned(), (player, now));
            }
            _ => {
                self.pending.remove(reference);
            }
        }
        result.map(|_| ())
    }

    /// Drives the steps, each player's in order; a player whose hub did not answer waits.
    pub fn drive(&mut self, ctx: &Ctx, budget: usize) -> Result<()> {
        let mut waiting: BTreeSet<u64> = BTreeSet::new();
        let mut touched: BTreeSet<u64> = BTreeSet::new();
        let mut finished_trades: BTreeSet<u64> = BTreeSet::new();
        let (mut i, mut tries) = (0, 0);
        while i < self.steps.len() && tries < budget {
            let player = self.steps[i].player();
            if waiting.contains(&player) {
                i += 1;
                continue;
            }
            tries += 1;
            let (method, args) = match &self.steps[i] {
                Step::Transfer { hold, bundle, r#ref, settled: false, .. } => {
                    ("settle", json!({ "ref": format!("{ref}:s"), "hold": hold, "bundle": bundle }))
                }
                Step::Transfer { bundle, r#ref, settled: true, .. } => ("deposit", json!({ "ref": format!("{ref}:d"), "bundle": bundle })),
                Step::Release { hold, r#ref, .. } => ("release", json!({ "ref": r#ref, "hold": hold })),
            };
            match self.hear(ctx, player, method, &args) {
                Heard::Unknown(message) => {
                    ctx.log(Level::Warn, &format!("step for player {player} unanswered ({message}); trying again"));
                    waiting.insert(player);
                    i += 1;
                    continue;
                }
                Heard::Refused(message) => {
                    ctx.log(Level::Error, &format!("player {player}'s hub refused {method} ({message}); the transfer is dropped"));
                    self.stuck += 1;
                }
                Heard::Done(_) => {
                    if let Step::Transfer { settled: settled @ false, .. } = &mut self.steps[i] {
                        *settled = true;
                        touched.insert(player);
                        continue;
                    }
                }
            }
            touched.insert(player);
            let step = self.steps.remove(i).expect("the step at i");
            if let Some(t) = step.trade()
                && !self.steps.iter().any(|s| s.trade() == Some(t))
            {
                finished_trades.insert(t);
            }
        }
        for t in finished_trades {
            if let Some(trade) = self.trades.get_mut(&t)
                && trade.state == TradeState::Settling
            {
                trade.state = TradeState::Done;
                touched.extend(trade.players);
            }
        }
        if !touched.is_empty() {
            self.dirty = true;
        }
        Ok(())
    }

    fn act(&mut self, ctx: &Ctx, call: &Call<'_>, now: u64) -> Result<(Vec<u8>, Tell)> {
        let player = call.player()?;
        match call.method {
            "buy" | "sell" => {
                let s: Shop = kit::args(call)?;
                let vendor = self.vendor(&s.vendor)?.clone();
                let offer = vendor.offer(&s.item)?.clone();
                let (pay, get) = if call.method == "buy" {
                    self.shelves.entry(vendor.id.clone()).or_default().check(&offer, player, s.count)?;
                    (offer.price.times(s.count)?, Bundle::item(&offer.item, s.count))
                } else {
                    let price = offer
                        .buys_for
                        .clone()
                        .ok_or_else(|| kit::error(codes::FORBIDDEN, format!("`{}` does not buy `{}`", vendor.id, offer.item)))?;
                    (Bundle::item(&offer.item, s.count), price.times(s.count)?)
                };
                let r = self.reference(ctx, call, &call.method[..1], player)?;
                self.ask(ctx, player, "exchange", &json!({ "ref": r, "pay": pay, "get": get }))?;
                if call.method == "buy" {
                    self.shelves.entry(vendor.id).or_default().sold(&offer, player, s.count);
                }
                self.dirty = true;
                Ok((encode(&get)?, vec![]))
            }
            "order" => {
                let p: Place = kit::args(call)?;
                if !self.config.book_items.contains(&p.item) {
                    return Err(kit::error(codes::NOT_FOUND, format!("`{}` has no book here", p.item)));
                }
                if p.price <= 0 || p.count == 0 {
                    return Err(kit::error(codes::BAD_REQUEST, "a price and a count are positive"));
                }
                if self.orders_of(player).len() >= self.config.orders_max {
                    return Err(kit::error(codes::LIMIT, format!("{} open orders at most", self.config.orders_max)));
                }
                let hold_ref = self.reference(ctx, call, "o", player)?;
                let held = match p.side {
                    Side::Buy => Bundle::coins(
                        &self.config.book_currency,
                        p.price
                            .checked_mul(i64::try_from(p.count).unwrap_or(i64::MAX))
                            .ok_or_else(|| kit::error(codes::LIMIT, "too much"))?,
                    ),
                    Side::Sell => Bundle::item(&p.item, p.count),
                };
                self.hold(ctx, player, &hold_ref, &held, now)?;
                self.next_id += 1;
                let id = self.next_id;
                let fills = self.books.entry(p.item.clone()).or_default().place(id, player, p.side, p.price, p.count, now)?;
                let mut tell = vec![(player, "orders")];
                let filled: u64 = fills.iter().map(|f| f.count).sum();
                self.orders.insert(
                    id,
                    OrderMeta {
                        id,
                        player,
                        item: p.item.clone(),
                        side: p.side,
                        price: p.price,
                        count: p.count,
                        open: p.count,
                        hold: hold_ref,
                    },
                );
                for f in &fills {
                    let (buy, sell) = (self.orders[&f.buy_order].clone(), self.orders[&f.sell_order].clone());
                    let fill = format!("{}:f:{}:{}", ctx.key, f.buy_order, f.sell_order);
                    self.steps.push_back(Step::Transfer {
                        from: f.seller,
                        to: f.buyer,
                        hold: sell.hold.clone(),
                        bundle: Bundle::item(&p.item, f.count),
                        r#ref: format!("{fill}:{}:i", f.count),
                        settled: false,
                        trade: None,
                    });
                    self.steps.push_back(Step::Transfer {
                        from: f.buyer,
                        to: f.seller,
                        hold: buy.hold.clone(),
                        bundle: Bundle::coins(
                            &self.config.book_currency,
                            f.price.saturating_mul(i64::try_from(f.count).unwrap_or(i64::MAX)),
                        ),
                        r#ref: format!("{fill}:{}:c", f.count),
                        settled: false,
                        trade: None,
                    });
                    for o in [f.buy_order, f.sell_order] {
                        let meta = self.orders.get_mut(&o).expect("a filled order");
                        meta.open -= f.count;
                        if meta.open == 0 {
                            let (owner, hold) = (meta.player, meta.hold.clone());
                            self.steps.push_back(Step::Release { player: owner, r#ref: format!("{hold}:r"), hold, trade: None });
                        }
                    }
                    tell.push((f.buyer, "orders"));
                    tell.push((f.seller, "orders"));
                    self.last.insert(p.item.clone(), f.price);
                }
                self.orders.retain(|_, o| o.open > 0);
                self.dirty = true;
                Ok((
                    encode(&Placed { order: id, fills: fills.iter().map(|f| (f.price, f.count)).collect(), open: p.count - filled })?,
                    tell,
                ))
            }
            "cancel" => {
                let Cancel { order } = kit::args(call)?;
                let meta = self.orders.get(&order).filter(|o| o.player == player).cloned();
                let meta = meta.ok_or_else(|| kit::error(codes::NOT_FOUND, format!("you have no open order {order}")))?;
                if let Some(book) = self.books.get_mut(&meta.item) {
                    book.cancel(order, player);
                }
                self.orders.remove(&order);
                self.steps.push_back(Step::Release { player, r#ref: format!("{}:r", meta.hold), hold: meta.hold, trade: None });
                self.dirty = true;
                Ok((encode(&true)?, vec![(player, "orders")]))
            }
            "trade" => {
                let With { with } = kit::args(call)?;
                if with == player {
                    return Err(kit::error(codes::BAD_REQUEST, "a trade is between two players"));
                }
                self.next_id += 1;
                let id = self.next_id;
                let trade = Trade {
                    id,
                    players: [player, with],
                    sides: Default::default(),
                    state: TradeState::Open,
                    expires_at: now + self.config.trade_ms,
                    offers: 0,
                };
                self.trades.insert(id, trade);
                self.dirty = true;
                Ok((encode(&id)?, vec![(player, "trades"), (with, "trades")]))
            }
            "offer" => {
                let o: OfferArgs = kit::args(call)?;
                let (side, others) = self.open_trade(o.trade, player)?;
                let reference = self.reference(ctx, call, &format!("t{}", o.trade), player)?;
                let previous = self.trades[&o.trade].sides[side].hold.clone();
                if previous.as_deref() == Some(reference.as_str()) {
                    return Ok((encode(&true)?, vec![]));
                }
                if !o.bundle.is_empty() {
                    self.hold(ctx, player, &reference, &o.bundle, now)?;
                }
                if let Some(h) = previous {
                    self.steps.push_back(Step::Release { player, r#ref: format!("{h}:r"), hold: h, trade: None });
                }
                let t = self.trades.get_mut(&o.trade).expect("an open trade");
                t.offers += 1;
                t.sides[side] =
                    Side_ { bundle: o.bundle.clone().trimmed(), hold: (!o.bundle.is_empty()).then_some(reference), accepted: false };
                t.sides[1 - side].accepted = false;
                Ok((encode(&true)?, vec![(player, "trades"), (others, "trades")]))
            }
            "accept" => {
                let TradeArgs { trade } = kit::args(call)?;
                let (side, others) = self.open_trade(trade, player)?;
                let t = self.trades.get_mut(&trade).expect("an open trade");
                if t.sides.iter().all(|s| s.bundle.is_empty()) {
                    return Err(kit::error(codes::NOT_READY, "nothing is offered yet"));
                }
                t.sides[side].accepted = true;
                if t.sides.iter().all(|s| s.accepted) {
                    t.state = TradeState::Settling;
                    let t = t.clone();
                    for (i, s) in t.sides.iter().enumerate() {
                        if let Some(hold) = &s.hold {
                            self.steps.push_back(Step::Transfer {
                                from: t.players[i],
                                to: t.players[1 - i],
                                hold: hold.clone(),
                                bundle: s.bundle.clone(),
                                r#ref: format!("{hold}:to"),
                                settled: false,
                                trade: Some(t.id),
                            });
                        }
                    }
                }
                self.dirty = true;
                Ok((encode(&self.trades[&trade].state)?, vec![(player, "trades"), (others, "trades")]))
            }
            "decline" => {
                let TradeArgs { trade } = kit::args(call)?;
                let (_, others) = self.open_trade(trade, player)?;
                self.call_off(trade);
                Ok((encode(&true)?, vec![(player, "trades"), (others, "trades")]))
            }
            other => Err(Error::unknown_method(other)),
        }
    }

    /// The caller's side of open trade `id`, and the other player.
    fn open_trade(&self, id: u64, player: u64) -> Result<(usize, u64)> {
        let t = self.trades.get(&id).ok_or_else(|| kit::error(codes::NOT_FOUND, format!("there is no trade {id}")))?;
        let side = t.side(player).ok_or_else(|| kit::error(codes::FORBIDDEN, "it is not your trade"))?;
        if t.state != TradeState::Open {
            return Err(kit::error(codes::FINISHED, "the trade is settled or called off"));
        }
        Ok((side, t.players[1 - side]))
    }

    fn call_off(&mut self, id: u64) {
        let Some(t) = self.trades.get_mut(&id) else { return };
        t.state = TradeState::Cancelled;
        let t = t.clone();
        for (i, s) in t.sides.iter().enumerate() {
            if let Some(hold) = &s.hold {
                self.steps.push_back(Step::Release {
                    player: t.players[i],
                    r#ref: format!("{hold}:r"),
                    hold: hold.clone(),
                    trade: Some(id),
                });
            }
        }
        self.dirty = true;
    }

    /// Expires trades, gives back holds left pending, and forgets old released references.
    fn sweep(&mut self, now: u64) {
        let expired: Vec<u64> = self.trades.values().filter(|t| t.state == TradeState::Open && now >= t.expires_at).map(|t| t.id).collect();
        for id in expired {
            self.call_off(id);
        }
        let stale: Vec<(String, u64)> =
            self.pending.iter().filter(|(_, (_, at))| now.saturating_sub(*at) >= PENDING_MS).map(|(r, (p, _))| (r.clone(), *p)).collect();
        for (r, player) in stale {
            self.pending.remove(&r);
            self.released.insert(r.clone(), now);
            self.steps.push_back(Step::Release { player, r#ref: format!("{r}:r"), hold: r, trade: None });
        }
        self.released.retain(|_, at| now.saturating_sub(*at) < RELEASED_MS);
        self.trades.retain(|_, t| matches!(t.state, TradeState::Open | TradeState::Settling) || now < t.expires_at + self.config.trade_ms);
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct MarketHub {
    #[serde(flatten)]
    pub market: Market,
    #[serde(default)]
    pub kit: Publisher,
    #[serde(default)]
    pub receipts: Receipts,
}

impl MarketHub {
    fn arm(&self, ctx: &Ctx) -> Result<()> {
        for v in &self.market.config.vendors {
            if let Some(expr) = &v.restock {
                ctx.timer_cron(&format!("restock:{}", v.id), &Cron::parse(expr).map_err(|e| kit::error(codes::BAD_REQUEST, e))?)?;
            }
        }
        Ok(())
    }

    fn tell(&mut self, ctx: &Ctx, tell: &[(u64, &'static str)]) -> Result<()> {
        let mut by_what: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        for (p, what) in tell {
            let who = by_what.entry(what).or_default();
            if !who.contains(p) {
                who.push(*p);
            }
        }
        for (what, who) in by_what {
            self.kit.changed(ctx, TOPIC, &who, what)?;
        }
        Ok(())
    }
}

impl Hub for MarketHub {
    fn spawn(ctx: &Ctx, seed: &[u8]) -> Result<Self> {
        let config = if seed.is_empty() {
            Config::default()
        } else {
            serde_json::from_slice(seed).map_err(|e| Error::new(format!("seed is not a JSON config: {e}")))?
        };
        let hub = MarketHub { market: Market::new(config, ctx.now_ms()), ..MarketHub::default() };
        hub.arm(ctx)?;
        ctx.timer_every("publish", PUBLISH_MS)?;
        ctx.timer_every("steps", STEPS_MS)?;
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
        let m = &self.market;
        match call.method {
            "state" => return encode(&m.view()),
            "vendors" => return encode(&m.vendors()),
            "book" => {
                let Item { item } = kit::args(&call)?;
                return encode(&m.book(&item));
            }
            "orders" => return encode(&m.orders_of(call.player()?)),
            "trades" => return encode(&m.trades_of(call.player()?)),
            kit::SNAPSHOT_METHOD => return self.kit.snapshot_reply(ctx, TOPIC, &m.view()),
            _ => {}
        }
        let mut tell = Vec::new();
        let reply = Receipts::run(
            self,
            |h| &mut h.receipts,
            &call,
            now,
            |h| {
                let (reply, t) = h.market.act(ctx, &call, now)?;
                tell = t;
                Ok(reply)
            },
        )?;
        self.market.drive(ctx, STEPS_PER_CALL)?;
        self.tell(ctx, &tell)?;
        Ok(reply)
    }

    fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
        let now = ctx.now_ms();
        if let Some(id) = timer.strip_prefix("restock:") {
            if let Some(v) = self.market.config.vendors.iter().find(|v| v.id == id).cloned() {
                self.market.shelves.entry(v.id.clone()).or_default().restock(&v, now);
                self.market.dirty = true;
            }
            return Ok(());
        }
        match timer {
            "steps" => {
                self.market.sweep(now);
                if !self.market.steps.is_empty() {
                    self.market.drive(ctx, STEPS_PER_CALL * 4)?;
                }
            }
            "publish" if self.market.dirty => {
                self.market.dirty = false;
                self.kit.snapshot(ctx, TOPIC, &self.market.view())?;
            }
            _ => {}
        }
        Ok(())
    }
}

ckx_sdk::export_hub!(MarketHub);

#[cfg(test)]
mod tests {
    use super::*;
    use ckx_sdk::CallError;
    use ckx_sdk::kit::econ::items::{Catalog, ItemDef};
    use ckx_sdk::kit::econ::{Applied, Holdings, Holds};
    use ckx_sdk::kit::error_code;
    use ckx_sdk::testing::{Message as Sent, TestHub};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The players' hubs as the market sees them: holdings, holds and the references answered.
    #[derive(Default)]
    struct Players {
        catalog: Catalog,
        of: BTreeMap<u64, (Holdings, Holds)>,
        applied: Applied,
        /// Players whose hub does not answer.
        down: BTreeSet<u64>,
    }

    impl Players {
        fn new(players: &[(u64, Bundle)]) -> Rc<RefCell<Self>> {
            let catalog = Catalog::from(
                ["wood", "plank", "ore", "ingot", "potion"]
                    .iter()
                    .map(|i| ItemDef { id: (*i).into(), max_stack: 99, tradable: true, name: String::new() })
                    .chain(std::iter::once(ItemDef { id: "sword".into(), max_stack: 1, tradable: true, name: String::new() }))
                    .collect::<Vec<_>>(),
            );
            let of = players
                .iter()
                .map(|(p, b)| {
                    let mut h = Holdings::new(20);
                    h.give(&catalog, b).unwrap();
                    (*p, (h, Holds::default()))
                })
                .collect();
            Rc::new(RefCell::new(Players { catalog, of, ..Players::default() }))
        }

        fn answer(&mut self, m: &Sent) -> std::result::Result<Vec<u8>, CallError> {
            let player: u64 = m.key.parse().unwrap();
            if self.down.contains(&player) {
                return Err(CallError { status: Status::DeadlineExceeded, message: "no reply".into() });
            }
            let args: Value = decode(&m.payload).unwrap();
            let reference = args["ref"].as_str().unwrap().to_owned();
            if let Some(replayed) = self.applied.replay(&reference, 0) {
                return replayed.map_err(|e| CallError { status: Status::AppError, message: e.0 });
            }
            let catalog = self.catalog.clone();
            let (holdings, holds) = self.of.get_mut(&player).unwrap();
            let bundle = |k: &str| -> Bundle { serde_json::from_value(args[k].clone()).unwrap_or_default() };
            let result: Result<Vec<u8>> = (|| match m.method.as_str() {
                "hold" => holds.hold(holdings, &reference, &bundle("bundle")).and_then(|_| encode(&true)),
                "settle" => holds.settle(args["hold"].as_str().unwrap(), &bundle("bundle")).and_then(|_| encode(&true)),
                "release" => encode(&holds.release(holdings, &catalog, args["hold"].as_str().unwrap())?),
                "deposit" => holdings.give_or_mail(&catalog, &bundle("bundle")).and_then(|_| encode(&true)),
                "exchange" => holdings.swap(&catalog, &bundle("pay"), &bundle("get")).and_then(|_| encode(&true)),
                other => panic!("the market asked for {other}"),
            })();
            self.applied.record(&reference, &result, 0);
            result.map_err(|e| CallError { status: Status::AppError, message: e.0 })
        }

        fn gold(&self, p: u64) -> i64 {
            self.of[&p].0.wallet.balance("gold")
        }

        fn count(&self, p: u64, item: &str) -> u64 {
            self.of[&p].0.inventory.count(item)
        }
    }

    fn market(players: &Rc<RefCell<Players>>) -> TestHub<MarketHub> {
        let hub = TestHub::<MarketHub>::spawn("market", "town").unwrap();
        let p = players.clone();
        hub.answer_calls("player", move |m| p.borrow_mut().answer(m));
        hub
    }

    fn code(e: &CallError) -> Option<&str> {
        error_code(&e.message).map(|c| c.0)
    }

    #[test]
    fn a_vendor_sells_from_its_stock_once_a_player_and_buys_back() {
        let players = Players::new(&[(1, Bundle::coins("gold", 120)), (2, Bundle::coins("gold", 10))]);
        let mut m = market(&players);
        let got: Bundle =
            m.call_typed(Caller::Player(1), "buy", &json!({ "vendor": "smith", "item": "sword", "action_id": "b1" })).unwrap();
        assert_eq!(got, Bundle::item("sword", 1));
        let _: Bundle = m.call_typed(Caller::Player(1), "buy", &json!({ "vendor": "smith", "item": "sword", "action_id": "b1" })).unwrap();
        assert_eq!(players.borrow().gold(1), 70, "a resent buy is answered, not bought again");
        let e = m.call(Caller::Player(1), "buy", &encode(&json!({ "vendor": "smith", "item": "sword" })).unwrap()).unwrap_err();
        assert_eq!(code(&e), Some(codes::LIMIT), "one a player until the restock");
        let e = m.call(Caller::Player(2), "buy", &encode(&json!({ "vendor": "smith", "item": "sword" })).unwrap()).unwrap_err();
        assert_eq!(code(&e), Some(codes::INSUFFICIENT), "player 2 has 10 gold");
        let paid: Bundle = m.call_typed(Caller::Player(1), "sell", &json!({ "vendor": "smith", "item": "sword" })).unwrap();
        assert_eq!(paid, Bundle::coins("gold", 20));
        let v: Vec<VendorView> = m.call_typed(Caller::Player(2), "vendors", &()).unwrap();
        assert_eq!(v[0].offers[0].stock, Some(4));
        m.advance(3_600_000).unwrap();
        let v: Vec<VendorView> = m.call_typed(Caller::Player(2), "vendors", &()).unwrap();
        assert_eq!(v[0].offers[0].stock, Some(5), "restocked on the hour");
    }

    #[test]
    fn orders_hold_what_they_offer_and_a_fill_moves_both_sides_once() {
        let players = Players::new(&[(1, Bundle::coins("gold", 100)), (2, Bundle::item("ore", 10))]);
        let mut m = market(&players);
        let sell: Placed = m
            .call_typed(Caller::Player(2), "order", &json!({ "item": "ore", "side": "sell", "price": 4, "count": 6, "action_id": "s1" }))
            .unwrap();
        assert_eq!((sell.open, players.borrow().count(2, "ore")), (6, 4), "the ore is held");
        let buy: Placed = m
            .call_typed(Caller::Player(1), "order", &json!({ "item": "ore", "side": "buy", "price": 5, "count": 4, "action_id": "o1" }))
            .unwrap();
        assert_eq!((buy.fills.clone(), buy.open), (vec![(4, 4)], 0), "at the resting price");
        let p = players.borrow();
        assert_eq!((p.count(1, "ore"), p.gold(1)), (4, 84), "16 paid; the 4 held above it came back");
        assert_eq!(p.gold(2), 16);
        drop(p);
        let book: BookView = m.call_typed(Caller::Player(1), "book", &json!({ "item": "ore" })).unwrap();
        assert_eq!((book.asks.clone(), book.last), (vec![(4, 2)], Some(4)));
        let mine: Vec<OrderMeta> = m.call_typed(Caller::Player(2), "orders", &()).unwrap();
        let _: bool = m.call_typed(Caller::Player(2), "cancel", &json!({ "order": mine[0].id })).unwrap();
        assert_eq!(players.borrow().count(2, "ore"), 6, "the rest comes back");
        let e = m
            .call(Caller::Player(1), "order", &encode(&json!({ "item": "ore", "side": "buy", "price": 5, "count": 100 })).unwrap())
            .unwrap_err();
        assert_eq!(code(&e), Some(codes::INSUFFICIENT), "a buy holds its price");
    }

    #[test]
    fn a_player_whose_hub_does_not_answer_gets_unavailable_and_a_resend_finishes() {
        let players = Players::new(&[(1, Bundle::coins("gold", 100)), (2, Bundle::item("ore", 10))]);
        let mut m = market(&players);
        let _: Placed = m
            .call_typed(Caller::Player(2), "order", &json!({ "item": "ore", "side": "sell", "price": 4, "count": 5, "action_id": "s1" }))
            .unwrap();
        players.borrow_mut().down.insert(2);
        let fill: Placed = m
            .call_typed(Caller::Player(1), "order", &json!({ "item": "ore", "side": "buy", "price": 4, "count": 5, "action_id": "o1" }))
            .unwrap();
        assert_eq!(fill.open, 0);
        assert_eq!(players.borrow().count(1, "ore"), 0, "the seller's hub must give the ore up first");
        players.borrow_mut().down.remove(&2);
        m.advance(STEPS_MS).unwrap();
        assert_eq!((players.borrow().count(1, "ore"), players.borrow().gold(2)), (5, 20), "the steps finish once it answers");
        players.borrow_mut().down.insert(1);
        let e = m
            .call(Caller::Player(1), "buy", &encode(&json!({ "vendor": "smith", "item": "potion", "action_id": "b9" })).unwrap())
            .unwrap_err();
        assert_eq!(code(&e), Some(codes::UNAVAILABLE));
        players.borrow_mut().down.remove(&1);
        let got: Bundle =
            m.call_typed(Caller::Player(1), "buy", &json!({ "vendor": "smith", "item": "potion", "action_id": "b9" })).unwrap();
        assert_eq!(got, Bundle::item("potion", 1), "the resend runs: unavailable was not a receipt");
    }

    #[test]
    fn a_trade_settles_when_both_accept_the_last_offers_and_a_declined_one_gives_back() {
        let players = Players::new(&[(1, Bundle::coins("gold", 50)), (2, Bundle::item("plank", 5))]);
        let mut m = market(&players);
        let t: u64 = m.call_typed(Caller::Player(1), "trade", &json!({ "with": 2 })).unwrap();
        let _: bool = m
            .call_typed(Caller::Player(1), "offer", &json!({ "trade": t, "bundle": { "currency": { "gold": 30 } }, "action_id": "a" }))
            .unwrap();
        let _: bool = m
            .call_typed(Caller::Player(2), "offer", &json!({ "trade": t, "bundle": { "items": { "plank": 5 } }, "action_id": "b" }))
            .unwrap();
        let _: TradeState = m.call_typed(Caller::Player(1), "accept", &json!({ "trade": t })).unwrap();
        let _: bool = m
            .call_typed(Caller::Player(1), "offer", &json!({ "trade": t, "bundle": { "currency": { "gold": 20 } }, "action_id": "a2" }))
            .unwrap();
        let state: TradeState = m.call_typed(Caller::Player(2), "accept", &json!({ "trade": t })).unwrap();
        assert_eq!(state, TradeState::Open, "an offer changed: player 1 accepts again");
        let state: TradeState = m.call_typed(Caller::Player(1), "accept", &json!({ "trade": t })).unwrap();
        assert_eq!(state, TradeState::Settling);
        let p = players.borrow();
        assert_eq!((p.gold(1), p.count(1, "plank"), p.gold(2), p.count(2, "plank")), (30, 5, 20, 0));
        drop(p);
        let trades: Vec<Trade> = m.call_typed(Caller::Player(1), "trades", &()).unwrap();
        assert!(trades.is_empty(), "settled");
        let t2: u64 = m.call_typed(Caller::Player(2), "trade", &json!({ "with": 1 })).unwrap();
        let _: bool = m
            .call_typed(Caller::Player(1), "offer", &json!({ "trade": t2, "bundle": { "currency": { "gold": 10 } }, "action_id": "c" }))
            .unwrap();
        assert_eq!(players.borrow().gold(1), 20);
        let e = m.call(Caller::Player(3), "decline", &encode(&json!({ "trade": t2 })).unwrap()).unwrap_err();
        assert_eq!(code(&e), Some(codes::FORBIDDEN));
        let _: bool = m.call_typed(Caller::Player(2), "decline", &json!({ "trade": t2 })).unwrap();
        assert_eq!(players.borrow().gold(1), 30, "the hold comes back");
    }

    #[test]
    fn a_hold_left_pending_is_given_back_and_its_reference_refused() {
        let players = Players::new(&[(1, Bundle::coins("gold", 100))]);
        let mut m = market(&players);
        players.borrow_mut().down.insert(1);
        let e = m
            .call(
                Caller::Player(1),
                "order",
                &encode(&json!({ "item": "ore", "side": "buy", "price": 5, "count": 2, "action_id": "p1" })).unwrap(),
            )
            .unwrap_err();
        assert_eq!(code(&e), Some(codes::UNAVAILABLE));
        players.borrow_mut().down.remove(&1);
        m.advance(PENDING_MS + STEPS_MS).unwrap();
        assert_eq!(players.borrow().gold(1), 100);
        let e = m
            .call(
                Caller::Player(1),
                "order",
                &encode(&json!({ "item": "ore", "side": "buy", "price": 5, "count": 2, "action_id": "p1" })).unwrap(),
            )
            .unwrap_err();
        assert_eq!(code(&e), Some(codes::EXPIRED), "a late resend is a new order");
    }
}
