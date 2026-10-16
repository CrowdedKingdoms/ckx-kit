//! Deck/hand/zone logic with true hidden information: hands and deck order
//! live in the HUB'S STATE (secrets the hub holds — clients only ever see their
//! own hand through a call the hub answers for its caller), while public zones are published
//! on its topics. Shuffles are seeded (`kit::Rng`)
//! so instance replays are deterministic.

use crate::kit::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub type CardId = String;

/// A pile with hidden order (draw deck / discard).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Pile {
    /// Top of the pile is the END of the vec (pop to draw).
    pub cards: Vec<CardId>,
}

impl Pile {
    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    pub fn draw(&mut self) -> Option<CardId> {
        self.cards.pop()
    }

    pub fn put(&mut self, card: CardId) {
        self.cards.push(card);
    }

    /// Seeded Fisher-Yates shuffle.
    pub fn shuffle(&mut self, rng: &mut Rng) {
        let n = self.cards.len();
        for i in (1..n).rev() {
            let j = (rng.next_u64() % (i as u64 + 1)) as usize;
            self.cards.swap(i, j);
        }
    }
}

/// The full table: draw pile, discard, per-actor hidden hands, and named
/// public zones. Everything serde — one `CardTable` per match partition.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CardTable {
    pub draw: Pile,
    pub discard: Pile,
    pub hands: BTreeMap<String, Vec<CardId>>,
    /// Named public zones (battlefield, trick, melds, ...), in play order.
    pub zones: BTreeMap<String, Vec<(String, CardId)>>,
    pub seed: u64,
}

impl CardTable {
    /// Build a table from a card catalog and shuffle with the given seed.
    pub fn new(catalog: impl IntoIterator<Item = CardId>, seed: u64) -> Self {
        let mut table = Self { draw: Pile { cards: catalog.into_iter().collect() }, seed, ..Default::default() };
        let mut rng = Rng::seeded(seed);
        table.draw.shuffle(&mut rng);
        table
    }

    /// Deal `count` cards to each actor (round-robin, deterministic order).
    pub fn deal(&mut self, actors: &[String], count: usize) {
        for _ in 0..count {
            for actor in actors {
                if let Some(card) = self.draw.draw() {
                    self.hands.entry(actor.clone()).or_default().push(card);
                }
            }
        }
    }

    /// Draw one card into an actor's hidden hand. Recycles the discard
    /// (reshuffled with the table seed + draw count) when the deck empties.
    pub fn draw_to_hand(&mut self, actor_id: &str) -> Option<CardId> {
        if self.draw.is_empty() && !self.discard.is_empty() {
            std::mem::swap(&mut self.draw.cards, &mut self.discard.cards);
            let mut rng = Rng::seeded(self.seed ^ (self.draw.len() as u64) << 8);
            self.draw.shuffle(&mut rng);
        }
        let card = self.draw.draw()?;
        self.hands.entry(actor_id.to_string()).or_default().push(card.clone());
        Some(card)
    }

    /// The caller's own hand (the ONLY read path for hidden cards).
    pub fn hand(&self, actor_id: &str) -> &[CardId] {
        self.hands.get(actor_id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn hand_sizes(&self) -> BTreeMap<String, usize> {
        self.hands.iter().map(|(id, hand)| (id.clone(), hand.len())).collect()
    }

    /// Play (reveal) a card from a hand into a public zone. Fails when the
    /// caller does not hold the card — the legality floor every game gets.
    pub fn play(&mut self, actor_id: &str, card: &str, zone: &str) -> Result<(), &'static str> {
        let hand = self.hands.entry(actor_id.to_string()).or_default();
        let index = hand.iter().position(|c| c == card).ok_or("card not in hand")?;
        let card = hand.remove(index);
        self.zones.entry(zone.to_string()).or_default().push((actor_id.to_string(), card));
        Ok(())
    }

    /// Move every card in a zone to the discard (trick taken, turn over).
    pub fn clear_zone(&mut self, zone: &str) -> Vec<(String, CardId)> {
        let cards = self.zones.remove(zone).unwrap_or_default();
        for (_, card) in &cards {
            self.discard.put(card.clone());
        }
        cards
    }

    /// The public view (zones + counts, NEVER hidden cards) for model sync
    /// and spectator events.
    pub fn public_json(&self) -> Value {
        json!({
            "drawCount": self.draw.len(),
            "discardCount": self.discard.len(),
            "handSizes": self.hand_sizes(),
            "zones": self.zones.iter().map(|(name, cards)| {
                (name.clone(), cards.iter().map(|(actor, card)| json!({
                    "actorId": actor, "card": card,
                })).collect::<Vec<_>>())
            }).collect::<BTreeMap<_, _>>(),
        })
    }
}

/// Build a standard 52-card catalog ("AS", "2S", ..., "KC") — handy for
/// examples and tests; a real game builds its own from its hub's config.
pub fn standard_52() -> Vec<CardId> {
    let ranks = ["A", "2", "3", "4", "5", "6", "7", "8", "9", "10", "J", "Q", "K"];
    let suits = ["S", "H", "D", "C"];
    let mut cards = Vec::with_capacity(52);
    for suit in suits {
        for rank in ranks {
            cards.push(format!("{rank}{suit}"));
        }
    }
    cards
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_shuffles_are_deterministic_and_seed_sensitive() {
        let a = CardTable::new(standard_52(), 42);
        let b = CardTable::new(standard_52(), 42);
        let c = CardTable::new(standard_52(), 43);
        assert_eq!(a.draw.cards, b.draw.cards);
        assert_ne!(a.draw.cards, c.draw.cards);
        // Shuffle is a permutation.
        let mut sorted = a.draw.cards.clone();
        sorted.sort();
        let mut catalog = standard_52();
        catalog.sort();
        assert_eq!(sorted, catalog);
    }

    #[test]
    fn deal_draw_play_lifecycle() {
        let actors = vec!["a".to_string(), "b".to_string()];
        let mut table = CardTable::new(standard_52(), 7);
        table.deal(&actors, 5);
        assert_eq!(table.hand("a").len(), 5);
        assert_eq!(table.hand("b").len(), 5);
        assert_eq!(table.draw.len(), 42);

        let drawn = table.draw_to_hand("a").unwrap();
        assert!(table.hand("a").contains(&drawn));

        let card = table.hand("a")[0].clone();
        table.play("a", &card, "trick").unwrap();
        assert_eq!(table.hand("a").len(), 5);
        assert_eq!(table.zones["trick"].len(), 1);
        assert_eq!(table.play("a", "not-held", "trick").unwrap_err(), "card not in hand");
        // b cannot play a's cards.
        let b_err = table.play("b", &card, "trick");
        assert!(b_err.is_err());

        let taken = table.clear_zone("trick");
        assert_eq!(taken.len(), 1);
        assert_eq!(table.discard.len(), 1);
    }

    #[test]
    fn discard_recycles_into_draw() {
        let mut table = CardTable::new(vec!["x".into(), "y".into()], 1);
        table.draw_to_hand("a");
        table.draw_to_hand("a");
        assert!(table.draw.is_empty());
        assert!(table.draw_to_hand("a").is_none(), "nothing anywhere");
        // Discard a card, then drawing recycles it.
        let card = table.hand("a")[0].clone();
        table.play("a", &card, "z").unwrap();
        table.clear_zone("z");
        let recycled = table.draw_to_hand("b").unwrap();
        assert_eq!(recycled, card);
    }

    #[test]
    fn public_view_hides_hands() {
        let mut table = CardTable::new(standard_52(), 9);
        table.deal(&["a".to_string()], 3);
        let view = table.public_json();
        assert_eq!(view["handSizes"]["a"], 3);
        assert!(view.get("hands").is_none(), "hidden hands never serialize publicly");
        assert_eq!(view["drawCount"], 49);
    }
}
