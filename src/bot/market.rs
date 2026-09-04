//! The state of each pair right now, rather than a log of every trigger.
//!
//! A tape of "the gap was 1 bp and the fees are 6" repeated sixty times is not
//! information, it is the same non-event printed until it fills the screen. A
//! trading screen shows where each market stands and what it would take to act,
//! and keeps the tape for things that actually happened.

use serde::Serialize;

use crate::bot::arb::gap_bps;
use crate::bot::pools::PoolBook;
use crate::bot::prices::PriceBook;

#[derive(Debug, Clone, Serialize)]
pub struct MarketRow {
    pub pair: &'static str,
    /// Widest gap between any two pools of this pair, right now.
    pub gap_bps: u64,
    /// What that gap has to beat: both pools' fees plus the margin.
    pub needs_bps: u64,
    /// The two pools the gap was measured between.
    pub route: String,
    /// How far the gap is from being worth taking. Zero means it is.
    pub short_by_bps: u64,
    /// Age of the older of the two prices, in milliseconds.
    pub age_ms: u64,
    pub priced: bool,
    /// How many of the pair's pools have reported. A gap needs two, so one
    /// reporting pool is a real state and not the same as none.
    pub pools_priced: usize,
    pub pools_total: usize,
    /// The prices behind this gap are older than the bot will act on, so the
    /// gap is not a gap. Showing it as one would put the screen at odds with
    /// the bot it is meant to explain.
    pub stale: bool,
}

/// One row per pool: the raw readings the pair rows are computed from.
#[derive(Debug, Clone, Serialize)]
pub struct PoolRow {
    pub pair: &'static str,
    pub label: String,
    pub fee_bps: u64,
    pub price: Option<f64>,
    pub age_ms: Option<u64>,
    pub slot: Option<u64>,
}

/// Every watched pool, grouped by pair. A pair row says what the decision is;
/// these say what it was decided from, which is what a viewer asks next.
pub fn pool_rows(
    pools: &PoolBook,
    prices: &PriceBook,
    now: std::time::Instant,
) -> Vec<PoolRow> {
    let mut rows: Vec<PoolRow> = pools
        .all()
        .iter()
        .map(|pool| match prices.get(&pool.address) {
            Some(p) => PoolRow {
                pair: pool.pair,
                label: pool.label.to_string(),
                fee_bps: p.state.fee_bps(),
                price: Some(p.state.price),
                age_ms: Some(
                    now.saturating_duration_since(p.at).as_millis().min(u64::MAX as u128) as u64,
                ),
                slot: Some(p.slot),
            },
            None => PoolRow {
                pair: pool.pair,
                label: pool.label.to_string(),
                fee_bps: 0,
                price: None,
                age_ms: None,
                slot: None,
            },
        })
        .collect();
    // Pools that are reporting come first: a screen of blanks says nothing.
    rows.sort_by(|a, b| {
        b.price
            .is_some()
            .cmp(&a.price.is_some())
            .then_with(|| a.pair.cmp(b.pair))
            .then_with(|| a.label.cmp(&b.label))
    });
    rows
}

/// One row per pair, ordered by how close it is to being worth taking.
pub fn snapshot(
    pools: &PoolBook,
    prices: &PriceBook,
    margin_bps: u64,
    max_price_age: std::time::Duration,
    now: std::time::Instant,
) -> Vec<MarketRow> {
    let mut rows: Vec<MarketRow> = pools
        .pairs()
        .into_iter()
        .map(|pair| {
            let mut best: Option<(u64, u64, String, u64)> = None;
            let members: Vec<_> =
                pools.all().iter().filter(|pool| pool.pair == pair).collect();
            let reporting =
                members.iter().filter(|pool| prices.get(&pool.address).is_some()).count();
            for (index, one) in members.iter().enumerate() {
                for other in members.iter().skip(index + 1) {
                    let (Some(a), Some(b)) =
                        (prices.get(&one.address), prices.get(&other.address))
                    else {
                        continue;
                    };
                    let gap = gap_bps(&a, &b);
                    let needs = a.state.fee_bps() + b.state.fee_bps() + margin_bps;
                    let age = now
                        .saturating_duration_since(a.at.min(b.at))
                        .as_millis()
                        .min(u64::MAX as u128) as u64;
                    let route = format!("{} | {}", one.label, other.label);
                    if best.as_ref().is_none_or(|(g, _, _, _)| gap > *g) {
                        best = Some((gap, needs, route, age));
                    }
                }
            }
            match best {
                Some((gap, needs, route, age)) => MarketRow {
                    pair,
                    gap_bps: gap,
                    needs_bps: needs,
                    route,
                    short_by_bps: needs.saturating_sub(gap),
                    age_ms: age,
                    priced: true,
                    stale: age > max_price_age.as_millis().min(u64::MAX as u128) as u64,
                    pools_priced: reporting,
                    pools_total: members.len(),
                },
                None => MarketRow {
                    pair,
                    gap_bps: 0,
                    needs_bps: 0,
                    route: String::new(),
                    short_by_bps: u64::MAX,
                    age_ms: 0,
                    priced: false,
                    stale: false,
                    pools_priced: reporting,
                    pools_total: members.len(),
                },
            }
        })
        .collect();
    // Closest to actionable first: that is the row worth glancing at.
    rows.sort_by_key(|row| (row.stale, row.short_by_bps, row.pair));
    rows
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::bot::prices::Priced;

    /// Long enough that the fixtures are never stale by accident.
    const FRESH: Duration = Duration::from_secs(5);
    use crate::bot::whirlpool::PoolState;

    fn book_with(prices: &[(usize, f64, u32)]) -> (PoolBook, PriceBook, Instant) {
        let pools = PoolBook::demo();
        let book = PriceBook::default();
        let now = Instant::now();
        for (index, price, fee) in prices {
            book.set(
                pools.all()[*index].address,
                Priced { state: PoolState { price: *price, fee_rate: *fee }, slot: 1, at: now },
            );
        }
        (pools, book, now)
    }

    #[test]
    fn a_pair_reports_the_gap_and_what_it_has_to_beat() {
        // 100 bps apart, 2+2 bps of fees, 1 bp of margin: it clears.
        let (pools, prices, now) = book_with(&[(0, 106.0, 200), (1, 105.0, 200)]);
        let rows = snapshot(&pools, &prices, 1, FRESH, now);
        let row = rows.iter().find(|r| r.pair == "SOL/USDC").expect("pair");
        assert_eq!(row.gap_bps, 95);
        assert_eq!(row.needs_bps, 5);
        assert_eq!(row.short_by_bps, 0, "a gap over the floor is short by nothing");
    }

    #[test]
    fn the_ordinary_case_says_how_far_off_it_is() {
        // The real market: two bps apart against six of fees. The useful fact
        // is not "no", it is "four short".
        let (pools, prices, now) = book_with(&[(0, 105.02, 400), (1, 105.0, 200)]);
        let row = snapshot(&pools, &prices, 0, FRESH, now)
            .into_iter()
            .find(|r| r.pair == "SOL/USDC")
            .expect("pair");
        assert_eq!(row.gap_bps, 1);
        assert_eq!(row.needs_bps, 6);
        assert_eq!(row.short_by_bps, 5);
    }

    #[test]
    fn a_pair_with_no_prices_is_marked_rather_than_shown_as_zero() {
        // A zero gap and a real zero gap look identical, and one of them means
        // "we have no idea".
        let (pools, prices, now) = book_with(&[]);
        let rows = snapshot(&pools, &prices, 1, FRESH, now);
        assert!(rows.iter().all(|r| !r.priced));
    }

    #[test]
    fn one_reporting_pool_is_not_the_same_as_none() {
        // A pair with one live pool has no gap, but it is not silent either,
        // and a row that says "no price yet" for both hides which is which.
        let (pools, prices, now) = book_with(&[(0, 105.0, 400)]);
        let row = snapshot(&pools, &prices, 1, FRESH, now)
            .into_iter()
            .find(|r| r.pair == "SOL/USDC")
            .expect("pair");
        assert!(!row.priced, "one pool cannot make a gap");
        assert_eq!(row.pools_priced, 1);
        assert!(row.pools_total >= 2);
    }

    #[test]
    fn the_closest_to_actionable_sorts_first() {
        let (pools, prices, now) =
            book_with(&[(0, 105.0, 200), (1, 105.0, 200), (3, 106.0, 200), (4, 105.0, 200)]);
        let rows = snapshot(&pools, &prices, 1, FRESH, now);
        assert!(rows[0].short_by_bps <= rows[1].short_by_bps);
    }

    #[test]
    fn every_watched_pool_gets_a_row_whether_or_not_it_has_reported() {
        // A pool that has never reported is the most interesting kind of gap in
        // the data, so it has to appear rather than be filtered out.
        let (pools, prices, now) = book_with(&[(0, 105.0, 400)]);
        let rows = pool_rows(&pools, &prices, now);
        assert_eq!(rows.len(), pools.all().len());
        assert!(rows[0].price.is_some(), "a reporting pool sorts first");
        assert!(rows.iter().any(|r| r.price.is_none()));
    }

    #[test]
    fn a_pool_row_carries_the_fee_that_the_gap_has_to_beat() {
        let (pools, prices, now) = book_with(&[(0, 105.0, 400)]);
        let row = pool_rows(&pools, &prices, now)
            .into_iter()
            .find(|r| r.price.is_some())
            .expect("one priced pool");
        assert_eq!(row.fee_bps, 4);
    }

    #[test]
    fn a_gap_from_prices_the_bot_would_refuse_is_not_shown_as_a_gap() {
        // The book keeps the last price of a pool that has not traded for an
        // hour. Comparing that against a live one produces a wide, confident
        // and completely fictional gap.
        let pools = PoolBook::demo();
        let book = PriceBook::default();
        let now = Instant::now();
        book.set(pools.all()[0].address, Priced {
            state: PoolState { price: 130.0, fee_rate: 200 }, slot: 1, at: now,
        });
        book.set(pools.all()[1].address, Priced {
            state: PoolState { price: 105.0, fee_rate: 200 },
            slot: 1,
            at: now - Duration::from_secs(3600),
        });
        let row = snapshot(&pools, &book, 1, FRESH, now)
            .into_iter()
            .find(|r| r.pair == "SOL/USDC")
            .expect("pair");
        assert!(row.stale, "an hour-old price cannot make a live gap");
    }

    #[test]
    fn the_age_is_the_older_of_the_two_prices() {
        let pools = PoolBook::demo();
        let book = PriceBook::default();
        let now = Instant::now();
        book.set(pools.all()[0].address, Priced {
            state: PoolState { price: 106.0, fee_rate: 200 }, slot: 1, at: now,
        });
        book.set(pools.all()[1].address, Priced {
            state: PoolState { price: 105.0, fee_rate: 200 },
            slot: 1,
            at: now - Duration::from_millis(800),
        });
        let row = snapshot(&pools, &book, 1, FRESH, now)
            .into_iter()
            .find(|r| r.pair == "SOL/USDC")
            .expect("pair");
        assert!(row.age_ms >= 790 && row.age_ms <= 810, "got {}", row.age_ms);
    }
}
