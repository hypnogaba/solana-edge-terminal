//! A full-battle screen driven by made-up data, for rehearsing the stream
//! before the shred feed exists.
//!
//! The live terminal cannot show what a race looks like until there is a
//! second lane to race against, and the operator needs to see and record that
//! beforehand. Everything here is invented, so the snapshot carries a flag the
//! page turns into a banner: a screen of fabricated numbers that does not say
//! so is worse than no screen at all.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::bot::ledger::{Booking, Ledger, Trade};
use crate::bot::market;
use crate::bot::pools::PoolBook;
use crate::bot::prices::{PriceBook, Priced};
use crate::bot::whirlpool::PoolState;
use crate::sources::Source;

/// Deterministic noise. A dependency on `rand` to shake a demo would be a poor
/// trade, and a fixed sequence makes the rehearsal repeatable between takes.
pub struct Roll(u64);

impl Roll {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        // xorshift64*, small and good enough to make a chart wiggle.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..range`.
    pub fn upto(&mut self, range: u64) -> u64 {
        if range == 0 {
            0
        } else {
            self.next() % range
        }
    }

    /// A drift in `-span..=span`.
    pub fn drift(&mut self, span: i64) -> i64 {
        self.upto((span * 2 + 1) as u64) as i64 - span
    }
}

/// The state a rehearsal carries between ticks, so prices and counters move
/// like a session rather than jumping every second.
pub struct Rehearsal {
    roll: Roll,
    pools: PoolBook,
    prices: PriceBook,
    /// One mid per pair, drifting slowly. Pools quote around it.
    mid: Vec<f64>,
    /// Each pool's deviation from its pair's mid, as a fraction. It reverts,
    /// because arbitrage is what makes it revert: that is the whole trade.
    dev: Vec<f64>,
    started: Instant,
    tick: u64,
    packets: u64,
    transactions: u64,
    duels: Vec<serde_json::Value>,
    slot: u64,
    taken: u64,
    spent: u64,
    ledger: Ledger,
    /// A rehearsal shows the live case, so it carries a wallet that moves.
    wallet_started: u64,
    wallet: u64,
}

/// The lane the feed is measured against, and the lane it beats.
pub const FAST: &str = "doublezero";
pub const SLOW: &str = "commercial";

impl Rehearsal {
    pub fn new(seed: u64) -> Self {
        let pools = PoolBook::demo();
        let mid = pools.all().iter().map(|p| seed_price(p.pair)).collect();
        let dev = vec![0.0; pools.all().len()];
        Self {
            roll: Roll::new(seed),
            pools,
            prices: PriceBook::default(),
            mid,
            dev,
            started: Instant::now(),
            tick: 0,
            packets: 0,
            transactions: 0,
            duels: Vec::new(),
            slot: 444_000_000,
            taken: 0,
            spent: 0,
            ledger: Ledger::default(),
            wallet_started: 2_000_000_000,
            wallet: 2_000_000_000,
        }
    }

    /// One second of a busy session.
    pub fn step(&mut self, feeds: &[Source]) -> serde_json::Value {
        self.tick += 1;
        self.slot += 2;
        // A leader's shreds on a live group, at the order of magnitude the
        // probe measured on the Kalshi feed.
        self.packets += 44_000 + self.roll.upto(9_000);
        self.transactions += 2_600 + self.roll.upto(700);

        let now = Instant::now();
        // Pools of one pair quote around a shared mid and are pulled back to it
        // by the arbitrage itself. Letting each wander freely, which is what the
        // first version did, walked them hundreds of basis points apart inside
        // a few minutes and produced a screen where six pairs of seven cleared
        // their fees at once. The live run measured 430 clears in 64,130
        // readings, which is 0.7 per cent, and a rehearsal that shows seventy
        // teaches the operator to promise something that does not happen.
        for (index, pool) in self.pools.all().iter().enumerate() {
            self.mid[index] *= 1.0 + self.roll.drift(4) as f64 / 100_000.0;
            // Reversion, then the ordinary noise between two venues.
            self.dev[index] = self.dev[index] * 0.55
                + self.roll.drift(2) as f64 / 100_000.0;
            // Now and then a real swap moves one pool and leaves it dislocated
            // for a moment. That moment is the entire opportunity.
            if self.roll.upto(400) == 0 {
                let shock = (20 + self.roll.upto(50)) as f64 / 10_000.0;
                self.dev[index] += if self.roll.upto(2) == 0 { shock } else { -shock };
            }
            let price = self.mid[index] * (1.0 + self.dev[index]);
            self.prices.set(
                pool.address,
                Priced {
                    state: PoolState { price, fee_rate: pool_fee(index) },
                    slot: self.slot,
                    at: now - Duration::from_millis(self.roll.upto(900)),
                },
            );
        }

        let markets = market::snapshot(
            &self.pools,
            &self.prices,
            5,
            Duration::from_millis(5_000),
            now,
        );
        let pool_rows = market::pool_rows(&self.pools, &self.prices, now);

        for row in markets.iter().filter(|row| row.priced && !row.stale) {
            // Not every reading becomes an event; a busy pair produces a few
            // per second and a quiet one none.
            if self.roll.upto(100) > 22 {
                continue;
            }
            let head_start = 18 + self.roll.upto(64);
            let lost = if row.short_by_bps == 0 { 1 + self.roll.upto(row.gap_bps.max(2)) } else { 0 };
            let took = row.short_by_bps == 0;
            if took {
                self.taken += 1;
                self.spent += 5_000;
            }
            // A rehearsal shows production, which means the money as well as
            // the race. The size is the default one attempt, and the profit is
            // the gap on that size less both pool fees and the network fee.
            // What is left is the surplus over the fee floor, not the shortfall:
            // short_by_bps is zero exactly when the trade clears, so using it
            // here booked every winning trade at nothing.
            let size = 250_000_000u64;
            let surplus = row.gap_bps.saturating_sub(row.needs_bps);
            let net = if took {
                (size / 10_000 * surplus).saturating_sub(5_000)
            } else {
                0
            };
            // `net` already has the network fee taken out of it, so charging
            // the fee again here made the balance and the earnings disagree by
            // exactly one fee per trade.
            self.wallet += net;
            self.ledger.record(Trade {
                at_s: self.started.elapsed().as_secs_f64(),
                slot: self.slot,
                lane: FAST.to_string(),
                pair: row.pair,
                route: row.route.replace(" | ", " -> "),
                size_lamports: size,
                gross_bps: row.gap_bps,
                net_lamports: net,
                booking: if took { Booking::Realised } else { Booking::None },
                outcome: if took {
                    format!("sent {:x}", self.slot)
                } else {
                    "under the fees".to_string()
                },
            });
            self.duels.insert(0, json!({
                "slot": self.slot,
                "pair": row.pair,
                "head_start_ms": head_start,
                "gap_lost_bps": lost,
                "views": [
                    {"lane": FAST, "route": row.route, "gap_bps": row.gap_bps, "tradeable": took},
                    {"lane": SLOW, "route": row.route,
                     "gap_bps": row.gap_bps.saturating_sub(lost), "tradeable": false},
                ],
            }));
        }
        self.duels.truncate(250);

        let paired = self.duels.len() as u64;
        let seen_fast = self.tick * 9;
        let seen_slow = self.tick * 8;
        json!({
            "rehearsal": true,
            "uptime_s": self.started.elapsed().as_secs_f64(),
            "window_s": 3600.0,
            "reference_lane": FAST,
            "sources": feeds,
            "shreds": {
                "packets": self.packets, "shreds": self.packets * 2,
                "batches_ready": self.packets / 32, "entries": self.transactions / 4,
                "transactions": self.transactions, "deshred_errors": 0, "panics": 0,
            },
            "lanes": [{
                "lane": SLOW,
                "median_lead_ms": 31 + self.roll.upto(9),
                "p10_lead_ms": 12, "p90_lead_ms": 88,
                "reference_first_pct": 93, "n_window": paired, "seen": seen_slow,
                // A plausible shape: a long right tail, and a P1 that is
                // negative because no feed wins every race.
                "quantiles": ladder(&[-14.0, -2.0, 9.0, 21.0, 34.0, 52.0, 58.0, 79.0, 104.0, 178.0]),
                "led_quantiles": ladder(&[3.0, 7.0, 12.0, 24.0, 37.0, 55.0, 61.0, 82.0, 107.0, 181.0]),
            }],
            "bots": {
                FAST: {"seen": seen_fast, "actionable": self.taken, "below_cost": seen_fast - self.taken,
                       "best_gap_bps": 140, "no_price": 0, "implausible": 0, "duplicates": 0},
                SLOW: {"seen": seen_slow, "actionable": 0, "below_cost": seen_slow,
                       "best_gap_bps": 96, "no_price": 0, "implausible": 0, "duplicates": 0},
            },
            "execution": {
                FAST: {"offered": self.taken, "refused": 0, "refused_killed": 0, "refused_cap": 0,
                       "refused_wallet": 0, "build_failed": 0, "sent": self.taken, "spent": self.spent},
            },
            "triggers": {FAST: seen_fast, SLOW: seen_slow},
            "duels": self.duels,
            "mode": "live",
            "wallet": {"lamports": self.wallet, "started_lamports": self.wallet_started},
            "trades": self.ledger.recent(),
            "money": self.ledger.totals(),
            "duel_paired": paired,
            "awaiting_pair": 0,
            "markets": markets,
            "pools": pool_rows,
        })
    }
}

/// The ladder shred-stats publishes, given ten values already in order.
fn ladder(values: &[f64; 10]) -> Vec<serde_json::Value> {
    ["P1", "P5", "P10", "P25", "P50", "P75", "P80", "P90", "P95", "P99"]
        .iter()
        .zip(values)
        .map(|(at, lead)| json!({"at": at, "lead_ms": lead}))
        .collect()
}

fn seed_price(pair: &str) -> f64 {
    match pair {
        "SOL/USDC" => 104.5,
        "JUP/SOL" => 0.0021,
        "SOL/BONK" => 3.2e7,
        "SOL/JLP" => 23.2,
        "SOL/FART" => 604.0,
        "MEW/SOL" => 0.000031,
        _ => 0.0012,
    }
}

fn pool_fee(index: usize) -> u32 {
    [200u32, 400, 500, 1600, 3000][index % 5]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feeds() -> Vec<Source> {
        vec![crate::sources::lane_source("commercial=env:RPC_WS_COMMERCIAL")]
    }

    #[test]
    fn a_rehearsal_snapshot_says_that_it_is_one() {
        // Fabricated numbers on a screen that does not admit it are worse than
        // no screen, so this flag is the feature, not a detail of it.
        let value = Rehearsal::new(7).step(&feeds());
        assert_eq!(value["rehearsal"], serde_json::Value::Bool(true));
    }

    #[test]
    fn it_carries_every_field_the_pages_read() {
        // The rehearsal builds its own snapshot, so it can drift from the real
        // one and show a screen the live run would never produce.
        let value = Rehearsal::new(3).step(&feeds());
        for key in [
            "shreds", "lanes", "duels", "bots", "triggers", "reference_lane",
            "duel_paired", "execution", "markets", "pools", "sources", "uptime_s",
            "trades", "money", "wallet", "mode",
        ] {
            assert!(value.get(key).is_some(), "rehearsal has no {key}");
        }
        let duel = &value["duels"][0];
        for key in ["slot", "pair", "head_start_ms", "gap_lost_bps", "views"] {
            assert!(duel.get(key).is_some(), "a rehearsed duel has no {key}");
        }
    }

    #[test]
    fn it_produces_a_race_with_two_lanes_on_shared_events() {
        // The whole point of the rehearsal is the thing the live run cannot
        // show yet: one event seen by both lanes, at different times.
        let mut session = Rehearsal::new(11);
        let mut value = session.step(&feeds());
        for _ in 0..40 {
            value = session.step(&feeds());
        }
        let duels = value["duels"].as_array().expect("duels");
        assert!(!duels.is_empty(), "a rehearsal with no events shows nothing");
        let views = duels[0]["views"].as_array().expect("views");
        assert_eq!(views.len(), 2);
        assert_eq!(views[0]["lane"], FAST);
        assert!(duels[0]["head_start_ms"].as_u64().expect("head start") > 0);
    }

    #[test]
    fn a_clearing_gap_stays_as_rare_on_the_rehearsal_as_it_is_live() {
        // The live run measured 430 clears in 64,130 readings. A rehearsal
        // where most pairs clear at once is not a rehearsal of anything: it
        // would have the operator promise a rate the market does not offer.
        let mut session = Rehearsal::new(13);
        let mut priced = 0usize;
        let mut clearing = 0usize;
        for _ in 0..600 {
            let value = session.step(&feeds());
            for row in value["markets"].as_array().expect("markets") {
                if !row["priced"].as_bool().unwrap_or(false) {
                    continue;
                }
                priced += 1;
                if row["short_by_bps"].as_u64() == Some(0) {
                    clearing += 1;
                }
            }
        }
        assert!(priced > 1_000, "not enough readings to judge: {priced}");
        let share = 100.0 * clearing as f64 / priced as f64;
        assert!(share < 8.0, "{share:.1}% of readings cleared, the market gives well under that");
        assert!(clearing > 0, "a rehearsal that never fires shows nothing either");
    }

    #[test]
    fn the_wallet_only_moves_when_a_trade_is_actually_sent() {
        // The rehearsal exists to show production, and production is the money.
        // A balance that drifts without a matching trade would teach the viewer
        // to read the number as decoration.
        let mut session = Rehearsal::new(21);
        let mut value = session.step(&feeds());
        for _ in 0..60 {
            value = session.step(&feeds());
        }
        let wallet = &value["wallet"];
        let sent = value["money"]["sent"].as_u64().expect("sent");
        let moved = wallet["lamports"] != wallet["started_lamports"];
        assert_eq!(moved, sent > 0, "the balance moved without a sent trade");
    }

    #[test]
    fn the_balance_change_equals_what_the_ledger_says_was_earned() {
        // Two numbers on the same screen describing the same money. If they
        // disagree a viewer cannot tell which one is wrong, and both stop
        // being worth reading.
        let mut session = Rehearsal::new(31);
        let mut value = session.step(&feeds());
        for _ in 0..80 {
            value = session.step(&feeds());
        }
        let wallet = &value["wallet"];
        let change = wallet["lamports"].as_u64().expect("lamports")
            - wallet["started_lamports"].as_u64().expect("started");
        let earned = value["money"]["realised_lamports"].as_u64().expect("realised");
        assert_eq!(change, earned, "the balance and the ledger disagree");
    }

    #[test]
    fn the_same_seed_replays_the_same_session() {
        let a = Rehearsal::new(5).step(&feeds());
        let b = Rehearsal::new(5).step(&feeds());
        assert_eq!(a["markets"][0]["pair"], b["markets"][0]["pair"]);
        assert_eq!(a["shreds"]["packets"], b["shreds"]["packets"]);
    }
}
