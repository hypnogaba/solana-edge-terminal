//! The decision. Pure: no clock, no wallet, no network.
//!
//! Both lanes call this same function with the same price book, which is what
//! makes the two bots identical in every way except when they were told to
//! look. Keep it that way: anything that reads the outside world belongs in
//! the caller.
//!
//! The trade is a round trip in SOL. Sell SOL on the pool where it is dear,
//! buy it back where it is cheap, and keep the difference less both pools'
//! fees and the network fee. Starting and ending in SOL is deliberate: the
//! profit is then lamports, and nothing has to be priced to judge it.

use crate::bot::prices::Priced;

/// A gap wider than this between two pools of one pair is not an opportunity,
/// it is bad data. The usual cause is a pool whose token order is the reverse
/// of its sibling's, which decodes to a reciprocal price and would otherwise
/// read as a gap of a million basis points.
pub const IMPLAUSIBLE_BPS: u64 = 500;

#[derive(Debug, Clone, Copy)]
pub struct Costs {
    /// What the atomic transaction costs to land, win or lose.
    pub network_fee_lamports: u64,
    /// How much better than break-even a gap has to be before we act.
    pub margin_bps: u64,
    /// How old a price may be and still be traded on. A gap computed from a
    /// stale price is a gap that may already be gone.
    pub max_price_age: std::time::Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Opportunity {
    /// Where SOL is dear: the first leg sells there.
    pub sell_on: &'static str,
    /// Where SOL is cheap: the second leg buys it back.
    pub buy_on: &'static str,
    pub in_lamports: u64,
    /// The raw price difference between the pools, before any cost.
    pub gross_bps: u64,
    /// What is left after both pool fees and the network fee.
    pub net_lamports: u64,
    /// The oldest of the two prices, in milliseconds. A gap computed from a
    /// stale price is a gap that may already be gone.
    pub staleness_ms: u64,
}

/// The raw distance between two prices in basis points, whichever way round
/// they sit. Reported even when no trade follows, so the terminal can show how
/// close the market came instead of only showing silence.
pub fn gap_bps(one: &Priced, other: &Priced) -> u64 {
    let (a, b) = (one.state.price, other.state.price);
    if a <= 0.0 || b <= 0.0 || !a.is_finite() || !b.is_finite() {
        return 0;
    }
    let (dear, cheap) = if a > b { (a, b) } else { (b, a) };
    let gap = ((dear / cheap - 1.0) * 10_000.0).floor();
    if !gap.is_finite() || gap <= 0.0 {
        return 0;
    }
    gap as u64
}

/// Whether both prices are fresh enough to act on.
pub fn fresh_enough(one: &Priced, other: &Priced, now: std::time::Instant, max_age: std::time::Duration) -> bool {
    now.saturating_duration_since(one.at) <= max_age
        && now.saturating_duration_since(other.at) <= max_age
}

/// `sell` is the pool the SOL is sold on, `buy` the one it is bought back on.
/// Returns None unless selling on `sell` is actually the dear side.
/// Why an opportunity was declined. "It did not trade" is not an answer a
/// screen can give: the operator needs to know whether the gap was too small,
/// the price too old, or the trade simply too small to carry the network fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decline {
    /// The two sides are the same pool, or the size is zero.
    NotATrade,
    /// One of the prices is older than the bot will act on.
    Stale,
    /// The side being sold is not the dear one, so this direction is backwards.
    WrongWayRound,
    /// Outside what a real pool pair does; a decode fault looks like this.
    Implausible,
    /// The gap does not cover both pool fees.
    UnderFees,
    /// It covers the fees but not by the margin demanded.
    UnderMargin,
    /// The gap is real, but this trade size cannot carry the network fee.
    TooSmallForTheFee,
}

pub fn evaluate(
    in_lamports: u64,
    sell: (&'static str, &Priced),
    buy: (&'static str, &Priced),
    costs: &Costs,
    now: std::time::Instant,
) -> Result<Opportunity, Decline> {
    let (sell_label, sell_priced) = sell;
    let (buy_label, buy_priced) = buy;
    if in_lamports == 0 || sell_label == buy_label {
        return Err(Decline::NotATrade);
    }
    if !fresh_enough(sell_priced, buy_priced, now, costs.max_price_age) {
        return Err(Decline::Stale);
    }
    let (dear, cheap) = (sell_priced.state.price, buy_priced.state.price);
    if !(dear > cheap) || cheap <= 0.0 {
        return Err(Decline::WrongWayRound);
    }

    let gross_bps = ((dear / cheap - 1.0) * 10_000.0).floor();
    if gross_bps <= 0.0 {
        return Err(Decline::WrongWayRound);
    }
    if gross_bps > IMPLAUSIBLE_BPS as f64 {
        return Err(Decline::Implausible);
    }

    // Both legs pay their pool's fee. Rounded up on each side, so the answer
    // can only understate the profit.
    let fee_bps = sell_priced.state.fee_bps() + buy_priced.state.fee_bps();
    let net_bps = gross_bps as u64;
    let Some(net_bps) = net_bps.checked_sub(fee_bps) else {
        return Err(Decline::UnderFees);
    };
    if net_bps < costs.margin_bps {
        return Err(Decline::UnderMargin);
    }

    let gain = (in_lamports as u128) * u128::from(net_bps) / 10_000;
    let gain = u64::try_from(gain).unwrap_or(u64::MAX);
    let net_lamports = gain.saturating_sub(costs.network_fee_lamports);
    if net_lamports == 0 {
        // The gap is genuine and clears the fees; the trade is simply too small
        // for the network fee to come out of it. Reporting that as "under the
        // fees" sends the operator to look at the wrong number.
        return Err(Decline::TooSmallForTheFee);
    }

    let staleness_ms = now
        .saturating_duration_since(sell_priced.at.min(buy_priced.at))
        .as_millis()
        .min(u64::MAX as u128) as u64;

    Ok(Opportunity {
        sell_on: sell_label,
        buy_on: buy_label,
        in_lamports,
        gross_bps: gross_bps as u64,
        net_lamports,
        staleness_ms,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::bot::whirlpool::PoolState;

    fn priced(price: f64, fee_rate: u32, age: Duration, now: Instant) -> Priced {
        Priced { state: PoolState { price, fee_rate }, slot: 1, at: now - age }
    }

    fn costs() -> Costs {
        Costs {
            network_fee_lamports: 5_000,
            margin_bps: 1,
            max_price_age: Duration::from_secs(5),
        }
    }

    #[test]
    fn a_gap_wider_than_both_fees_is_an_opportunity() {
        let now = Instant::now();
        // 100 bps apart, 4 bps of fees: 96 bps net on 0.005 SOL is 48,000
        // lamports, less the 5,000 network fee.
        let dear = priced(106.0, 200, Duration::ZERO, now);
        let cheap = priced(105.0, 200, Duration::ZERO, now);
        let found = evaluate(5_000_000, ("dear", &dear), ("cheap", &cheap), &costs(), now)
            .expect("opportunity");
        assert_eq!(found.sell_on, "dear");
        assert_eq!(found.buy_on, "cheap");
        assert_eq!(found.gross_bps, 95);
        assert_eq!(found.net_lamports, 5_000_000 * 91 / 10_000 - 5_000);
    }

    #[test]
    fn a_gap_narrower_than_the_fees_is_not_an_opportunity() {
        // This is the normal state of the market: pools sit a couple of basis
        // points apart while the fees are several. A bot that traded here
        // would lose on every fill.
        let now = Instant::now();
        let dear = priced(105.14158, 200, Duration::ZERO, now);
        let cheap = priced(105.11364, 500, Duration::ZERO, now);
        assert_eq!(evaluate(5_000_000, ("dear", &dear), ("cheap", &cheap), &costs(), now), Err(Decline::UnderFees));
    }

    #[test]
    fn selling_on_the_cheap_side_is_never_an_opportunity() {
        let now = Instant::now();
        let dear = priced(106.0, 200, Duration::ZERO, now);
        let cheap = priced(105.0, 200, Duration::ZERO, now);
        assert_eq!(evaluate(5_000_000, ("cheap", &cheap), ("dear", &dear), &costs(), now), Err(Decline::WrongWayRound));
    }

    #[test]
    fn a_gain_that_does_not_cover_the_network_fee_is_not_an_opportunity() {
        let now = Instant::now();
        // 10 bps gross, 4 bps fees, 6 bps net on 5,000 lamports is 3 lamports.
        let dear = priced(105.105, 200, Duration::ZERO, now);
        let cheap = priced(105.0, 200, Duration::ZERO, now);
        assert_eq!(evaluate(5_000, ("dear", &dear), ("cheap", &cheap), &costs(), now), Err(Decline::TooSmallForTheFee));
    }

    #[test]
    fn the_same_pool_on_both_legs_is_never_an_opportunity() {
        let now = Instant::now();
        let one = priced(106.0, 200, Duration::ZERO, now);
        assert_eq!(evaluate(5_000_000, ("same", &one), ("same", &one), &costs(), now), Err(Decline::NotATrade));
    }

    #[test]
    fn the_reported_staleness_is_the_older_of_the_two_prices() {
        // A gap computed from a price that arrived a second ago may already be
        // gone, and the terminal has to be able to say so.
        let now = Instant::now();
        let dear = priced(106.0, 200, Duration::from_millis(900), now);
        let cheap = priced(105.0, 200, Duration::from_millis(20), now);
        let found = evaluate(5_000_000, ("dear", &dear), ("cheap", &cheap), &costs(), now)
            .expect("opportunity");
        assert!(found.staleness_ms >= 890 && found.staleness_ms <= 910, "{}", found.staleness_ms);
    }

    #[test]
    fn a_price_older_than_the_limit_is_not_traded_on() {
        // A frozen feed would otherwise keep answering with the last gap it
        // saw, confidently, forever.
        let now = Instant::now();
        let dear = priced(106.0, 200, Duration::from_secs(30), now);
        let cheap = priced(105.0, 200, Duration::ZERO, now);
        assert_eq!(evaluate(5_000_000, ("dear", &dear), ("cheap", &cheap), &costs(), now), Err(Decline::Stale));
    }

    #[test]
    fn an_implausible_gap_is_treated_as_bad_data_not_free_money() {
        // Two pools of one pair cannot be 100% apart. The usual cause is a pool
        // whose token order is reversed, which decodes to a reciprocal price.
        let now = Instant::now();
        let dear = priced(105.0, 200, Duration::ZERO, now);
        let cheap = priced(0.0095, 200, Duration::ZERO, now);
        assert_eq!(evaluate(5_000_000, ("dear", &dear), ("cheap", &cheap), &costs(), now), Err(Decline::Implausible));
    }

    #[test]
    fn a_zero_size_is_refused() {
        let now = Instant::now();
        let dear = priced(106.0, 200, Duration::ZERO, now);
        let cheap = priced(105.0, 200, Duration::ZERO, now);
        assert_eq!(evaluate(0, ("dear", &dear), ("cheap", &cheap), &costs(), now), Err(Decline::NotATrade));
    }
}

#[cfg(test)]
mod live_shape_tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::bot::prices::Priced;
    use crate::bot::whirlpool::PoolState;

    fn priced(price: f64, fee_rate: u32, now: Instant) -> Priced {
        Priced { state: PoolState { price, fee_rate }, slot: 1, at: now }
    }

    #[test]
    fn a_gap_the_live_run_actually_saw_is_acted_on() {
        // The live run reported a widest gap of 195 bps and zero actionable
        // findings in the same session, which cannot both be true unless
        // something rejects a gap that clears its costs many times over.
        let now = Instant::now();
        let cheap = 100.0f64;
        let dear = cheap * 1.0195;
        let costs = Costs {
            network_fee_lamports: 5_000,
            margin_bps: 5,
            max_price_age: Duration::from_secs(5),
        };
        let found = evaluate(
            5_000_000,
            ("orca 4bp", &priced(dear, 400, now)),
            ("orca 30bp", &priced(cheap, 3000, now)),
            &costs,
            now,
        );
        let found = found.expect("195 bps against 34 of cost was refused");
        assert_eq!(found.gross_bps, 195);
        assert!(found.net_lamports > 0);
    }
}
