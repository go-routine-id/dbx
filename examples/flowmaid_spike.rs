//! M0 spike (roadmap task 0.8): prove flowmaid's ER scene can be painted into
//! a ratatui canvas with unicode box/line drawing — before M3 depends on it.
//!
//! Contract (spike agent):
//! - Parse a small hardcoded `erDiagram` (3-4 tables with 1:N and a junction
//!   M:N relationship).
//! - Obtain layout geometry from flowmaid (scene API — positions + edges).
//! - Paint it into a ratatui buffer: nodes as rounded boxes with the entity
//!   name, edges as lines with crow's-foot-ish markers where feasible.
//! - Runs as a standalone TUI (`cargo run --example flowmaid_spike`): q quits,
//!   hjkl/arrows pan. If flowmaid's API turns out not to fit, document exactly
//!   what is missing in a comment at the top of this file — that is a
//!   successful spike outcome too.

fn main() {
    todo!("spike agent: flowmaid erDiagram -> ratatui canvas")
}
