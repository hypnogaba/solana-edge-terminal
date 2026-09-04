//! Hard limits, enforced here and nowhere else.
//!
//! The strategy is not allowed to know about money caps: it proposes, this
//! refuses. Keeping the two apart means a bug in the arbitrage maths cannot
//! spend more than the cap, and the kill file works even if every other value
//! is wrong.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The kill file exists: an operator stopped the bot by hand.
    Killed,
    /// This session has already spent its allowance.
    SessionCap,
    /// The wallet holds more than the demo is allowed to touch.
    WalletTooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Allowed {
    Yes,
    No(Refusal),
}

#[derive(Debug, Clone)]
pub struct BotLimits {
    pub per_attempt_lamports: u64,
    pub session_cap_lamports: u64,
    pub kill_file: PathBuf,
    pub max_wallet_lamports: u64,
}

impl BotLimits {
    /// Checked before every attempt. `spent` is this session's total so far,
    /// `balance` the wallet's current lamports.
    pub fn check(&self, spent: u64, balance: u64) -> Allowed {
        if self.kill_file.exists() {
            return Allowed::No(Refusal::Killed);
        }
        if balance > self.max_wallet_lamports {
            return Allowed::No(Refusal::WalletTooLarge);
        }
        if spent.saturating_add(self.per_attempt_lamports) > self.session_cap_lamports {
            return Allowed::No(Refusal::SessionCap);
        }
        Allowed::Yes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn base() -> BotLimits {
        BotLimits {
            per_attempt_lamports: 5_000_000,
            session_cap_lamports: 100_000_000,
            kill_file: PathBuf::from("/nonexistent/KILL"),
            max_wallet_lamports: 200_000_000,
        }
    }

    #[test]
    fn an_attempt_is_allowed_until_the_session_cap() {
        let limits = base();
        assert_eq!(limits.check(0, 100_000_000), Allowed::Yes);
        assert_eq!(limits.check(95_000_000, 100_000_000), Allowed::Yes);
        assert_eq!(limits.check(96_000_000, 100_000_000), Allowed::No(Refusal::SessionCap));
    }

    #[test]
    fn the_kill_file_beats_every_other_setting() {
        // A kill file is the one control an operator can use in a hurry, so it
        // must not depend on any other value being sane.
        let dir = std::env::temp_dir().join("sel-kill-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("KILL");
        std::fs::File::create(&path).unwrap().write_all(b"").unwrap();
        let limits = BotLimits { kill_file: path.clone(), ..base() };
        assert_eq!(limits.check(0, 100_000_000), Allowed::No(Refusal::Killed));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(limits.check(0, 100_000_000), Allowed::Yes);
    }

    #[test]
    fn a_wallet_holding_too_much_refuses_to_trade() {
        // Guards against pointing the demo at a funded wallet by mistake.
        assert_eq!(base().check(0, 500_000_000), Allowed::No(Refusal::WalletTooLarge));
    }

    #[test]
    fn the_cap_cannot_be_stepped_over_by_a_huge_attempt() {
        let limits = BotLimits { per_attempt_lamports: u64::MAX, ..base() };
        assert_eq!(limits.check(0, 100_000_000), Allowed::No(Refusal::SessionCap));
    }
}
