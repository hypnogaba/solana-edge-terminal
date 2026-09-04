//! The measurement: the same transaction, seen on two paths, on one clock.
//!
//! Every lane runs in this one process on this one host, so a delta is a real
//! difference in arrival and not clock skew between machines. Transactions are
//! matched by signature, which is the network's own identity for them and is
//! identical on every path, so nothing has to be inferred from timing.
//!
//! A lane is credited only with what it actually delivered. A transaction one
//! lane never reported is not a win for the other: it drops out of the
//! comparison and is counted separately, because a feed that is fast but lossy
//! must not be able to hide the loss inside a latency number.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::pipeline::SeenTx;

/// The lane everything else is measured against. Configurable, because the
/// machinery can be proven on two ordinary RPC lanes before the shred feed
/// exists, and that dry run is worth more than a mock.
pub const DEFAULT_REFERENCE_LANE: &str = "doublezero";

/// How long to wait for the slower side before giving up on a signature.
const PENDING_TTL: Duration = Duration::from_secs(60);
const RECENT_KEPT: usize = 40;

#[derive(Debug, Clone, Serialize)]
pub struct MatchedTx {
    pub signature: String,
    pub slot: u64,
    pub lane: String,
    /// Milliseconds the reference lane arrived ahead of `lane`. Negative means
    /// the reference lane was the slower of the two.
    pub lead_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaneStats {
    pub lane: String,
    pub seen: u64,
    pub matched: u64,
    pub n_window: usize,
    pub median_lead_ms: Option<f64>,
    pub p10_lead_ms: Option<f64>,
    pub p90_lead_ms: Option<f64>,
    /// Share of matched transactions the reference lane delivered first.
    pub reference_first_pct: Option<f64>,
    /// The lead distribution, following BlockRazor's shred-stats, which is the
    /// only published benchmark of one shred stream against another. A single
    /// median hides the tail, and the tail is what decides whether a bot
    /// actually wins: a feed that is 40 ms ahead half the time and behind at
    /// P10 loses the races that pay.
    pub quantiles: Vec<Quantile>,
    /// The same ladder over the cases where the reference arrived first, which
    /// is how shred-stats defines lead time. Kept separate because mixing the
    /// losses in flatters the number.
    pub led_quantiles: Vec<Quantile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Quantile {
    /// "P50", and so on, as it is written in the field.
    pub at: &'static str,
    pub lead_ms: f64,
}

/// The ladder shred-stats publishes.
const LADDER: [(&str, f64); 10] = [
    ("P1", 0.01), ("P5", 0.05), ("P10", 0.10), ("P25", 0.25), ("P50", 0.50),
    ("P75", 0.75), ("P80", 0.80), ("P90", 0.90), ("P95", 0.95), ("P99", 0.99),
];

/// `sorted` must already be in order.
fn ladder(sorted: &[f64]) -> Vec<Quantile> {
    LADDER
        .iter()
        .filter_map(|(at, q)| {
            percentile(sorted, *q).map(|lead_ms| Quantile { at, lead_ms })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct RaceSnapshot {
    pub uptime_s: f64,
    pub window_s: f64,
    pub reference_lane: String,
    pub reference_seen: u64,
    pub awaiting_pair: usize,
    pub lanes: Vec<LaneStats>,
    pub recent: Vec<MatchedTx>,
}

struct Pending {
    slot: u64,
    arrivals: HashMap<String, Instant>,
    paired: HashSet<String>,
    created: Instant,
}

pub struct Race {
    reference: String,
    window: Duration,
    pending: HashMap<String, Pending>,
    deltas: VecDeque<(Instant, String, f64)>,
    recent: VecDeque<MatchedTx>,
    seen: HashMap<String, u64>,
    matched: HashMap<String, u64>,
    started: Instant,
}

/// Signed milliseconds from `reference` to `other`. Positive means the
/// reference lane was there first. `Instant` differences cannot be negative, so
/// the sign has to be reconstructed by comparing first.
fn lead_ms(reference: Instant, other: Instant) -> f64 {
    if other >= reference {
        other.duration_since(reference).as_secs_f64() * 1000.0
    } else {
        -(reference.duration_since(other).as_secs_f64() * 1000.0)
    }
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    Some(sorted[index.min(sorted.len() - 1)])
}

impl Race {
    pub fn new(window: Duration, reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            window,
            pending: HashMap::new(),
            deltas: VecDeque::new(),
            recent: VecDeque::new(),
            seen: HashMap::new(),
            matched: HashMap::new(),
            started: Instant::now(),
        }
    }

    /// Record one arrival. Returns the pairs it completed, if any.
    pub fn observe(&mut self, lane: &str, tx: &SeenTx) -> Vec<MatchedTx> {
        *self.seen.entry(lane.to_string()).or_default() += 1;
        let reference = self.reference.clone();

        let entry = self.pending.entry(tx.signature.clone()).or_insert_with(|| Pending {
            slot: tx.slot,
            arrivals: HashMap::new(),
            paired: HashSet::new(),
            created: tx.at,
        });
        // First arrival on a lane wins. A feed that repeats a transaction must
        // not be able to improve its own number with the second copy.
        entry.arrivals.entry(lane.to_string()).or_insert(tx.at);

        let Some(&reference_at) = entry.arrivals.get(reference.as_str()) else {
            return Vec::new();
        };
        let slot = entry.slot;
        let ready: Vec<(String, Instant)> = entry
            .arrivals
            .iter()
            .filter(|(name, _)| name.as_str() != reference)
            .filter(|(name, _)| !entry.paired.contains(name.as_str()))
            .map(|(name, at)| (name.clone(), *at))
            .collect();
        for (name, _) in &ready {
            entry.paired.insert(name.clone());
        }

        let now = Instant::now();
        let mut completed = Vec::new();
        for (name, at) in ready {
            let matched = MatchedTx {
                signature: tx.signature.clone(),
                slot,
                lane: name.clone(),
                lead_ms: lead_ms(reference_at, at),
            };
            *self.matched.entry(name.clone()).or_default() += 1;
            self.deltas.push_back((now, name, matched.lead_ms));
            self.recent.push_back(matched.clone());
            if self.recent.len() > RECENT_KEPT {
                self.recent.pop_front();
            }
            completed.push(matched);
        }
        completed
    }

    /// Drop signatures the slower side never delivered, and stale window data.
    pub fn evict(&mut self) {
        let now = Instant::now();
        self.pending
            .retain(|_, pending| now.saturating_duration_since(pending.created) < PENDING_TTL);
        while let Some((at, _, _)) = self.deltas.front() {
            if now.saturating_duration_since(*at) > self.window {
                self.deltas.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn snapshot(&self) -> RaceSnapshot {
        let mut by_lane: HashMap<&str, Vec<f64>> = HashMap::new();
        for (_, lane, delta) in &self.deltas {
            by_lane.entry(lane.as_str()).or_default().push(*delta);
        }
        let mut lanes: Vec<LaneStats> = self
            .seen
            .keys()
            .filter(|lane| lane.as_str() != self.reference)
            .map(|lane| {
                let mut deltas = by_lane.remove(lane.as_str()).unwrap_or_default();
                deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let n = deltas.len();
                let ahead = deltas.iter().filter(|d| **d > 0.0).count();
                LaneStats {
                    lane: lane.clone(),
                    seen: self.seen.get(lane).copied().unwrap_or(0),
                    matched: self.matched.get(lane).copied().unwrap_or(0),
                    n_window: n,
                    median_lead_ms: percentile(&deltas, 0.50),
                    p10_lead_ms: percentile(&deltas, 0.10),
                    p90_lead_ms: percentile(&deltas, 0.90),
                    reference_first_pct: (n > 0)
                        .then(|| (100.0 * ahead as f64 / n as f64 * 10.0).round() / 10.0),
                    quantiles: ladder(&deltas),
                    led_quantiles: {
                        let led: Vec<f64> =
                            deltas.iter().copied().filter(|d| *d > 0.0).collect();
                        ladder(&led)
                    },
                }
            })
            .collect();
        lanes.sort_by(|a, b| a.lane.cmp(&b.lane));

        RaceSnapshot {
            uptime_s: (Instant::now().saturating_duration_since(self.started).as_secs_f64()
                * 10.0)
                .round()
                / 10.0,
            window_s: self.window.as_secs_f64(),
            reference_lane: self.reference.clone(),
            reference_seen: self.seen.get(self.reference.as_str()).copied().unwrap_or(0),
            awaiting_pair: self.pending.len(),
            lanes,
            recent: self.recent.iter().rev().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(signature: &str, at: Instant) -> SeenTx {
        SeenTx { signature: signature.to_string(), slot: 42, at, account_keys: Vec::new() }
    }

    fn race() -> (Race, Instant) {
        (Race::new(Duration::from_secs(3600), REFERENCE_LANE), Instant::now())
    }

    const REFERENCE_LANE: &str = DEFAULT_REFERENCE_LANE;

    #[test]
    fn the_lead_is_signed_from_the_reference_lane() {
        let (mut race, t0) = race();
        race.observe(REFERENCE_LANE, &tx("sig", t0));
        let matched = race.observe("public", &tx("sig", t0 + Duration::from_millis(7)));
        assert_eq!(matched.len(), 1);
        assert!((matched[0].lead_ms - 7.0).abs() < 1e-6);
    }

    #[test]
    fn the_reference_lane_losing_shows_as_a_negative_lead() {
        // It must be possible for the feed to lose, or the number means nothing.
        let (mut race, t0) = race();
        race.observe("public", &tx("sig", t0));
        let matched = race.observe(REFERENCE_LANE, &tx("sig", t0 + Duration::from_millis(3)));
        assert_eq!(matched.len(), 1);
        assert!((matched[0].lead_ms + 3.0).abs() < 1e-6, "got {}", matched[0].lead_ms);
    }

    #[test]
    fn order_of_arrival_does_not_change_the_delta() {
        let (mut race_a, t0) = race();
        race_a.observe(REFERENCE_LANE, &tx("sig", t0));
        let a = race_a.observe("public", &tx("sig", t0 + Duration::from_millis(5)))[0].lead_ms;

        let (mut race_b, t0) = race();
        race_b.observe("public", &tx("sig", t0 + Duration::from_millis(5)));
        let b = race_b.observe(REFERENCE_LANE, &tx("sig", t0))[0].lead_ms;
        assert_eq!(a, b);
    }

    #[test]
    fn a_lane_is_paired_at_most_once_per_transaction() {
        let (mut race, t0) = race();
        race.observe(REFERENCE_LANE, &tx("sig", t0));
        assert_eq!(race.observe("public", &tx("sig", t0 + Duration::from_millis(9))).len(), 1);
        assert!(
            race.observe("public", &tx("sig", t0 + Duration::from_millis(1))).is_empty(),
            "a repeated copy must not be able to improve the lane's own number"
        );
        assert_eq!(race.snapshot().lanes[0].matched, 1);
    }

    #[test]
    fn a_transaction_only_one_lane_delivered_is_not_a_win() {
        let (mut race, t0) = race();
        race.observe(REFERENCE_LANE, &tx("only-on-the-feed", t0));
        let snapshot = race.snapshot();
        assert_eq!(snapshot.reference_seen, 1);
        assert!(snapshot.lanes.is_empty(), "nothing to compare against yet");
        assert_eq!(snapshot.awaiting_pair, 1);
    }

    #[test]
    fn two_lanes_are_each_measured_against_the_reference() {
        let (mut race, t0) = race();
        race.observe(REFERENCE_LANE, &tx("sig", t0));
        race.observe("public", &tx("sig", t0 + Duration::from_millis(30)));
        race.observe("commercial", &tx("sig", t0 + Duration::from_millis(8)));
        let snapshot = race.snapshot();
        let commercial = snapshot.lanes.iter().find(|l| l.lane == "commercial").unwrap();
        let public = snapshot.lanes.iter().find(|l| l.lane == "public").unwrap();
        assert!((commercial.median_lead_ms.unwrap() - 8.0).abs() < 1e-6);
        assert!((public.median_lead_ms.unwrap() - 30.0).abs() < 1e-6);
    }

    #[test]
    fn the_win_rate_counts_only_matched_transactions() {
        let (mut race, t0) = race();
        // Two where the feed was ahead, one where it was behind.
        for (index, offset) in [5i64, 4, -2].into_iter().enumerate() {
            let signature = format!("sig{index}");
            race.observe(REFERENCE_LANE, &tx(&signature, t0));
            let at = if offset >= 0 {
                t0 + Duration::from_millis(offset as u64)
            } else {
                t0 - Duration::from_millis(offset.unsigned_abs())
            };
            race.observe("public", &tx(&signature, at));
        }
        let stats = &race.snapshot().lanes[0];
        assert_eq!(stats.n_window, 3);
        assert_eq!(stats.reference_first_pct, Some(66.7));
        // The ladder is what this measurement is reported as in the field.
        let names: Vec<&str> = stats.quantiles.iter().map(|q| q.at).collect();
        assert_eq!(names, ["P1", "P5", "P10", "P25", "P50", "P75", "P80", "P90", "P95", "P99"]);
        // Lead time counts only the races the reference won, so its ladder is
        // strictly above zero even when the full one is not.
        assert!(
            stats.led_quantiles.iter().all(|q| q.lead_ms > 0.0),
            "a lead ladder may not contain a loss: {:?}",
            stats.led_quantiles
        );
        assert!(
            stats.quantiles[0].lead_ms <= stats.quantiles[9].lead_ms,
            "the ladder must be ordered"
        );
    }

    #[test]
    fn stale_pending_signatures_are_evicted() {
        let mut race = Race::new(Duration::from_secs(3600), REFERENCE_LANE);
        let old = Instant::now() - PENDING_TTL - Duration::from_secs(1);
        race.observe(REFERENCE_LANE, &tx("forgotten", old));
        assert_eq!(race.snapshot().awaiting_pair, 1);
        race.evict();
        assert_eq!(race.snapshot().awaiting_pair, 0);
    }
}
