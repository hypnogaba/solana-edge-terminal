//! The bot layer: spot a gap between pools, decide, and act.
//!
//! Both lanes share every part of this except the wallet and the feed that
//! triggers them, which is the whole point: any difference in outcome comes
//! from when each lane learned, not from what it decided.

pub mod arb;
pub mod config;
pub mod duel;
pub mod execute;
pub mod lane;
pub mod ledger;
pub mod market;
pub mod pools;
pub mod prices;
pub mod whirlpool;
