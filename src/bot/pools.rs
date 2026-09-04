//! The pools we watch: same pair, several pools, different fees.
//!
//! Arbitrage here is between two Whirlpools of the SAME pair. They hold
//! different fee tiers and different depth, so a swap in one moves it and
//! leaves the other behind for a moment. That moment is the opportunity, and
//! the transaction that opens it is what arrives on the feed.
//!
//! Addresses come from Orca's published pool list and were then checked by
//! decoding each account: the fee rate the account reports matches the fee the
//! list advertises, on every pool below. An address that has since moved
//! decodes to nothing and is reported at startup rather than quietly producing
//! no triggers.

use solana_pubkey::Pubkey;
use std::str::FromStr;

#[derive(Debug, Clone, Copy)]
pub struct Pool {
    /// The pair, as the terminal writes it.
    pub pair: &'static str,
    pub address: Pubkey,
    /// What the terminal calls this pool. Short: the route column is narrow.
    pub label: &'static str,
    /// Decimals differ per token and are not guessable: BONK has 5, WBTC 8.
    /// A wrong pair of these scales the price by a power of ten and turns an
    /// ordinary market into an enormous fake opportunity.
    pub decimals_a: i32,
    pub decimals_b: i32,
}

pub struct PoolBook {
    pools: Vec<Pool>,
}

impl PoolBook {
    /// Read from Orca's published pool list on 2026-09-03: every pair that has
    /// at least two pools over $50k of liquidity, deepest three per pair.
    ///
    /// A stable pair alone is not enough. Two pools of SOL/USDC sit two or
    /// three basis points apart against five to nine of fees, so the honest
    /// answer there is almost always "no trade". The volatile pairs carry wide
    /// fee tiers and move far enough to open a real gap.
    pub fn demo() -> Self {
        let entries: &[(&'static str, &str, &'static str, i32, i32)] = &[
            ("SOL/USDC", "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", "orca 4bp", 9, 6),
            ("SOL/USDC", "FpCMFDFGYotvufJ7HrFHsWEiiQCGbkLCtwHiDnh7o28Q", "orca 2bp", 9, 6),
            ("SOL/USDC", "7qbRF6YsyGuLUVs6Y1q64bdVrfe4ZcUUz1JRdoVNUJnm", "orca 5bp", 9, 6),
            ("SOL/FART", "C9U2Ksk6KKWvLEeo5yUQ7Xu46X7NzeBJtd9PBfuXaUSM", "orca 16bp", 9, 6),
            ("SOL/FART", "HUGNqTa2qqkaAVsQmYzBorZfusMUY4ToHq99WKJY43Vb", "orca 200bp", 9, 6),
            ("SOL/WBTC", "B5EwJVDuAauzUEEdwvbuXzbFFgEYnUqqS37TUM1c4PQA", "orca 5bp", 9, 8),
            ("SOL/WBTC", "CjZhHQnNUWdMtHzpJuxrPsK2hydeqKRyiMuzLQ3BMDkK", "orca 30bp", 9, 8),
            ("SOL/JLP", "6a3m2EgFFKfsFuQtP4LJJXPcAe3TQYXNyHUjjZpUxYgd", "orca 4bp", 9, 6),
            ("SOL/JLP", "4Z1A4Wy4Qj1GDC98YXYCuEkibVsHXoB1pMsn2crqEgDF", "orca 16bp", 9, 6),
            ("SOL/JLP", "4GZWN1bzbkBXwxMhyj9aQd5ZfovoLNSheBtukKdMtNXS", "orca 5bp", 9, 6),
            ("JUP/SOL", "C1MgLojNLWBKADvu9BHdtgzz1oZX4dZ5zGdGcgvvW8Wz", "orca 5bp", 6, 9),
            ("JUP/SOL", "FgTCR1ufcaTZMwZZYhNRhJm2K3HgMA8V8kXtdqyttm19", "orca 100bp", 6, 9),
            ("MEW/SOL", "ENrEBzFdNp8mZ11j1wXYZ5mbyX5yA3Z4t9ALbBKtZ2RD", "orca 1bp", 5, 9),
            ("MEW/SOL", "6b3pGVYwAemYXuCQCH2qQee3zyp3nzeqgpoyxUXSCQbc", "orca 200bp", 5, 9),
            ("SOL/BONK", "3ne4mWqdYuNiYrYZC9TrA3FcfuFdErghH97vNPbjicr1", "orca 30bp", 9, 5),
            ("SOL/BONK", "5zpyutJu9ee6jFymDGoK7F6S5Kczqtc9FomP3ueKuyA9", "orca 5bp", 9, 5),
            ("SOL/BONK", "BqnpCdDLPV2pFdAaLnVidmn3G93RP2p5oRdGEY2sJGez", "orca 100bp", 9, 5),
        ];
        Self::from_entries(entries)
    }

    pub fn from_entries(entries: &[(&'static str, &str, &'static str, i32, i32)]) -> Self {
        Self {
            pools: entries
                .iter()
                .map(|(pair, address, label, decimals_a, decimals_b)| Pool {
                    pair,
                    address: Pubkey::from_str(address).expect("checked pool address"),
                    label,
                    decimals_a: *decimals_a,
                    decimals_b: *decimals_b,
                })
                .collect(),
        }
    }

    pub fn all(&self) -> &[Pool] {
        &self.pools
    }

    pub fn find(&self, address: &Pubkey) -> Option<&Pool> {
        self.pools.iter().find(|pool| &pool.address == address)
    }

    pub fn siblings(&self, pool: &Pool) -> Vec<&Pool> {
        self.pools
            .iter()
            .filter(|other| other.pair == pool.pair && other.address != pool.address)
            .collect()
    }

    pub fn pairs(&self) -> Vec<&'static str> {
        let mut pairs: Vec<&'static str> = self.pools.iter().map(|pool| pool.pair).collect();
        pairs.sort_unstable();
        pairs.dedup();
        pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pair_has_a_sibling_to_be_out_of_line_with() {
        // A pair with one pool cannot produce an opportunity, so watching it
        // would only add triggers that go nowhere.
        let book = PoolBook::demo();
        for pool in book.all() {
            assert!(!book.siblings(pool).is_empty(), "{} has no sibling", pool.label);
        }
    }

    #[test]
    fn a_pool_address_resolves_to_its_pair() {
        let book = PoolBook::demo();
        let first = book.all()[0];
        assert_eq!(book.find(&first.address).map(|p| p.pair), Some(first.pair));
    }

    #[test]
    fn no_address_is_listed_twice() {
        // The same pool under two pairs would let the bot trade it against
        // itself, which always looks free and never is.
        let book = PoolBook::demo();
        let mut seen = std::collections::HashSet::new();
        for pool in book.all() {
            assert!(seen.insert(pool.address), "{} listed twice", pool.label);
        }
    }

    #[test]
    fn a_pair_never_mixes_decimals() {
        // Two pools of one pair must agree about the tokens, or the prices
        // are on different scales and every comparison is nonsense.
        let book = PoolBook::demo();
        for pool in book.all() {
            for sibling in book.siblings(pool) {
                assert_eq!(
                    (pool.decimals_a, pool.decimals_b),
                    (sibling.decimals_a, sibling.decimals_b),
                    "{} and {} disagree about decimals",
                    pool.label,
                    sibling.label
                );
            }
        }
    }

    #[test]
    fn the_volatile_pairs_are_there_on_purpose() {
        // Stable pairs alone almost never open a gap wider than the fees.
        let pairs = PoolBook::demo().pairs();
        assert!(pairs.len() >= 5, "too few pairs to ever see an opportunity");
    }

    #[test]
    fn an_unknown_address_is_not_a_trigger() {
        assert!(PoolBook::demo().find(&Pubkey::new_unique()).is_none());
    }

    #[test]
    fn siblings_never_include_the_pool_itself() {
        let book = PoolBook::demo();
        for pool in book.all() {
            assert!(book.siblings(pool).iter().all(|other| other.address != pool.address));
        }
    }

    #[test]
    fn a_sibling_is_always_the_same_pair() {
        let book = PoolBook::demo();
        for pool in book.all() {
            assert!(book.siblings(pool).iter().all(|other| other.pair == pool.pair));
        }
    }
}
