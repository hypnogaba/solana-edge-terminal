//! One lane's bot: the same parts, told to look at a different moment.
//!
//! There is no network call in here. A trigger arrives, the bot reads the
//! shared price book and decides, in microseconds. Both lanes read the SAME
//! book, so when one acts 300 ms later than the other and the book has moved
//! on, that is the whole demonstration, not an artefact.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use solana_pubkey::Pubkey;

use crate::bot::arb::{evaluate, Costs, Opportunity};
use crate::bot::pools::PoolBook;
use crate::bot::prices::PriceBook;

#[derive(Debug, Clone)]
pub struct TriggerEvent {
    pub pool: Pubkey,
    pub signature: String,
    pub slot: u64,
    pub at: Instant,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct LaneStats {
    /// Triggers priced, after dropping repeats.
    pub seen: u64,
    /// Of those, the ones worth acting on.
    pub actionable: u64,
    /// Triggers dropped because that signature and pool had already been priced.
    pub duplicates: u64,
    /// Triggers where no sibling had a fresh price, so nothing was compared.
    pub no_price: u64,
    /// Gaps too wide to be real, which means bad reference data, not profit.
    pub implausible: u64,
    /// Triggers where the gap was there but smaller than the costs. This is
    /// the normal case and the terminal shows it, because a bot that never
    /// reports a rejection looks like one that is not really checking.
    pub below_cost: u64,
    /// The widest gap seen, taken or not. Without this the only way to tune
    /// the threshold is to guess at how close the market came.
    pub best_gap_bps: u64,
    /// The widest gap after the two pool fees are taken out of it. This is the
    /// number that means something: the session's widest raw gap was 197 bps
    /// on a pair whose two pools charge 216 between them, so the headline
    /// promised an opportunity that could not exist.
    pub best_net_bps: u64,
    /// Why the bot did not act, counted by reason. "It did not trade" is not
    /// an answer; this says whether the gap was too small, the price too old,
    /// or the trade too small to carry the network fee.
    pub declined: std::collections::BTreeMap<crate::bot::arb::Decline, u64>,
}

/// What a lane saw when it looked, whether or not it could trade on it.
///
/// The rejections matter as much as the trades here. Two pools of a liquid
/// pair sit a couple of basis points apart against several of fees, so the
/// honest answer is almost always "no trade" -- and the demonstration is that
/// the two lanes, looking at the same event a few hundred milliseconds apart,
/// do not see the same market.
#[derive(Debug, Clone)]
pub struct Found {
    /// The pool the trigger touched. Part of the identity of an event: a
    /// transaction can touch two watched pools, and the two lanes must be
    /// compared on the same one or the comparison means nothing.
    pub pool: Pubkey,
    pub pool_label: &'static str,
    /// The gap this lane could see, on the pools named by `route`.
    pub gap_bps: u64,
    /// Set only when that gap cleared both pools' fees and the network fee.
    pub opportunity: Option<Opportunity>,
    pub trigger_signature: String,
    pub slot: u64,
    pub decided_at: Instant,
    pub looked_at: Instant,
    pub pair_label: &'static str,
    pub route: String,
}

pub struct LaneBot {
    pub name: String,
    pools: Arc<PoolBook>,
    prices: Arc<PriceBook>,
    costs: Costs,
    size_lamports: u64,
    acted: HashSet<(String, Pubkey)>,
    stats: LaneStats,
}

impl LaneBot {
    pub fn new(
        name: &str,
        pools: Arc<PoolBook>,
        prices: Arc<PriceBook>,
        costs: Costs,
        size_lamports: u64,
    ) -> Self {
        Self {
            name: name.to_string(),
            pools,
            prices,
            costs,
            size_lamports,
            acted: HashSet::new(),
            stats: LaneStats::default(),
        }
    }

    pub fn stats(&self) -> LaneStats {
        self.stats.clone()
    }

    /// Judge one trigger against every sibling pool, keeping the best.
    ///
    /// Returns what this lane saw even when there is nothing to trade, because
    /// what it saw is the measurement.
    pub fn on_trigger(&mut self, event: &TriggerEvent, now: Instant) -> Option<Found> {
        if !self.acted.insert((event.signature.clone(), event.pool)) {
            self.stats.duplicates += 1;
            return None;
        }
        self.stats.seen += 1;

        let pool = self.pools.find(&event.pool)?;
        let Some(moved) = self.prices.get(&pool.address) else {
            self.stats.no_price += 1;
            return None;
        };

        let mut best: Option<(Opportunity, u64, String)> = None;
        let mut widest = 0u64;
        let mut widest_route = String::new();
        let mut compared = 0usize;
        for sibling in self.pools.siblings(pool) {
            let Some(other) = self.prices.get(&sibling.address) else {
                continue;
            };
            if !crate::bot::arb::fresh_enough(&moved, &other, now, self.costs.max_price_age) {
                continue;
            }
            compared += 1;
            let gap = crate::bot::arb::gap_bps(&moved, &other);
            if gap > widest && gap <= crate::bot::arb::IMPLAUSIBLE_BPS {
                widest = gap;
                widest_route = format!("{} | {}", pool.label, sibling.label);
            }
            if gap <= crate::bot::arb::IMPLAUSIBLE_BPS {
                self.stats.best_gap_bps = self.stats.best_gap_bps.max(gap);
                let fees = moved.state.fee_bps() + other.state.fee_bps();
                self.stats.best_net_bps =
                    self.stats.best_net_bps.max(gap.saturating_sub(fees));
            } else {
                self.stats.implausible += 1;
            }
            // Try it both ways round: the pool that just moved may now be the
            // dear side or the cheap one, and only the market decides which.
            for (sell, buy) in [
                ((pool.label, &moved), (sibling.label, &other)),
                ((sibling.label, &other), (pool.label, &moved)),
            ] {
                let route = format!("{} → {}", sell.0, buy.0);
                match evaluate(self.size_lamports, sell, buy, &self.costs, now) {
                    Err(why) => {
                        // Both directions are tried, and one of them is always
                        // backwards, so that one is not a finding about the
                        // market and is not counted.
                        if why != crate::bot::arb::Decline::WrongWayRound {
                            *self.stats.declined.entry(why).or_default() += 1;
                        }
                    }
                    Ok(found) => {
                        let better = best.as_ref().is_none_or(|(current, _, _)| {
                            found.net_lamports > current.net_lamports
                        });
                        if better {
                            best = Some((found, gap, route));
                        }
                    }
                }
            }
        }

        if compared == 0 {
            // Nothing to compare against: no sibling had a fresh price. That is
            // not "the gap was too small", and reporting it as one would put a
            // confident zero on the screen for a check that never happened.
            self.stats.no_price += 1;
            return None;
        }

        // Report the gap and the pools it was measured on together. Reporting
        // the widest gap beside the pools of a different, narrower trade is how
        // a screen ends up contradicting itself.
        let (opportunity, gap_bps, route) = match best {
            Some((found, gap, route)) => {
                self.stats.actionable += 1;
                (Some(found), gap, route)
            }
            None => {
                self.stats.below_cost += 1;
                (None, widest, widest_route)
            }
        };
        Some(Found {
            pool: pool.address,
            pool_label: pool.label,
            gap_bps,
            opportunity,
            trigger_signature: event.signature.clone(),
            slot: event.slot,
            decided_at: event.at,
            looked_at: now,
            pair_label: pool.pair,
            route,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::bot::pools::PoolBook;
    use crate::bot::prices::Priced;
    use crate::bot::whirlpool::PoolState;

    fn setup(prices: &[(usize, f64, u32)]) -> (LaneBot, Arc<PoolBook>, Instant) {
        let pools = Arc::new(PoolBook::demo());
        let book = Arc::new(PriceBook::default());
        let now = Instant::now();
        for (index, price, fee_rate) in prices {
            book.set(
                pools.all()[*index].address,
                Priced { state: PoolState { price: *price, fee_rate: *fee_rate }, slot: 1, at: now },
            );
        }
        let bot = LaneBot::new(
            "test",
            pools.clone(),
            book,
            Costs {
                network_fee_lamports: 5_000,
                margin_bps: 1,
                max_price_age: Duration::from_secs(5),
            },
            5_000_000,
        );
        (bot, pools, now)
    }

    fn event(pool: Pubkey, signature: &str, at: Instant) -> TriggerEvent {
        TriggerEvent { pool, signature: signature.to_string(), slot: 443_961_641, at }
    }

    #[test]
    fn the_widest_gap_is_reported_beside_what_is_left_of_it() {
        // A 197 bps gap between two pools charging 216 between them is not an
        // opportunity that got away, and a headline saying 197 reads as one.
        // 197 bps apart, and the two pools charge 100 and 200 between them.
        let (mut bot, pools, now) =
            setup(&[(0, 100.0 * 1.0197, 10_000), (1, 100.0, 20_000)]);
        bot.on_trigger(&event(pools.all()[0].address, "sig", now), now);
        let stats = bot.stats();
        assert_eq!(stats.best_gap_bps, 197);
        assert_eq!(stats.best_net_bps, 0, "nothing was left after 300 bps of fees");
        assert_eq!(stats.actionable, 0);
    }

    #[test]
    fn a_pool_that_moved_away_from_its_sibling_is_an_opportunity() {
        let (mut bot, pools, now) = setup(&[(0, 106.0, 200), (1, 105.0, 200)]);
        let found = bot
            .on_trigger(&event(pools.all()[0].address, "sig", now), now)
            .expect("looked");
        let taken = found.opportunity.expect("opportunity");
        assert_eq!(taken.sell_on, "orca 4bp");
        assert_eq!(taken.buy_on, "orca 2bp");
        assert_eq!(bot.stats().actionable, 1);
    }

    #[test]
    fn the_direction_is_whichever_way_the_market_actually_sits() {
        // The pool that moved is the cheap side here, so the bot must sell on
        // the sibling, not on the pool that triggered it.
        let (mut bot, pools, now) = setup(&[(0, 105.0, 200), (1, 106.0, 200)]);
        let taken = bot
            .on_trigger(&event(pools.all()[0].address, "sig", now), now)
            .expect("looked")
            .opportunity
            .expect("opportunity");
        assert_eq!(taken.sell_on, "orca 2bp");
        assert_eq!(taken.buy_on, "orca 4bp");
    }

    #[test]
    fn the_ordinary_market_is_reported_as_a_gap_with_no_trade() {
        // Two real prices, 2 bps apart, against 6 bps of fees. The lane still
        // reports what it saw: that number is the measurement, and a bot that
        // only ever reports trades looks like one that is not really checking.
        let (mut bot, pools, now) = setup(&[(0, 105.11738, 400), (1, 105.14158, 200)]);
        let found = bot
            .on_trigger(&event(pools.all()[0].address, "sig", now), now)
            .expect("looked");
        assert!(found.opportunity.is_none());
        assert_eq!(found.gap_bps, 2);
        assert_eq!(bot.stats().below_cost, 1);
        assert_eq!(bot.stats().actionable, 0);
    }

    #[test]
    fn the_widest_of_several_siblings_is_the_one_taken() {
        let (mut bot, pools, now) = setup(&[(0, 106.0, 200), (1, 105.5, 200), (2, 105.0, 200)]);
        let taken = bot
            .on_trigger(&event(pools.all()[0].address, "sig", now), now)
            .expect("looked")
            .opportunity
            .expect("opportunity");
        assert_eq!(taken.buy_on, "orca 5bp", "should take the cheapest sibling");
    }

    #[test]
    fn a_missing_price_is_counted_rather_than_guessed() {
        let (mut bot, pools, now) = setup(&[]);
        assert!(bot.on_trigger(&event(pools.all()[0].address, "sig", now), now).is_none());
        assert_eq!(bot.stats().no_price, 1);
    }

    #[test]
    fn a_trigger_with_no_comparable_sibling_reports_nothing_at_all() {
        // Only the pool that moved is priced. Returning a zero gap here would
        // put "we checked, the market was tight" on screen for a check that
        // never happened.
        let (mut bot, pools, now) = setup(&[(0, 106.0, 200)]);
        assert!(bot.on_trigger(&event(pools.all()[0].address, "sig", now), now).is_none());
        assert_eq!(bot.stats().no_price, 1);
        assert_eq!(bot.stats().below_cost, 0);
    }

    #[test]
    fn the_gap_and_the_pools_it_was_measured_on_always_agree() {
        // Three pools: the widest gap is against one sibling, the best trade
        // may be against another. The row must not mix them.
        let (mut bot, pools, now) = setup(&[(0, 106.0, 200), (1, 105.5, 200), (2, 105.0, 200)]);
        let found = bot
            .on_trigger(&event(pools.all()[0].address, "sig", now), now)
            .expect("looked");
        let taken = found.opportunity.expect("opportunity");
        assert!(found.route.contains(taken.buy_on), "route names the pools traded");
        assert_eq!(found.gap_bps, taken.gross_bps);
    }

    #[test]
    fn the_same_signature_on_a_different_pool_is_a_different_event() {
        // A router transaction touches two watched pools. Each is its own
        // comparison, and dropping the second as a duplicate would leave the
        // two lanes measuring different things.
        let (mut bot, pools, now) = setup(&[(0, 106.0, 200), (1, 105.0, 200)]);
        assert!(bot.on_trigger(&event(pools.all()[0].address, "sig", now), now).is_some());
        assert!(bot.on_trigger(&event(pools.all()[1].address, "sig", now), now).is_some());
        assert_eq!(bot.stats().seen, 2);
        assert_eq!(bot.stats().duplicates, 0);
    }

    #[test]
    fn a_price_that_went_stale_stops_being_compared() {
        let pools = Arc::new(PoolBook::demo());
        let book = Arc::new(PriceBook::default());
        let now = Instant::now();
        book.set(pools.all()[0].address, Priced {
            state: PoolState { price: 106.0, fee_rate: 200 }, slot: 1, at: now,
        });
        book.set(pools.all()[1].address, Priced {
            state: PoolState { price: 105.0, fee_rate: 200 },
            slot: 1,
            at: now - Duration::from_secs(60),
        });
        let mut bot = LaneBot::new("test", pools.clone(), book, Costs {
            network_fee_lamports: 5_000, margin_bps: 1, max_price_age: Duration::from_secs(5),
        }, 5_000_000);
        assert!(bot.on_trigger(&event(pools.all()[0].address, "sig", now), now).is_none());
        assert_eq!(bot.stats().no_price, 1);
    }

    #[test]
    fn the_same_signature_is_only_priced_once() {
        let (mut bot, pools, now) = setup(&[(0, 106.0, 200), (1, 105.0, 200)]);
        let pool = pools.all()[0].address;
        assert!(bot.on_trigger(&event(pool, "sig", now), now).is_some());
        assert!(bot.on_trigger(&event(pool, "sig", now), now).is_none());
        assert_eq!(bot.stats().seen, 1);
        assert_eq!(bot.stats().duplicates, 1);
    }

    #[test]
    fn a_later_look_at_a_moved_book_finds_nothing_and_that_is_the_point() {
        // The slow lane's trigger arrives after the price has caught up. This
        // is exactly what the demo shows, so it gets a test.
        let (mut bot, pools, now) = setup(&[(0, 106.0, 200), (1, 105.0, 200)]);
        let pool = pools.all()[0].address;
        let early = bot.on_trigger(&event(pool, "early", now), now).expect("looked");
        assert!(early.opportunity.is_some());

        bot.prices.set(
            pools.all()[1].address,
            Priced {
                state: PoolState { price: 106.0, fee_rate: 200 },
                slot: 2,
                at: now + Duration::from_millis(300),
            },
        );
        let late = bot
            .on_trigger(&event(pool, "late", now + Duration::from_millis(300)), now)
            .expect("looked");
        assert!(late.opportunity.is_none(), "the gap closed while the slow lane waited");
        assert!(late.gap_bps < early.gap_bps, "and the gap it saw is narrower");
    }
}
