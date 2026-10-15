//! Agent brains for hubs that move things: [`path`] (A* with a step budget), [`flow`] (one
//! distance field many agents descend), [`steering`] (seek, flee, arrive, wander, separate),
//! [`brain`] (behavior trees as JSON) and [`turn`] (budgeted movers for turn-based enemies).
//! Every search is capped, so one bad query cannot use up a call's fuel.

pub mod brain;
pub mod flow;
pub mod path;
pub mod steering;
pub mod turn;
