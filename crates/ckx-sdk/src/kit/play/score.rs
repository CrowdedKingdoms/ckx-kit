//! Authoritative scoring: server-held score books, typed win conditions, and
//! match summaries. The hub owns the only write path (a client never
//! submits its own score), then publishes the summary on a topic or
//! as an event.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// When is the match decided?
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum WinCondition {
    /// First to reach `target` points.
    ScoreReached { target: i64 },
    /// Highest score when round `final_round` completes.
    BestOfRounds { final_round: u32 },
    /// Last active participant standing (driven by forfeits/eliminations).
    LastStanding,
    /// Survive until the timer: highest score at `ends_at_ms`.
    TimeLimit { ends_at_ms: u64 },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Standing {
    pub actor_id: String,
    pub score: i64,
    pub rank: u32,
}

/// The published result of a decided match.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MatchSummary {
    pub winner_id: Option<String>,
    pub standings: Vec<Standing>,
    pub rounds_played: u32,
    pub decided_by: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ScoreBook {
    pub scores: BTreeMap<String, i64>,
    /// Actors still in the running (forfeits/eliminations remove).
    pub active: Vec<String>,
    pub decided: Option<MatchSummary>,
}

impl ScoreBook {
    pub fn new(actors: impl IntoIterator<Item = String>) -> Self {
        let active: Vec<String> = actors.into_iter().collect();
        let scores = active.iter().map(|id| (id.clone(), 0)).collect();
        Self { scores, active, decided: None }
    }

    /// Add points (the hub calls this from validated moves only).
    pub fn award(&mut self, actor_id: &str, points: i64) -> i64 {
        let entry = self.scores.entry(actor_id.to_string()).or_insert(0);
        *entry += points;
        *entry
    }

    pub fn score(&self, actor_id: &str) -> i64 {
        self.scores.get(actor_id).copied().unwrap_or(0)
    }

    /// Remove an actor from contention (death/forfeit/disconnect).
    pub fn eliminate(&mut self, actor_id: &str) {
        self.active.retain(|id| id != actor_id);
    }

    /// Standings ordered by score desc (stable; rank is 1-based, ties share
    /// the earlier rank's position order but distinct ranks — simple v1).
    pub fn standings(&self) -> Vec<Standing> {
        let mut rows: Vec<(String, i64)> = self.scores.iter().map(|(id, s)| (id.clone(), *s)).collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        rows.into_iter().enumerate().map(|(index, (actor_id, score))| Standing { actor_id, score, rank: index as u32 + 1 }).collect()
    }

    /// Evaluate the condition; the first decision sticks (idempotent after).
    pub fn evaluate(&mut self, condition: &WinCondition, rounds_played: u32, now_ms: u64) -> Option<&MatchSummary> {
        if self.decided.is_some() {
            return self.decided.as_ref();
        }
        let (decided, decided_by) = match condition {
            WinCondition::ScoreReached { target } => (self.scores.values().any(|s| *s >= *target), "score_reached"),
            WinCondition::BestOfRounds { final_round } => (rounds_played >= *final_round, "rounds_complete"),
            WinCondition::LastStanding => (self.active.len() <= 1, "last_standing"),
            WinCondition::TimeLimit { ends_at_ms } => (now_ms >= *ends_at_ms, "time_limit"),
        };
        if !decided {
            return None;
        }
        let standings = self.standings();
        let winner_id = match condition {
            // Standing-based conditions crown the survivor; score-based the top.
            WinCondition::LastStanding => self.active.first().cloned(),
            _ => standings.first().map(|s| s.actor_id.clone()),
        };
        self.decided = Some(MatchSummary { winner_id, standings, rounds_played, decided_by: decided_by.to_string() });
        self.decided.as_ref()
    }
}

/// The summary as the JSON shape clients/events consume.
pub fn summary_json(summary: &MatchSummary) -> Value {
    json!({
        "winnerId": summary.winner_id,
        "standings": summary.standings.iter().map(|s| json!({
            "actorId": s.actor_id, "score": s.score, "rank": s.rank,
        })).collect::<Vec<_>>(),
        "roundsPlayed": summary.rounds_played,
        "decidedBy": summary.decided_by,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> ScoreBook {
        ScoreBook::new(["a".to_string(), "b".to_string(), "c".to_string()])
    }

    #[test]
    fn scoring_and_standings() {
        let mut scores = book();
        scores.award("b", 5);
        scores.award("a", 3);
        scores.award("b", 2);
        let standings = scores.standings();
        assert_eq!(standings[0].actor_id, "b");
        assert_eq!(standings[0].score, 7);
        assert_eq!(standings[0].rank, 1);
        assert_eq!(standings[2].actor_id, "c");
    }

    #[test]
    fn score_reached_decides_once() {
        let mut scores = book();
        scores.award("a", 9);
        assert!(scores.evaluate(&WinCondition::ScoreReached { target: 10 }, 1, 0).is_none());
        scores.award("a", 1);
        let summary = scores.evaluate(&WinCondition::ScoreReached { target: 10 }, 1, 0).cloned().unwrap();
        assert_eq!(summary.winner_id.as_deref(), Some("a"));
        assert_eq!(summary.decided_by, "score_reached");
        // Later evaluations return the same decision even if scores move.
        scores.award("b", 99);
        let again = scores.evaluate(&WinCondition::ScoreReached { target: 10 }, 2, 0).unwrap();
        assert_eq!(again.winner_id.as_deref(), Some("a"));
    }

    #[test]
    fn last_standing_and_rounds_and_time() {
        let mut scores = book();
        scores.eliminate("a");
        assert!(scores.evaluate(&WinCondition::LastStanding, 1, 0).is_none());
        scores.eliminate("c");
        let summary = scores.evaluate(&WinCondition::LastStanding, 1, 0).unwrap();
        assert_eq!(summary.winner_id.as_deref(), Some("b"));
        assert_eq!(summary.decided_by, "last_standing");

        let mut rounds = book();
        rounds.award("c", 4);
        assert!(rounds.evaluate(&WinCondition::BestOfRounds { final_round: 3 }, 2, 0).is_none());
        let summary = rounds.evaluate(&WinCondition::BestOfRounds { final_round: 3 }, 3, 0).unwrap();
        assert_eq!(summary.winner_id.as_deref(), Some("c"));

        let mut timed = book();
        timed.award("a", 1);
        assert!(timed.evaluate(&WinCondition::TimeLimit { ends_at_ms: 60_000 }, 5, 59_999).is_none());
        assert!(timed.evaluate(&WinCondition::TimeLimit { ends_at_ms: 60_000 }, 5, 60_000).is_some());
    }

    #[test]
    fn summary_json_shape() {
        let mut scores = book();
        scores.award("a", 10);
        let summary = scores.evaluate(&WinCondition::ScoreReached { target: 10 }, 2, 0).unwrap();
        let v = summary_json(summary);
        assert_eq!(v["winnerId"], "a");
        assert_eq!(v["standings"][0]["rank"], 1);
        assert_eq!(v["roundsPlayed"], 2);
    }
}
