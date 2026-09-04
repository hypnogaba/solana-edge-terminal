//! Every attempt the bot makes, and what it was worth.
//!
//! The counters said how many opportunities were offered and refused, which
//! answers "is it working" and not "what did it make". A trading screen has to
//! answer the second one, and it has to answer it without ever letting an
//! expected number pass for a realised one: in a dry run the bot books nothing
//! at all, and a figure that does not say so is a lie with a decimal point.

use std::collections::VecDeque;

use serde::Serialize;

/// How much of a trade is real. A dry run produces expectations and nothing
/// else; only a sent transaction produces money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Booking {
    /// Decided, not sent. The number is what it would have made.
    Expected,
    /// Built and put to the cluster, which said it would work.
    Simulated,
    /// Sent. The number is money.
    Realised,
    /// Refused or failed before it could be worth anything.
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trade {
    /// Seconds since the session started, so a viewer can see the pace.
    pub at_s: f64,
    pub slot: u64,
    pub lane: String,
    pub pair: &'static str,
    pub route: String,
    pub size_lamports: u64,
    pub gross_bps: u64,
    /// After both pool fees and the network fee.
    pub net_lamports: u64,
    pub booking: Booking,
    /// What happened, in the words the screen shows.
    pub outcome: String,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct Totals {
    pub attempts: u64,
    /// What the bot would have made had every accepted opportunity been sent.
    pub expected_lamports: u64,
    /// What it actually sent, and what that came to.
    pub sent: u64,
    pub realised_lamports: u64,
    pub refused: u64,
    /// The best single opportunity seen, expected or realised.
    pub best_lamports: u64,
}

const KEPT: usize = 200;

#[derive(Debug, Default)]
pub struct Ledger {
    trades: VecDeque<Trade>,
    totals: Totals,
}

impl Ledger {
    pub fn record(&mut self, trade: Trade) {
        self.totals.attempts += 1;
        self.totals.best_lamports = self.totals.best_lamports.max(trade.net_lamports);
        match trade.booking {
            Booking::Expected | Booking::Simulated => {
                self.totals.expected_lamports += trade.net_lamports;
            }
            Booking::Realised => {
                self.totals.sent += 1;
                self.totals.realised_lamports += trade.net_lamports;
                // A sent trade was also expected to make this, so the expected
                // figure stays the full story of what the bot found.
                self.totals.expected_lamports += trade.net_lamports;
            }
            Booking::None => self.totals.refused += 1,
        }
        self.trades.push_front(trade);
        self.trades.truncate(KEPT);
    }

    pub fn totals(&self) -> Totals {
        self.totals
    }

    pub fn recent(&self) -> Vec<Trade> {
        self.trades.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(net: u64, booking: Booking) -> Trade {
        Trade {
            at_s: 1.0,
            slot: 1,
            lane: "doublezero".into(),
            pair: "SOL/USDC",
            route: "orca 2bp -> orca 5bp".into(),
            size_lamports: 5_000_000,
            gross_bps: 12,
            net_lamports: net,
            booking,
            outcome: "dry run".into(),
        }
    }

    #[test]
    fn a_dry_run_books_nothing_but_still_reports_what_it_found() {
        // The whole point: the bot has to be able to say "this is what I would
        // have made" without that number appearing anywhere as earnings.
        let mut ledger = Ledger::default();
        ledger.record(trade(4_000, Booking::Expected));
        ledger.record(trade(6_000, Booking::Expected));
        let totals = ledger.totals();
        assert_eq!(totals.expected_lamports, 10_000);
        assert_eq!(totals.realised_lamports, 0, "a dry run earns nothing");
        assert_eq!(totals.sent, 0);
    }

    #[test]
    fn only_a_sent_trade_counts_as_realised() {
        let mut ledger = Ledger::default();
        ledger.record(trade(7_000, Booking::Simulated));
        assert_eq!(ledger.totals().realised_lamports, 0, "a simulation is not money");
        ledger.record(trade(3_000, Booking::Realised));
        assert_eq!(ledger.totals().realised_lamports, 3_000);
        assert_eq!(ledger.totals().sent, 1);
    }

    #[test]
    fn a_refusal_is_recorded_without_being_counted_as_a_find() {
        // A refused attempt still belongs on the screen, because "the limits
        // stopped it" is the answer to a question a viewer will ask.
        let mut ledger = Ledger::default();
        ledger.record(trade(9_000, Booking::None));
        let totals = ledger.totals();
        assert_eq!(totals.refused, 1);
        assert_eq!(totals.expected_lamports, 0);
        assert_eq!(ledger.recent().len(), 1);
    }

    #[test]
    fn the_newest_trade_is_first_and_the_ledger_stays_bounded() {
        let mut ledger = Ledger::default();
        for slot in 0..(KEPT as u64 + 40) {
            let mut one = trade(1, Booking::Expected);
            one.slot = slot;
            ledger.record(one);
        }
        let recent = ledger.recent();
        assert_eq!(recent.len(), KEPT);
        assert_eq!(recent[0].slot, KEPT as u64 + 39);
        assert_eq!(ledger.totals().attempts, KEPT as u64 + 40);
    }

    #[test]
    fn the_best_single_find_survives_the_window() {
        // The ledger keeps 200 rows; the widest thing the bot ever saw has to
        // outlive them, or the session's headline number quietly resets.
        let mut ledger = Ledger::default();
        ledger.record(trade(90_000, Booking::Expected));
        for _ in 0..KEPT + 10 {
            ledger.record(trade(10, Booking::Expected));
        }
        assert_eq!(ledger.totals().best_lamports, 90_000);
    }
}
