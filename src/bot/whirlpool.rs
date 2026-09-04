//! Reading an Orca Whirlpool's price straight off its account.
//!
//! This is what replaced asking a quote service. A free quote endpoint rate
//! limited us out within seconds of a real trigger rate, and, worse, put a
//! 130 ms network hop in the path we are trying to measure. A pool account is
//! pushed to us by our own RPC and decodes in nanoseconds, which is how a real
//! bot does it.
//!
//! Offsets were read off live mainnet accounts, not off a layout document:
//! `fee_rate` at 45 came back as 400, 200 and 500 for the pools Orca's own list
//! calls 0.04%, 0.02% and 0.05%, and `sqrt_price` at 65 gives 105.11 against a
//! SOL price of 105.06 from an independent quote.

/// Anchor discriminator (8) + config (32) + bump (1) + tick spacing (2) + seed (2).
const FEE_RATE_OFFSET: usize = 45;
/// ...+ fee rate (2) + protocol fee rate (2) + liquidity (16).
const SQRT_PRICE_OFFSET: usize = 65;
const SQRT_PRICE_LEN: usize = 16;
/// Every Whirlpool account seen on mainnet is this long.
pub const WHIRLPOOL_ACCOUNT_LEN: usize = 653;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolState {
    /// Price of one unit of token A in units of token B, decimals applied.
    pub price: f64,
    /// The pool's own fee, in hundredths of a basis point (400 = 4 bps).
    pub fee_rate: u32,
}

impl PoolState {
    /// The pool's fee in basis points, rounded up so a rounding error can only
    /// make an opportunity look worse than it is.
    pub fn fee_bps(&self) -> u64 {
        (u64::from(self.fee_rate) + 99) / 100
    }
}

/// Decode a Whirlpool account. `decimals_a`/`decimals_b` come from the mints.
///
/// Returns None rather than a wrong number: a short account, or one from
/// another program, would otherwise produce a plausible price out of whatever
/// bytes happened to sit at the offset.
pub fn decode(data: &[u8], decimals_a: i32, decimals_b: i32) -> Option<PoolState> {
    if data.len() < SQRT_PRICE_OFFSET + SQRT_PRICE_LEN {
        return None;
    }
    let fee_rate = u16::from_le_bytes([data[FEE_RATE_OFFSET], data[FEE_RATE_OFFSET + 1]]);
    let mut raw = [0u8; SQRT_PRICE_LEN];
    raw.copy_from_slice(&data[SQRT_PRICE_OFFSET..SQRT_PRICE_OFFSET + SQRT_PRICE_LEN]);
    let sqrt_price = u128::from_le_bytes(raw);
    if sqrt_price == 0 {
        return None;
    }
    let ratio = sqrt_price as f64 / 2f64.powi(64);
    let price = ratio * ratio * 10f64.powi(decimals_a - decimals_b);
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    Some(PoolState { price, fee_rate: u32::from(fee_rate) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first 97 bytes of Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE,
    /// the deepest SOL/USDC Whirlpool, read from mainnet on 2026-09-03.
    const CZFQ3: &str = "3f95d10ce180630913e441f83913ca68b0634fb025fdeaa88737e84110d1255e\
357b3377ddee1ccdff0400040090011405980f48d6e1c202000000000000000000710e1b5b51f5ff5200000000\
00000000a8ffff90b5dd290000000084742103";

    fn bytes() -> Vec<u8> {
        (0..CZFQ3.len() / 2)
            .map(|i| u8::from_str_radix(&CZFQ3[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn a_live_account_decodes_to_the_price_an_independent_quote_gave() {
        // A quote service priced SOL at 105.06 within a minute of this dump.
        let state = decode(&bytes(), 9, 6).expect("decodes");
        assert!(
            (state.price - 105.1).abs() < 0.5,
            "price {} is not near the independently quoted 105.06",
            state.price
        );
    }

    #[test]
    fn the_fee_rate_matches_what_the_venue_publishes() {
        // Orca's own list calls this pool 0.04%.
        let state = decode(&bytes(), 9, 6).expect("decodes");
        assert_eq!(state.fee_rate, 400);
        assert_eq!(state.fee_bps(), 4);
    }

    #[test]
    fn decimals_are_applied_and_not_assumed() {
        // Same bytes, read as a pair of equal-decimal tokens, must not give the
        // SOL/USDC number. Getting this wrong scales every price by 1000.
        let state = decode(&bytes(), 6, 6).expect("decodes");
        assert!((state.price - 0.1051).abs() < 0.001, "got {}", state.price);
    }

    #[test]
    fn a_fee_rate_rounds_up_so_a_gap_never_looks_better_than_it_is() {
        let state = PoolState { price: 1.0, fee_rate: 401 };
        assert_eq!(state.fee_bps(), 5);
    }

    #[test]
    fn a_short_account_is_refused_rather_than_read_past() {
        assert!(decode(&bytes()[..40], 9, 6).is_none());
        assert!(decode(&[], 9, 6).is_none());
    }

    #[test]
    fn an_empty_pool_is_refused() {
        // sqrt_price of zero is not a price of zero, it is a pool that has
        // never traded, and dividing by it downstream would be a crash.
        let mut data = bytes();
        for byte in &mut data[SQRT_PRICE_OFFSET..SQRT_PRICE_OFFSET + SQRT_PRICE_LEN] {
            *byte = 0;
        }
        assert!(decode(&data, 9, 6).is_none());
    }
}
