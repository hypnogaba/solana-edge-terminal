//! The gate every attempt passes before any money moves.
//!
//! Deliberately separate from both the strategy and the transaction building.
//! The strategy proposes and knows nothing about caps; the builder knows how to
//! make a transaction and nothing about whether it is allowed. This decides,
//! and it is the only place that counts what has been spent.
//!
//! Dry run is the default and costs nothing against the cap, so the terminal
//! can be watched for a day before anything is sent.

use crate::bot::arb::Opportunity;
use crate::bot::config::{Allowed, BotLimits, Refusal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Decide and record, send nothing.
    DryRun,
    /// Build and sign, then ask the cluster what would happen, still sending
    /// nothing. This is what proves the transaction is well formed.
    Simulate,
    /// Send it.
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    DryRun,
    Limit(Refusal),
    /// The transaction could not be built. Counted, never retried in a loop.
    BuildFailed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Skipped(Reason),
    /// Built and signed, and the cluster was asked what would happen.
    Simulated { ok: bool, note: String },
    Sent { signature: String },
}

/// What a broker does with an opportunity. Behind a trait so every decision
/// above it can be tested without a network or a wallet.
pub trait Broker: Send + Sync {
    fn execute(&self, opportunity: &Opportunity, mode: Mode) -> Outcome;
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct ExecutionStats {
    /// Opportunities handed to the executor.
    pub offered: u64,
    /// Refused by the limits, with the reason counted separately below.
    pub refused: u64,
    pub refused_killed: u64,
    pub refused_cap: u64,
    pub refused_wallet: u64,
    /// Reached the broker.
    pub attempted: u64,
    pub simulated_ok: u64,
    pub simulated_failed: u64,
    pub build_failed: u64,
    pub sent: u64,
    /// Lamports committed this session. Dry runs do not count.
    pub spent: u64,
}

pub struct Executor {
    pub lane: String,
    limits: BotLimits,
    mode: Mode,
    stats: ExecutionStats,
}

impl Executor {
    pub fn new(lane: &str, limits: BotLimits, mode: Mode) -> Self {
        Self { lane: lane.to_string(), limits, mode, stats: ExecutionStats::default() }
    }

    pub fn stats(&self) -> ExecutionStats {
        self.stats
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Check, then act. `balance` is the wallet's current lamports; pass 0 when
    /// there is no wallet, which is the state the demo runs in.
    pub fn attempt(
        &mut self,
        opportunity: &Opportunity,
        balance: u64,
        broker: Option<&dyn Broker>,
    ) -> Outcome {
        self.stats.offered += 1;

        if let Allowed::No(refusal) = self.limits.check(self.stats.spent, balance) {
            self.stats.refused += 1;
            match refusal {
                Refusal::Killed => self.stats.refused_killed += 1,
                Refusal::SessionCap => self.stats.refused_cap += 1,
                Refusal::WalletTooLarge => self.stats.refused_wallet += 1,
            }
            return Outcome::Skipped(Reason::Limit(refusal));
        }

        let Some(broker) = broker else {
            return Outcome::Skipped(Reason::DryRun);
        };
        if self.mode == Mode::DryRun {
            return Outcome::Skipped(Reason::DryRun);
        }

        self.stats.attempted += 1;
        let outcome = broker.execute(opportunity, self.mode);
        match &outcome {
            Outcome::Simulated { ok: true, .. } => self.stats.simulated_ok += 1,
            Outcome::Simulated { ok: false, .. } => self.stats.simulated_failed += 1,
            Outcome::Skipped(Reason::BuildFailed(_)) => self.stats.build_failed += 1,
            Outcome::Sent { .. } => {
                self.stats.sent += 1;
                // Only a real send commits money. A simulation that happened to
                // cost nothing must not eat the session's allowance.
                self.stats.spent += self.limits.per_attempt_lamports;
            }
            Outcome::Skipped(_) => {}
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::bot::arb::Opportunity;

    fn limits() -> BotLimits {
        BotLimits {
            per_attempt_lamports: 5_000_000,
            session_cap_lamports: 20_000_000,
            kill_file: PathBuf::from("/nonexistent/KILL"),
            max_wallet_lamports: 200_000_000,
        }
    }

    fn opportunity() -> Opportunity {
        Opportunity {
            sell_on: "orca 4bp",
            buy_on: "orca 2bp",
            in_lamports: 5_000_000,
            gross_bps: 30,
            net_lamports: 40_000,
            staleness_ms: 12,
        }
    }

    /// Records what it was asked to do and answers as told.
    struct FakeBroker {
        answer: Outcome,
        calls: Mutex<Vec<Mode>>,
    }

    impl Broker for FakeBroker {
        fn execute(&self, _: &Opportunity, mode: Mode) -> Outcome {
            self.calls.lock().unwrap().push(mode);
            self.answer.clone()
        }
    }

    fn broker(answer: Outcome) -> FakeBroker {
        FakeBroker { answer, calls: Mutex::new(Vec::new()) }
    }

    #[test]
    fn a_dry_run_never_reaches_the_broker_and_spends_nothing() {
        let sink = broker(Outcome::Sent { signature: "no".into() });
        let mut executor = Executor::new("test", limits(), Mode::DryRun);
        assert_eq!(executor.attempt(&opportunity(), 0, Some(&sink)), Outcome::Skipped(Reason::DryRun));
        assert!(sink.calls.lock().unwrap().is_empty(), "the broker must not be called");
        assert_eq!(executor.stats().spent, 0);
        assert_eq!(executor.stats().attempted, 0);
    }

    #[test]
    fn the_kill_file_stops_an_attempt_before_anything_else_is_considered() {
        // The one control an operator uses in a hurry. It must not depend on
        // the mode, the balance, or a broker existing.
        let dir = std::env::temp_dir().join("sel-exec-kill");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("KILL");
        std::fs::write(&path, b"").unwrap();
        let sink = broker(Outcome::Sent { signature: "no".into() });
        let mut executor =
            Executor::new("test", BotLimits { kill_file: path.clone(), ..limits() }, Mode::Live);
        let outcome = executor.attempt(&opportunity(), 0, Some(&sink));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(outcome, Outcome::Skipped(Reason::Limit(Refusal::Killed)));
        assert!(sink.calls.lock().unwrap().is_empty());
        assert_eq!(executor.stats().refused_killed, 1);
    }

    #[test]
    fn simulating_proves_the_transaction_without_spending_the_allowance() {
        let sink = broker(Outcome::Simulated { ok: true, note: "would land".into() });
        let mut executor = Executor::new("test", limits(), Mode::Simulate);
        for _ in 0..10 {
            executor.attempt(&opportunity(), 0, Some(&sink));
        }
        assert_eq!(executor.stats().simulated_ok, 10);
        assert_eq!(executor.stats().spent, 0, "a simulation costs no allowance");
        assert_eq!(sink.calls.lock().unwrap().len(), 10);
    }

    #[test]
    fn only_a_real_send_eats_the_session_cap() {
        // A 20 M cap in 5 M attempts is exactly four, and the fifth is refused.
        // The cap is a ceiling on the total, not on the count.
        let sink = broker(Outcome::Sent { signature: "sig".into() });
        let mut executor = Executor::new("test", limits(), Mode::Live);
        for step in 1..=4 {
            assert!(
                matches!(executor.attempt(&opportunity(), 0, Some(&sink)), Outcome::Sent { .. }),
                "attempt {step} should fit under the cap"
            );
        }
        assert_eq!(executor.stats().spent, 20_000_000);
        assert_eq!(
            executor.attempt(&opportunity(), 0, Some(&sink)),
            Outcome::Skipped(Reason::Limit(Refusal::SessionCap))
        );
        assert_eq!(executor.stats().sent, 4);
        assert_eq!(executor.stats().spent, 20_000_000, "a refusal spends nothing");
    }

    #[test]
    fn a_build_failure_is_counted_and_costs_nothing() {
        // A broker that cannot build must not quietly burn the allowance, and
        // must not look like a send that vanished.
        let sink = broker(Outcome::Skipped(Reason::BuildFailed("no route".into())));
        let mut executor = Executor::new("test", limits(), Mode::Live);
        executor.attempt(&opportunity(), 0, Some(&sink));
        assert_eq!(executor.stats().build_failed, 1);
        assert_eq!(executor.stats().spent, 0);
        assert_eq!(executor.stats().sent, 0);
    }

    #[test]
    fn a_wallet_holding_too_much_is_refused_whatever_the_mode() {
        for mode in [Mode::DryRun, Mode::Simulate, Mode::Live] {
            let sink = broker(Outcome::Sent { signature: "no".into() });
            let mut executor = Executor::new("test", limits(), mode);
            assert_eq!(
                executor.attempt(&opportunity(), 500_000_000, Some(&sink)),
                Outcome::Skipped(Reason::Limit(Refusal::WalletTooLarge)),
                "mode {mode:?} must not bypass the wallet ceiling"
            );
        }
    }

    #[test]
    fn with_no_broker_at_all_nothing_can_be_sent() {
        // The state the demo runs in: no wallet, no broker, and the executor
        // still counts what it was offered.
        let mut executor = Executor::new("test", limits(), Mode::Live);
        assert_eq!(executor.attempt(&opportunity(), 0, None), Outcome::Skipped(Reason::DryRun));
        assert_eq!(executor.stats().offered, 1);
        assert_eq!(executor.stats().spent, 0);
    }
}
