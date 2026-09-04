//! One opportunity, as found by each lane.
//!
//! Paired on the trigger transaction's signature, which is the same on every
//! feed. A lane that never found an opportunity is not scored against it: that
//! is a miss, reported as such, never a win for the other side.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::bot::lane::Found;

/// How many events the tape holds. A wide screen shows well over forty rows,
/// and a tape that runs out halfway down reads as a stalled feed rather than a
/// quiet market.
const RECENT_KEPT: usize = 250;

/// What each lane saw of one event.
#[derive(Debug, Clone, Serialize)]
pub struct LaneView {
    pub lane: String,
    /// The widest gap that lane could see at the moment it looked.
    pub gap_bps: u64,
    /// Set only when the gap cleared every cost.
    pub tradeable: bool,
    pub net_lamports: u64,
    pub route: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Duel {
    pub signature: String,
    pub slot: u64,
    pub pair: String,
    /// The pool that moved. Part of the event's identity.
    pub pool: String,
    /// Milliseconds the first lane was ahead. None until both have reported.
    pub head_start_ms: Option<f64>,
    pub first: Option<String>,
    pub views: Vec<LaneView>,
    /// How much narrower the gap had become by the time the slower lane
    /// looked. This is the demonstration: same event, different market.
    pub gap_lost_bps: Option<i64>,
}

struct Entry {
    duel: Duel,
    times: HashMap<String, Instant>,
    created: Instant,
}

impl Entry {
    /// Recompute the head start and what the wait cost, from the views held.
    fn settle(&mut self) {
        if self.times.len() < 2 {
            return;
        }
        let mut sorted: Vec<(&String, &Instant)> = self.times.iter().collect();
        sorted.sort_by_key(|(_, at)| **at);
        let (first_lane, first_at) = sorted[0];
        let (second_lane, second_at) = sorted[1];
        self.duel.first = Some(first_lane.clone());
        self.duel.head_start_ms =
            Some(second_at.saturating_duration_since(*first_at).as_secs_f64() * 1000.0);

        let gap_of = |name: &str| {
            self.duel.views.iter().find(|view| view.lane == name).map(|view| view.gap_bps as i64)
        };
        if let (Some(early), Some(late)) = (gap_of(first_lane), gap_of(second_lane)) {
            self.duel.gap_lost_bps = Some(early - late);
        }
    }
}

pub struct DuelBook {
    ttl: Duration,
    entries: HashMap<String, Entry>,
    order: Vec<String>,
}

impl DuelBook {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, entries: HashMap::new(), order: Vec::new() }
    }

    pub fn record(&mut self, lane: &str, found: &Found) {
        // Identity is the transaction AND the pool. One transaction can touch
        // two watched pools, and each is a separate comparison; keying on the
        // signature alone would let the two lanes be scored against each other
        // on two different markets.
        let key = format!("{}|{}", found.trigger_signature, found.pool);
        if !self.entries.contains_key(&key) {
            self.order.push(key.clone());
        }
        let entry = self.entries.entry(key).or_insert_with(|| Entry {
            duel: Duel {
                signature: found.trigger_signature.clone(),
                slot: found.slot,
                pair: found.pair_label.to_string(),
                pool: found.pool_label.to_string(),
                head_start_ms: None,
                first: None,
                views: Vec::new(),
                gap_lost_bps: None,
            },
            times: HashMap::new(),
            created: found.decided_at,
        });

        // First report per lane wins: a repeat must not improve a lane's time
        // or replace what it saw with a later, different view.
        if entry.times.contains_key(lane) {
            return;
        }
        // Timed by when the feed delivered it, not by when this process got
        // round to looking: both lanes share one loop, so processing order
        // would measure our own scheduling rather than the feeds.
        entry.times.insert(lane.to_string(), found.decided_at);
        entry.duel.views.push(LaneView {
            lane: lane.to_string(),
            gap_bps: found.gap_bps,
            tradeable: found.opportunity.is_some(),
            net_lamports: found.opportunity.map(|o| o.net_lamports).unwrap_or(0),
            route: found.route.clone(),
        });
        entry.settle();
    }

    pub fn evict(&mut self) {
        let now = Instant::now();
        let ttl = self.ttl;
        self.entries.retain(|_, entry| now.saturating_duration_since(entry.created) < ttl);
        self.order.retain(|key| self.entries.contains_key(key));
    }

    /// Newest first.
    pub fn recent(&self) -> Vec<Duel> {
        self.order
            .iter()
            .rev()
            .filter_map(|key| self.entries.get(key).map(|entry| entry.duel.clone()))
            .take(RECENT_KEPT)
            .collect()
    }

    /// How often each lane got there first, over everything still in the book.
    pub fn wins(&self) -> HashMap<String, u64> {
        let mut wins: HashMap<String, u64> = HashMap::new();
        for entry in self.entries.values() {
            if let Some(first) = &entry.duel.first {
                *wins.entry(first.clone()).or_default() += 1;
            }
        }
        wins
    }

    pub fn paired(&self) -> usize {
        self.entries.values().filter(|e| e.times.len() >= 2).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::arb::Opportunity;

    /// One fixed pool, so two calls with the same signature are the same event.
    fn pool() -> solana_pubkey::Pubkey {
        solana_pubkey::Pubkey::new_from_array([7u8; 32])
    }

    fn found_with(signature: &str, at: Instant, gap_bps: u64, tradeable: bool) -> Found {
        Found {
            pool: pool(),
            pool_label: "orca 4bp",
            gap_bps,
            opportunity: tradeable.then_some(Opportunity {
                sell_on: "orca 4bp",
                buy_on: "orca 2bp",
                in_lamports: 5_000_000,
                gross_bps: gap_bps,
                net_lamports: 45_000,
                staleness_ms: 12,
            }),
            trigger_signature: signature.to_string(),
            slot: 443_961_641,
            decided_at: at,
            looked_at: at,
            pair_label: "SOL/USDC",
            route: "orca 4bp→orca 2bp".to_string(),
        }
    }

    fn found(signature: &str, at: Instant) -> Found {
        found_with(signature, at, 24, true)
    }

    #[test]
    fn the_head_start_is_the_gap_between_the_two_decisions() {
        let mut book = DuelBook::new(Duration::from_secs(600));
        let t0 = Instant::now();
        book.record("doublezero", &found("sig", t0));
        book.record("public", &found("sig", t0 + Duration::from_millis(318)));
        let duel = &book.recent()[0];
        assert_eq!(duel.head_start_ms.map(|v| v.round()), Some(318.0));
        assert_eq!(duel.first.as_deref(), Some("doublezero"));
        assert_eq!(book.paired(), 1);
    }

    #[test]
    fn the_gap_the_slow_lane_lost_is_the_point_of_the_whole_thing() {
        // Same event: the early lane sees 27 basis points, the lane that looks
        // 300 ms later sees 3, because the market moved in between.
        let mut book = DuelBook::new(Duration::from_secs(600));
        let t0 = Instant::now();
        book.record("doublezero", &found_with("sig", t0, 27, true));
        book.record("public", &found_with("sig", t0 + Duration::from_millis(300), 3, false));
        let duel = &book.recent()[0];
        assert_eq!(duel.gap_lost_bps, Some(24));
        assert_eq!(duel.views.len(), 2);
        assert!(duel.views.iter().find(|v| v.lane == "doublezero").unwrap().tradeable);
        assert!(!duel.views.iter().find(|v| v.lane == "public").unwrap().tradeable);
    }

    #[test]
    fn the_same_transaction_on_two_pools_is_two_events() {
        // A router transaction touching two watched pools is two separate
        // comparisons. Merging them would score the lanes against each other
        // on two different markets.
        let mut book = DuelBook::new(Duration::from_secs(600));
        let t0 = Instant::now();
        let mut one = found("sig", t0);
        let mut two = found("sig", t0);
        two.pool = solana_pubkey::Pubkey::new_from_array([9u8; 32]);
        one.pool = pool();
        book.record("doublezero", &one);
        book.record("doublezero", &two);
        assert_eq!(book.recent().len(), 2);
    }

    #[test]
    fn an_opportunity_only_one_lane_found_has_no_head_start() {
        // The other lane may still be about to find it, or may never. Inventing
        // a head start here would flatter whichever lane reported first.
        let mut book = DuelBook::new(Duration::from_secs(600));
        book.record("doublezero", &found("sig", Instant::now()));
        assert_eq!(book.recent()[0].head_start_ms, None);
        assert_eq!(book.recent()[0].gap_lost_bps, None);
        assert_eq!(book.paired(), 0);
    }

    #[test]
    fn a_lane_reporting_twice_does_not_improve_its_own_time() {
        let mut book = DuelBook::new(Duration::from_secs(600));
        let t0 = Instant::now();
        book.record("doublezero", &found("sig", t0 + Duration::from_millis(50)));
        book.record("doublezero", &found("sig", t0));
        book.record("public", &found("sig", t0 + Duration::from_millis(150)));
        assert_eq!(book.recent()[0].head_start_ms.map(|v| v.round()), Some(100.0));
    }

    #[test]
    fn the_slower_lane_can_win_and_it_shows() {
        let mut book = DuelBook::new(Duration::from_secs(600));
        let t0 = Instant::now();
        book.record("public", &found("sig", t0));
        book.record("doublezero", &found("sig", t0 + Duration::from_millis(20)));
        assert_eq!(book.recent()[0].first.as_deref(), Some("public"));
        assert_eq!(book.wins().get("public"), Some(&1));
    }

    #[test]
    fn stale_duels_leave_the_book_in_order() {
        let mut book = DuelBook::new(Duration::from_millis(1));
        book.record("doublezero", &found("old", Instant::now() - Duration::from_secs(5)));
        book.record("doublezero", &found("new", Instant::now()));
        book.evict();
        let recent = book.recent();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].signature, "new");
    }
}
