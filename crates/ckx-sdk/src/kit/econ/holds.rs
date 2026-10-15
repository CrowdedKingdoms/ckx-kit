//! Escrow in the owner's own hub, and the references a hub answers once.
//!
//! A transfer between two players' hubs never leaves goods in neither or both: whatever drives
//! it (a market) first [`Holds::hold`]s them in the giver's hub under a reference, later
//! [`Holds::settle`]s the hold (the goods leave for good) and delivers them to the taker, or
//! [`Holds::release`]s it back. Every step another hub asks for carries a reference of its own,
//! which [`Applied`] answers once, so the driver retries a step until it gets an answer.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use super::check_id;
use super::holdings::{Bundle, Holdings};
use super::items::Catalog;
use crate::kit::{codes, error, error_code};
use crate::{Error, Result};

/// Goods held under references, out of their owner's holdings until settled or released.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Holds {
    pub held: BTreeMap<String, Bundle>,
}

impl Holds {
    /// Moves `bundle` out of `holdings` into a hold under `reference`. The same reference with
    /// the same bundle again is the hold already made; with another, `conflict`.
    pub fn hold(&mut self, holdings: &mut Holdings, reference: &str, bundle: &Bundle) -> Result<()> {
        check_id("reference", reference)?;
        bundle.check()?;
        if let Some(held) = self.held.get(reference) {
            return if held == bundle { Ok(()) } else { Err(error(codes::CONFLICT, format!("`{reference}` holds something else"))) };
        }
        holdings.take(bundle)?;
        self.held.insert(reference.to_owned(), bundle.clone().trimmed());
        Ok(())
    }

    /// Takes `part` out of the hold for good; the rest stays held, and an emptied hold is gone.
    /// `not_found` for a reference not held, `insufficient` when it holds less.
    pub fn settle(&mut self, reference: &str, part: &Bundle) -> Result<()> {
        let held = self.held.get(reference).ok_or_else(|| error(codes::NOT_FOUND, format!("nothing is held as `{reference}`")))?;
        let rest = held.minus(part)?;
        if rest.is_empty() {
            self.held.remove(reference);
        } else {
            self.held.insert(reference.to_owned(), rest);
        }
        Ok(())
    }

    /// Gives what the hold still has back to its owner (items that do not fit go to the mail);
    /// answers what came back, nothing for a reference not held.
    pub fn release(&mut self, holdings: &mut Holdings, catalog: &Catalog, reference: &str) -> Result<Bundle> {
        let Some(held) = self.held.remove(reference) else { return Ok(Bundle::default()) };
        holdings.give_or_mail(catalog, &held)?;
        Ok(held)
    }

    /// Everything held, together.
    pub fn total(&self) -> Bundle {
        self.held.values().fold(Bundle::default(), |acc, b| acc.plus(b).unwrap_or(acc))
    }
}

/// How long a reference's answer is kept.
pub const APPLIED_TTL_MS: u64 = 7 * 24 * 3_600_000;
/// How many references' answers are kept.
pub const APPLIED_MAX: usize = 4_096;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct Done {
    reference: String,
    at: u64,
    ok: bool,
    reply: Vec<u8>,
}

/// The answer each reference got, so a step asked for again is answered as it was the first
/// time. Keep it in the hub's state. A refusal coded `unavailable` is not kept: it says the step
/// did not run.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    #[serde(default)]
    kept: VecDeque<Done>,
}

impl Applied {
    pub fn replay(&self, reference: &str, now: u64) -> Option<Result<Vec<u8>>> {
        let d = self.kept.iter().rev().find(|d| d.reference == reference && now.saturating_sub(d.at) < APPLIED_TTL_MS)?;
        Some(if d.ok { Ok(d.reply.clone()) } else { Err(Error::new(String::from_utf8_lossy(&d.reply).into_owned())) })
    }

    pub fn record(&mut self, reference: &str, result: &Result<Vec<u8>>, now: u64) {
        if let Err(e) = result
            && error_code(&e.0).is_some_and(|(c, _)| c == codes::UNAVAILABLE)
        {
            return;
        }
        while self.kept.front().is_some_and(|d| now.saturating_sub(d.at) >= APPLIED_TTL_MS) || self.kept.len() >= APPLIED_MAX {
            self.kept.pop_front();
        }
        let (ok, reply) = match result {
            Ok(r) => (true, r.clone()),
            Err(e) => (false, e.0.clone().into_bytes()),
        };
        self.kept.push_back(Done { reference: reference.to_owned(), at: now, ok, reply });
    }

    /// Runs `act` once per `reference`: asked again, it answers what it answered first.
    pub fn run<H>(
        hub: &mut H,
        applied: fn(&mut H) -> &mut Applied,
        reference: &str,
        now: u64,
        act: impl FnOnce(&mut H) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        check_id("reference", reference)?;
        if let Some(answer) = applied(hub).replay(reference, now) {
            return answer;
        }
        let result = act(hub);
        applied(hub).record(reference, &result, now);
        result
    }

    pub fn len(&self) -> usize {
        self.kept.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::econ::items::ItemDef;

    fn setup() -> (Catalog, Holdings) {
        let c = Catalog::from(vec![ItemDef { id: "ore".into(), max_stack: 10, tradable: true, name: String::new() }]);
        let mut h = Holdings::new(2);
        h.give(&c, &Bundle::item("ore", 8).plus(&Bundle::coins("gold", 40)).unwrap()).unwrap();
        (c, h)
    }

    #[test]
    fn a_hold_leaves_the_holdings_and_comes_back_on_release() {
        let (c, mut h) = setup();
        let mut holds = Holds::default();
        holds.hold(&mut h, "o:1", &Bundle::coins("gold", 30)).unwrap();
        holds.hold(&mut h, "o:1", &Bundle::coins("gold", 30)).unwrap();
        assert_eq!(h.wallet.balance("gold"), 10, "the same hold twice is one hold");
        assert!(holds.hold(&mut h, "o:1", &Bundle::coins("gold", 5)).is_err(), "a reference holds one thing");
        assert!(holds.hold(&mut h, "o:2", &Bundle::coins("gold", 11)).is_err(), "not more than there is");
        holds.settle("o:1", &Bundle::coins("gold", 12)).unwrap();
        assert_eq!(holds.release(&mut h, &c, "o:1").unwrap(), Bundle::coins("gold", 18));
        assert_eq!(h.wallet.balance("gold"), 28);
        assert_eq!(holds.release(&mut h, &c, "o:1").unwrap(), Bundle::default(), "released once");
        assert!(holds.settle("o:1", &Bundle::coins("gold", 1)).is_err());
    }

    #[test]
    fn a_reference_is_answered_once_and_unavailable_is_not_an_answer() {
        struct Hub {
            applied: Applied,
            runs: u32,
        }
        let mut hub = Hub { applied: Applied::default(), runs: 0 };
        let act = |h: &mut Hub| {
            h.runs += 1;
            Ok(vec![1])
        };
        assert_eq!(Applied::run(&mut hub, |h| &mut h.applied, "d:1", 0, act).unwrap(), vec![1]);
        assert_eq!(Applied::run(&mut hub, |h| &mut h.applied, "d:1", 5, act).unwrap(), vec![1]);
        assert_eq!(hub.runs, 1);
        let mut applied = Applied::default();
        applied.record("d:2", &Err(error(codes::UNAVAILABLE, "the market did not answer")), 0);
        assert!(applied.replay("d:2", 1).is_none());
        applied.record("d:3", &Err(error(codes::INSUFFICIENT, "no")), 0);
        assert!(applied.replay("d:3", 1).unwrap().is_err());
        assert!(applied.replay("d:3", APPLIED_TTL_MS).is_none(), "kept for a week");
    }
}
