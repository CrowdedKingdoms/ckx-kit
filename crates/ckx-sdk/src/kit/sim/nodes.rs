//! Resource nodes: harvest/deplete/respawn lifecycle (ore veins, berry
//! bushes, fishing spots). Pure state machine — a hub keeps the pool in
//! its state.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub enum NodePhase {
    #[default]
    Ready,
    /// Depleted; respawns at the embedded time.
    Respawning { at_ms: u64 },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Node {
    pub phase: NodePhase,
    /// Remaining harvests before depletion (0 = infinite until depleted
    /// explicitly).
    pub charges: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NodePool {
    pub nodes: BTreeMap<String, Node>,
}

pub struct HarvestOutcome {
    pub harvested: bool,
    pub depleted: bool,
}

impl NodePool {
    /// Register a node (idempotent; keeps existing state).
    pub fn ensure(&mut self, id: &str, charges: u32) -> &mut Node {
        self.nodes.entry(id.to_string()).or_insert(Node { phase: NodePhase::Ready, charges })
    }

    /// Attempt a harvest. Depletes when charges run out.
    pub fn harvest(&mut self, id: &str, now_ms: u64, respawn_ms: u64) -> HarvestOutcome {
        let Some(node) = self.nodes.get_mut(id) else {
            return HarvestOutcome { harvested: false, depleted: false };
        };
        if !matches!(node.phase, NodePhase::Ready) {
            return HarvestOutcome { harvested: false, depleted: false };
        }
        let depleted = if node.charges > 0 {
            node.charges -= 1;
            node.charges == 0
        } else {
            false
        };
        if depleted {
            node.phase = NodePhase::Respawning { at_ms: now_ms + respawn_ms };
        }
        HarvestOutcome { harvested: true, depleted }
    }

    /// Tick respawns: returns ids that just came back (re-arm charges).
    pub fn tick(&mut self, now_ms: u64, recharge: u32) -> Vec<String> {
        let mut respawned = Vec::new();
        for (id, node) in &mut self.nodes {
            if let NodePhase::Respawning { at_ms } = node.phase
                && now_ms >= at_ms
            {
                node.phase = NodePhase::Ready;
                node.charges = recharge;
                respawned.push(id.clone());
            }
        }
        respawned
    }

    pub fn is_ready(&self, id: &str) -> bool {
        matches!(self.nodes.get(id), Some(Node { phase: NodePhase::Ready, .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harvest_deplete_respawn() {
        let mut pool = NodePool::default();
        pool.ensure("vein-1", 2);
        assert!(pool.harvest("vein-1", 1_000, 30_000).harvested);
        let second = pool.harvest("vein-1", 2_000, 30_000);
        assert!(second.harvested && second.depleted);
        // Depleted: further harvests fail; not ready.
        assert!(!pool.harvest("vein-1", 3_000, 30_000).harvested);
        assert!(!pool.is_ready("vein-1"));
        // Respawn on schedule with recharged charges.
        assert!(pool.tick(31_000, 2).is_empty());
        assert_eq!(pool.tick(32_000, 2), vec!["vein-1".to_string()]);
        assert!(pool.harvest("vein-1", 33_000, 30_000).harvested);
    }

    #[test]
    fn infinite_nodes_never_self_deplete() {
        let mut pool = NodePool::default();
        pool.ensure("well", 0);
        for t in 0..50 {
            let outcome = pool.harvest("well", t, 1_000);
            assert!(outcome.harvested && !outcome.depleted);
        }
    }
}
