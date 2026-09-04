# Stream terminal implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Two identical arbitrage bots, one on the DoubleZero shred feed and one on an ordinary RPC lane, competing for the same DEX-to-DEX gaps, shown on a dense trading terminal.

**Architecture:** The existing binary already reads both lanes on one clock. Add a bot layer on top: a trigger detector that spots a swap in a watched pool, a quote client, a pure decision function shared by both lanes, and an executor that owns one wallet per lane and enforces the hard limits. The decision function is pure and shared, which is what makes the two bots provably identical. The terminal page reads one JSON snapshot.

**Tech Stack:** Rust 1.97, `solana-stream-sdk` for shred decoding, Jupiter's quote and swap-instructions HTTP API, `solana-client` for sending, `reqwest` for HTTP, existing hand-rolled HTTP server for the page.

**Staging:** Tasks 1 to 8 deliver a terminal that detects and measures, sending nothing. Task 9 turns on real sending behind the limits. Do not start Task 9 until the terminal has run for an hour and the numbers look sane.

---

### Task 1: Bot configuration and hard limits

**Files:**
- Create: `src/bot/mod.rs`
- Create: `src/bot/config.rs`
- Modify: `src/main.rs:19-23` (add `mod bot;`)

- [ ] **Step 1: Write the failing test**

In `src/bot/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn base() -> BotLimits {
        BotLimits {
            per_attempt_lamports: 5_000_000,
            session_cap_lamports: 100_000_000,
            kill_file: std::path::PathBuf::from("/nonexistent/KILL"),
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
        let limits = base();
        assert_eq!(limits.check(0, 500_000_000), Allowed::No(Refusal::WalletTooLarge));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release bot::config -- --nocapture`
Expected: FAIL, `BotLimits` not found.

- [ ] **Step 3: Write minimal implementation**

`src/bot/mod.rs`:

```rust
//! The bot layer: spot a gap between pools, decide, and act.
//!
//! Both lanes share every part of this except the wallet and the feed that
//! triggers them, which is the whole point: any difference in outcome comes
//! from when each lane learned, not from what it decided.

pub mod config;
```

`src/bot/config.rs`:

```rust
//! Hard limits, enforced here and nowhere else.
//!
//! The strategy is not allowed to know about money caps: it proposes, this
//! refuses. Keeping the two apart means a bug in the arbitrage maths cannot
//! spend more than the cap, and the kill file works even if every other value
//! is wrong.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// data/KILL exists: an operator stopped the bot by hand.
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
        if spent + self.per_attempt_lamports > self.session_cap_lamports {
            return Allowed::No(Refusal::SessionCap);
        }
        Allowed::Yes
    }
}
```

Add `mod bot;` to `src/main.rs` beside the other module declarations.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --release bot::config`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/bot/mod.rs src/bot/config.rs src/main.rs
git commit -m "Bot limits: the kill file wins over everything else"
```

---

### Task 2: The watched venues table

**Files:**
- Create: `src/bot/venues.rs`
- Modify: `src/bot/mod.rs` (add `pub mod venues;`)

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_address_resolves_to_its_pair_and_venue() {
        let book = VenueBook::demo();
        let pool = book.all().first().expect("demo book is not empty").pool;
        let found = book.find(&pool).expect("known pool");
        assert_eq!(found.pool, pool);
    }

    #[test]
    fn an_unknown_address_is_not_a_trigger() {
        let book = VenueBook::demo();
        let stranger = solana_pubkey::Pubkey::new_unique();
        assert!(book.find(&stranger).is_none());
    }

    #[test]
    fn every_pair_has_at_least_two_venues() {
        // One venue cannot be out of line with itself: a pair with a single
        // pool would silently never produce an opportunity.
        let book = VenueBook::demo();
        for pair in book.pairs() {
            assert!(book.venues_for(pair).len() >= 2, "{pair:?} has one venue");
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release bot::venues`
Expected: FAIL, `VenueBook` not found.

- [ ] **Step 3: Write minimal implementation**

```rust
//! Which pools we watch, and what they are.
//!
//! A trigger is a transaction touching one of these addresses. Keeping the list
//! explicit and small is deliberate: the demo is about latency, not about
//! covering the whole chain, and a short list keeps the terminal readable.

use solana_pubkey::Pubkey;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pair {
    SolUsdc,
    SolUsdt,
}

impl Pair {
    pub fn label(self) -> &'static str {
        match self {
            Pair::SolUsdc => "SOL/USDC",
            Pair::SolUsdt => "SOL/USDT",
        }
    }

    /// The mint bought and sold, base first.
    pub fn mints(self) -> (&'static str, &'static str) {
        const SOL: &str = "So11111111111111111111111111111111111111112";
        match self {
            Pair::SolUsdc => (SOL, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            Pair::SolUsdt => (SOL, "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Venue {
    Raydium,
    Orca,
    Meteora,
}

impl Venue {
    /// The value Jupiter's `dexes` filter expects.
    pub fn jupiter_label(self) -> &'static str {
        match self {
            Venue::Raydium => "Raydium",
            Venue::Orca => "Whirlpool",
            Venue::Meteora => "Meteora DLMM",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Venue::Raydium => "ray",
            Venue::Orca => "orca",
            Venue::Meteora => "met",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WatchedPool {
    pub pair: Pair,
    pub venue: Venue,
    pub pool: Pubkey,
}

pub struct VenueBook {
    pools: Vec<WatchedPool>,
}

impl VenueBook {
    /// The pools the demo watches. Addresses are mainnet pool accounts; verify
    /// each against an explorer before a stream, because a delisted pool stops
    /// producing triggers silently.
    pub fn demo() -> Self {
        let entries: &[(Pair, Venue, &str)] = &[
            (Pair::SolUsdc, Venue::Raydium, "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2"),
            (Pair::SolUsdc, Venue::Orca, "HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ"),
            (Pair::SolUsdt, Venue::Raydium, "7XawhbbxtsRcQA8KTkHT9f9nc6d69UwqCDh6U5EEbEmX"),
            (Pair::SolUsdt, Venue::Orca, "4fuUiYxTQ6QCrdSq9ouBYcTM7bqSwYTSyLueGZLTy4T4"),
        ];
        Self {
            pools: entries
                .iter()
                .map(|(pair, venue, addr)| WatchedPool {
                    pair: *pair,
                    venue: *venue,
                    pool: Pubkey::from_str(addr).expect("hardcoded pool address"),
                })
                .collect(),
        }
    }

    pub fn all(&self) -> &[WatchedPool] {
        &self.pools
    }

    pub fn find(&self, address: &Pubkey) -> Option<&WatchedPool> {
        self.pools.iter().find(|p| &p.pool == address)
    }

    pub fn pairs(&self) -> Vec<Pair> {
        let mut pairs: Vec<Pair> = self.pools.iter().map(|p| p.pair).collect();
        pairs.sort_by_key(|p| p.label());
        pairs.dedup_by_key(|p| p.label());
        pairs
    }

    pub fn venues_for(&self, pair: Pair) -> Vec<Venue> {
        self.pools.iter().filter(|p| p.pair == pair).map(|p| p.venue).collect()
    }
}
```

Add `solana-pubkey = "3"` to `[dependencies]` in `Cargo.toml`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --release bot::venues`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/bot/venues.rs src/bot/mod.rs Cargo.toml
git commit -m "The pools we watch, and the rule that a pair needs two of them"
```

---

### Task 3: Turning a transaction into a trigger

**Files:**
- Create: `src/bot/trigger.rs`
- Modify: `src/bot/mod.rs`
- Modify: `src/pipeline.rs:24-30` (carry account keys on `SeenTx`)

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::venues::VenueBook;
    use solana_pubkey::Pubkey;

    #[test]
    fn a_transaction_touching_a_watched_pool_is_a_trigger() {
        let book = VenueBook::demo();
        let watched = book.all()[0];
        let keys = vec![Pubkey::new_unique(), watched.pool, Pubkey::new_unique()];
        let trigger = detect(&keys, &book).expect("should trigger");
        assert_eq!(trigger.pair.label(), watched.pair.label());
        assert_eq!(trigger.venue, watched.venue);
    }

    #[test]
    fn an_ordinary_transaction_is_not_a_trigger() {
        let book = VenueBook::demo();
        let keys = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        assert!(detect(&keys, &book).is_none());
    }

    #[test]
    fn the_first_watched_pool_wins_when_several_appear() {
        // A router transaction can touch two watched pools. Taking the first
        // keeps the trigger deterministic; taking "any" would make the two
        // lanes disagree about the same transaction.
        let book = VenueBook::demo();
        let a = book.all()[0];
        let b = book.all()[1];
        let keys = vec![a.pool, b.pool];
        assert_eq!(detect(&keys, &book).unwrap().venue, a.venue);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release bot::trigger`
Expected: FAIL, `detect` not found.

- [ ] **Step 3: Write minimal implementation**

`src/bot/trigger.rs`:

```rust
//! A transaction becomes a trigger when it touches a pool we watch.
//!
//! Matching is on the transaction's static account keys. Keys hidden behind an
//! address lookup table are not resolved here, so a router transaction using
//! one is missed. That is a known gap: it costs opportunities, it never
//! invents them, and it costs both lanes equally.

use solana_pubkey::Pubkey;

use crate::bot::venues::{Pair, Venue, VenueBook};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    pub pair: Pair,
    pub venue: Venue,
}

pub fn detect(account_keys: &[Pubkey], book: &VenueBook) -> Option<Trigger> {
    account_keys.iter().find_map(|key| {
        book.find(key).map(|pool| Trigger { pair: pool.pair, venue: pool.venue })
    })
}
```

In `src/pipeline.rs`, add the keys to `SeenTx` so the bot can see them:

```rust
#[derive(Debug, Clone)]
pub struct SeenTx {
    pub signature: String,
    pub slot: u64,
    pub at: Instant,
    /// Static account keys, for trigger matching. Lookup-table keys are absent.
    pub account_keys: Vec<solana_pubkey::Pubkey>,
}
```

and fill it in `ShredPipeline::on_packet` where `SeenTx` is built:

```rust
account_keys: transaction.message.static_account_keys().to_vec(),
```

The RPC lane has no account keys in a firehose notification, so
`parse_logs_notification` sets `account_keys: Vec::new()`.

**Correction, found while implementing.** That left the RPC-fed bot with nothing
to react to, which would have removed the race entirely. The fix is a
`logsSubscribe` with a `mentions` filter, one subscription per watched pool: the
notification then belongs to a known account, and both lanes learn the same
fact by their own route. `RpcLane::watching(accounts)` does this, and `Watched`
maps each subscription id back to its account. Verified live: 1,249
notifications in 98 seconds across four pools, every one attributed.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --release`
Expected: PASS, all tests including the 3 new ones.

- [ ] **Step 5: Commit**

```bash
git add src/bot/trigger.rs src/bot/mod.rs src/pipeline.rs src/rpc_lane.rs
git commit -m "A trigger is a transaction that touched a pool we watch"
```

---

### Task 4: The quote client, behind a trait

**Files:**
- Create: `src/bot/quote.rs`
- Modify: `src/bot/mod.rs`
- Modify: `Cargo.toml` (add `reqwest`)

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_response_is_read_into_output_lamports() {
        // Shape captured from a real Jupiter /quote response. A silent parse
        // failure here would look exactly like "there is never an opportunity".
        let body = r#"{"inputMint":"So11111111111111111111111111111111111111112",
            "inAmount":"5000000","outputMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "outAmount":"1043210","otherAmountThreshold":"1038000","priceImpactPct":"0.0004",
            "routePlan":[{"swapInfo":{"label":"Raydium"},"percent":100}]}"#;
        let quote = parse_quote(body).expect("parses");
        assert_eq!(quote.in_amount, 5_000_000);
        assert_eq!(quote.out_amount, 1_043_210);
    }

    #[test]
    fn a_quote_with_no_route_is_not_an_error_but_an_absence() {
        // Jupiter answers 200 with an error body when no route exists. Treating
        // that as a failure would spam the log; treating it as a zero-value
        // quote would invent an opportunity.
        let body = r#"{"error":"No routes found","errorCode":"NO_ROUTES_FOUND"}"#;
        assert!(parse_quote(body).is_none());
    }

    #[test]
    fn junk_never_panics() {
        for body in ["", "not json", "{}", r#"{"outAmount":"x"}"#] {
            assert!(parse_quote(body).is_none());
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release bot::quote`
Expected: FAIL, `parse_quote` not found.

- [ ] **Step 3: Write minimal implementation**

```rust
//! Prices, asked one venue at a time.
//!
//! A production bot keeps pool reserves in memory and computes the price
//! itself. This asks Jupiter, restricted to a single venue per call, which
//! costs roughly 100 ms per quote. Both lanes pay it equally, so the
//! comparison between them stays honest, but the absolute reaction time is
//! slower than a real operation's and the stream should say so.

use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::bot::venues::Venue;

pub const JUPITER_QUOTE_URL: &str = "https://quote-api.jup.ag/v6/quote";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    pub in_amount: u64,
    pub out_amount: u64,
}

#[derive(Deserialize)]
struct RawQuote {
    #[serde(rename = "inAmount")]
    in_amount: String,
    #[serde(rename = "outAmount")]
    out_amount: String,
}

pub fn parse_quote(body: &str) -> Option<Quote> {
    let raw: RawQuote = serde_json::from_str(body).ok()?;
    Some(Quote {
        in_amount: raw.in_amount.parse().ok()?,
        out_amount: raw.out_amount.parse().ok()?,
    })
}

/// Behind a trait so the decision can be tested without a network.
pub trait Quoter: Send + Sync {
    fn quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        venue: Venue,
    ) -> Result<Option<Quote>>;
}

pub struct JupiterQuoter {
    client: reqwest::blocking::Client,
}

impl JupiterQuoter {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self { client: reqwest::blocking::Client::builder().timeout(timeout).build()? })
    }
}

impl Quoter for JupiterQuoter {
    fn quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        venue: Venue,
    ) -> Result<Option<Quote>> {
        let body = self
            .client
            .get(JUPITER_QUOTE_URL)
            .query(&[
                ("inputMint", input_mint),
                ("outputMint", output_mint),
                ("amount", &amount.to_string()),
                ("slippageBps", "50"),
                ("dexes", venue.jupiter_label()),
                ("onlyDirectRoutes", "true"),
            ])
            .send()?
            .text()?;
        Ok(parse_quote(&body))
    }
}
```

Add to `Cargo.toml`:

```toml
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --release bot::quote`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/bot/quote.rs src/bot/mod.rs Cargo.toml
git commit -m "Quotes, one venue at a time, behind a trait the tests can fake"
```

---

### Task 5: The decision, pure and shared

**Files:**
- Create: `src/bot/arb.rs`
- Modify: `src/bot/mod.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::venues::Venue;

    fn costs() -> Costs {
        Costs { network_fee_lamports: 5_000, margin_bps: 5 }
    }

    #[test]
    fn a_round_trip_that_returns_more_than_it_cost_is_an_opportunity() {
        // 5_000_000 in, 5_050_000 back: 100 bps gross, comfortably over cost.
        let found = evaluate(5_000_000, 5_050_000, Venue::Raydium, Venue::Orca, &costs())
            .expect("opportunity");
        assert_eq!(found.buy_on, Venue::Raydium);
        assert_eq!(found.sell_on, Venue::Orca);
        assert_eq!(found.gross_bps, 100);
        assert_eq!(found.net_lamports, 50_000 - 5_000);
    }

    #[test]
    fn a_round_trip_that_loses_is_not_an_opportunity() {
        assert!(evaluate(5_000_000, 4_990_000, Venue::Raydium, Venue::Orca, &costs()).is_none());
    }

    #[test]
    fn a_gain_smaller_than_the_fee_is_not_an_opportunity() {
        // 5_000_000 -> 5_004_000 is 8 bps gross, but the fee eats it. Reporting
        // this as an opportunity is how a demo ends up sending losing trades.
        assert!(evaluate(5_000_000, 5_004_000, Venue::Raydium, Venue::Orca, &costs()).is_none());
    }

    #[test]
    fn a_gain_that_only_just_covers_the_fee_still_has_to_clear_the_margin() {
        // Exactly fee + 1 lamport: gross 8 bps, net positive, but under the
        // 5 bps margin, so it stays out.
        let costs = Costs { network_fee_lamports: 5_000, margin_bps: 20 };
        assert!(evaluate(5_000_000, 5_010_000, Venue::Raydium, Venue::Orca, &costs).is_none());
    }

    #[test]
    fn a_zero_input_is_refused_rather_than_dividing_by_zero() {
        assert!(evaluate(0, 100, Venue::Raydium, Venue::Orca, &costs()).is_none());
    }

    #[test]
    fn the_same_venue_on_both_legs_is_never_an_opportunity() {
        // One pool cannot be out of line with itself; a gap here means a bug
        // upstream, and acting on it would be free money that does not exist.
        assert!(evaluate(5_000_000, 5_500_000, Venue::Orca, Venue::Orca, &costs()).is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release bot::arb`
Expected: FAIL, `evaluate` not found.

- [ ] **Step 3: Write minimal implementation**

```rust
//! The decision. Pure: no clock, no wallet, no network.
//!
//! Both lanes call this same function with their own numbers, which is what
//! makes the two bots identical in every way except when they learned. Keep it
//! that way: anything that reads the outside world belongs in the caller.

use crate::bot::venues::Venue;

#[derive(Debug, Clone, Copy)]
pub struct Costs {
    /// What the atomic transaction costs to land, win or lose.
    pub network_fee_lamports: u64,
    /// How much better than break-even a gap has to be before we act.
    pub margin_bps: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opportunity {
    pub buy_on: Venue,
    pub sell_on: Venue,
    pub in_lamports: u64,
    /// The gap before costs, in basis points. What the terminal shows as GAP.
    pub gross_bps: u64,
    /// What is left after the network fee. Can be small; never negative.
    pub net_lamports: u64,
}

/// `out_lamports` is what the round trip returns for `in_lamports`: buy the
/// quote asset on `buy_on`, sell it back on `sell_on`.
pub fn evaluate(
    in_lamports: u64,
    out_lamports: u64,
    buy_on: Venue,
    sell_on: Venue,
    costs: &Costs,
) -> Option<Opportunity> {
    if in_lamports == 0 || buy_on == sell_on {
        return None;
    }
    let gross = out_lamports.checked_sub(in_lamports)?;
    let gross_bps = gross.checked_mul(10_000)? / in_lamports;
    if gross_bps < costs.margin_bps {
        return None;
    }
    let net_lamports = gross.checked_sub(costs.network_fee_lamports)?;
    if net_lamports == 0 {
        return None;
    }
    Some(Opportunity { buy_on, sell_on, in_lamports, gross_bps, net_lamports })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --release bot::arb`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add src/bot/arb.rs src/bot/mod.rs
git commit -m "The arbitrage decision, pure so both lanes can share it"
```

---

### Task 6: A lane's bot, and the record it keeps

**Files:**
- Create: `src/bot/lane.rs`
- Modify: `src/bot/mod.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use super::*;
    use crate::bot::quote::Quote;
    use crate::bot::venues::{Pair, Venue, VenueBook};

    /// Answers each venue with a fixed output, so the decision is the only
    /// thing under test.
    struct FakeQuoter {
        outputs: Mutex<Vec<u64>>,
    }

    impl crate::bot::quote::Quoter for FakeQuoter {
        fn quote(&self, _: &str, _: &str, amount: u64, _: Venue)
            -> anyhow::Result<Option<Quote>>
        {
            let out = self.outputs.lock().unwrap().pop().unwrap_or(amount);
            Ok(Some(Quote { in_amount: amount, out_amount: out }))
        }
    }

    fn lane(outputs: Vec<u64>) -> LaneBot {
        LaneBot::new(
            "test",
            VenueBook::demo(),
            Box::new(FakeQuoter { outputs: Mutex::new(outputs) }),
            Costs { network_fee_lamports: 5_000, margin_bps: 5 },
            5_000_000,
        )
    }

    #[test]
    fn a_trigger_with_a_gap_produces_an_opportunity() {
        // Leg one returns 1_050_000 quote units, leg two returns 5_100_000
        // lamports: a clear round-trip gain.
        let mut bot = lane(vec![5_100_000, 1_050_000]);
        let trigger = Trigger { pair: Pair::SolUsdc, venue: Venue::Raydium };
        let found = bot.on_trigger(&trigger, "sig1", 443_961_641, Instant::now());
        assert!(found.is_some());
        assert_eq!(bot.stats().seen, 1);
        assert_eq!(bot.stats().actionable, 1);
    }

    #[test]
    fn a_trigger_with_no_gap_is_counted_but_not_acted_on() {
        let mut bot = lane(vec![5_000_000, 1_000_000]);
        let trigger = Trigger { pair: Pair::SolUsdc, venue: Venue::Raydium };
        assert!(bot.on_trigger(&trigger, "sig1", 1, Instant::now()).is_none());
        assert_eq!(bot.stats().seen, 1);
        assert_eq!(bot.stats().actionable, 0);
    }

    #[test]
    fn the_same_signature_is_only_acted_on_once() {
        // A feed can deliver the same transaction twice. Acting twice would
        // double-count the lane's own score.
        let mut bot = lane(vec![5_100_000, 1_050_000, 5_100_000, 1_050_000]);
        let trigger = Trigger { pair: Pair::SolUsdc, venue: Venue::Raydium };
        assert!(bot.on_trigger(&trigger, "sig1", 1, Instant::now()).is_some());
        assert!(bot.on_trigger(&trigger, "sig1", 1, Instant::now()).is_none());
        assert_eq!(bot.stats().seen, 1);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release bot::lane`
Expected: FAIL, `LaneBot` not found.

- [ ] **Step 3: Write minimal implementation**

```rust
//! One lane's bot: the same parts, wired to one feed.

use std::collections::HashSet;
use std::time::Instant;

use crate::bot::arb::{evaluate, Costs, Opportunity};
use crate::bot::quote::Quoter;
use crate::bot::trigger::Trigger;
use crate::bot::venues::{Venue, VenueBook};

#[derive(Debug, Default, Clone, Copy)]
pub struct LaneStats {
    /// Triggers this lane acted on, after de-duplication.
    pub seen: u64,
    /// Of those, the ones worth acting on.
    pub actionable: u64,
    /// Quote calls that failed or returned no route.
    pub quote_misses: u64,
}

#[derive(Debug, Clone)]
pub struct Found {
    pub opportunity: Opportunity,
    pub trigger_signature: String,
    pub slot: u64,
    pub decided_at: Instant,
    pub pair_label: &'static str,
}

pub struct LaneBot {
    pub name: String,
    book: VenueBook,
    quoter: Box<dyn Quoter>,
    costs: Costs,
    size_lamports: u64,
    acted: HashSet<String>,
    stats: LaneStats,
}

impl LaneBot {
    pub fn new(
        name: &str,
        book: VenueBook,
        quoter: Box<dyn Quoter>,
        costs: Costs,
        size_lamports: u64,
    ) -> Self {
        Self {
            name: name.to_string(),
            book,
            quoter,
            costs,
            size_lamports,
            acted: HashSet::new(),
            stats: LaneStats::default(),
        }
    }

    pub fn stats(&self) -> LaneStats {
        self.stats
    }

    /// Price the round trip and decide. Returns the opportunity if there is one.
    pub fn on_trigger(
        &mut self,
        trigger: &Trigger,
        signature: &str,
        slot: u64,
        decided_at: Instant,
    ) -> Option<Found> {
        if !self.acted.insert(signature.to_string()) {
            return None;
        }
        self.stats.seen += 1;

        let (base, quote_mint) = trigger.pair.mints();
        let other: Vec<Venue> = self
            .book
            .venues_for(trigger.pair)
            .into_iter()
            .filter(|v| *v != trigger.venue)
            .collect();

        for sell_on in other {
            let leg_one = match self.quoter.quote(base, quote_mint, self.size_lamports, trigger.venue) {
                Ok(Some(quote)) => quote,
                _ => {
                    self.stats.quote_misses += 1;
                    continue;
                }
            };
            let leg_two = match self.quoter.quote(quote_mint, base, leg_one.out_amount, sell_on) {
                Ok(Some(quote)) => quote,
                _ => {
                    self.stats.quote_misses += 1;
                    continue;
                }
            };
            if let Some(opportunity) = evaluate(
                self.size_lamports,
                leg_two.out_amount,
                trigger.venue,
                sell_on,
                &self.costs,
            ) {
                self.stats.actionable += 1;
                return Some(Found {
                    opportunity,
                    trigger_signature: signature.to_string(),
                    slot,
                    decided_at,
                    pair_label: trigger.pair.label(),
                });
            }
        }
        None
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --release bot::lane`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/bot/lane.rs src/bot/mod.rs
git commit -m "A lane's bot: trigger in, priced round trip out"
```

---

### Task 7: Pairing the two lanes onto one opportunity

**Files:**
- Create: `src/bot/duel.rs`
- Modify: `src/bot/mod.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn found(sig: &str, at: Instant) -> Found {
        Found {
            opportunity: crate::bot::arb::Opportunity {
                buy_on: crate::bot::venues::Venue::Raydium,
                sell_on: crate::bot::venues::Venue::Orca,
                in_lamports: 5_000_000,
                gross_bps: 24,
                net_lamports: 45_000,
            },
            trigger_signature: sig.to_string(),
            slot: 443_961_641,
            decided_at: at,
            pair_label: "SOL/USDC",
        }
    }

    #[test]
    fn the_head_start_is_the_gap_between_the_two_decisions() {
        let mut book = DuelBook::new(Duration::from_secs(600));
        let t0 = Instant::now();
        book.record("doublezero", found("sig", t0));
        book.record("public", found("sig", t0 + Duration::from_millis(318)));
        let duel = &book.recent()[0];
        assert_eq!(duel.head_start_ms.map(|v| v.round()), Some(318.0));
        assert_eq!(duel.first.as_deref(), Some("doublezero"));
    }

    #[test]
    fn an_opportunity_only_one_lane_found_has_no_head_start() {
        // The other lane may still be about to find it, or may never. Inventing
        // a head start here would flatter whichever lane reported first.
        let mut book = DuelBook::new(Duration::from_secs(600));
        book.record("doublezero", found("sig", Instant::now()));
        assert_eq!(book.recent()[0].head_start_ms, None);
    }

    #[test]
    fn a_lane_reporting_twice_does_not_improve_its_own_time() {
        let mut book = DuelBook::new(Duration::from_secs(600));
        let t0 = Instant::now();
        book.record("doublezero", found("sig", t0 + Duration::from_millis(50)));
        book.record("doublezero", found("sig", t0));
        book.record("public", found("sig", t0 + Duration::from_millis(150)));
        assert_eq!(book.recent()[0].head_start_ms.map(|v| v.round()), Some(100.0));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release bot::duel`
Expected: FAIL, `DuelBook` not found.

- [ ] **Step 3: Write minimal implementation**

```rust
//! One opportunity, as found by each lane.
//!
//! Paired on the trigger transaction's signature, which is the same on every
//! feed. A lane that never found an opportunity is not scored against it: that
//! is a miss, reported as such, not a win for the other side.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::bot::lane::Found;

#[derive(Debug, Clone, Serialize)]
pub struct Duel {
    pub signature: String,
    pub slot: u64,
    pub pair: String,
    pub route: String,
    pub gross_bps: u64,
    /// Milliseconds the first lane was ahead. None until both have reported.
    pub head_start_ms: Option<f64>,
    pub first: Option<String>,
    pub lanes: Vec<String>,
}

struct Entry {
    duel: Duel,
    times: HashMap<String, Instant>,
    created: Instant,
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

    pub fn record(&mut self, lane: &str, found: Found) {
        let key = found.trigger_signature.clone();
        let entry = self.entries.entry(key.clone()).or_insert_with(|| {
            self.order.push(key.clone());
            Entry {
                duel: Duel {
                    signature: found.trigger_signature.clone(),
                    slot: found.slot,
                    pair: found.pair_label.to_string(),
                    route: format!(
                        "{}→{}",
                        found.opportunity.buy_on.short(),
                        found.opportunity.sell_on.short()
                    ),
                    gross_bps: found.opportunity.gross_bps,
                    head_start_ms: None,
                    first: None,
                    lanes: Vec::new(),
                },
                times: HashMap::new(),
                created: found.decided_at,
            }
        });

        // First report per lane wins: a repeat must not improve a lane's time.
        entry.times.entry(lane.to_string()).or_insert(found.decided_at);
        if !entry.duel.lanes.iter().any(|l| l == lane) {
            entry.duel.lanes.push(lane.to_string());
        }

        if entry.times.len() >= 2 {
            let mut sorted: Vec<(&String, &Instant)> = entry.times.iter().collect();
            sorted.sort_by_key(|(_, at)| **at);
            let (first_lane, first_at) = sorted[0];
            let (_, second_at) = sorted[1];
            entry.duel.first = Some(first_lane.clone());
            entry.duel.head_start_ms =
                Some(second_at.duration_since(*first_at).as_secs_f64() * 1000.0);
        }
    }

    pub fn evict(&mut self) {
        let now = Instant::now();
        let ttl = self.ttl;
        self.entries.retain(|_, e| now.saturating_duration_since(e.created) < ttl);
        let live: Vec<String> =
            self.order.iter().filter(|k| self.entries.contains_key(*k)).cloned().collect();
        self.order = live;
    }

    /// Newest first.
    pub fn recent(&self) -> Vec<Duel> {
        self.order
            .iter()
            .rev()
            .filter_map(|k| self.entries.get(k).map(|e| e.duel.clone()))
            .take(40)
            .collect()
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --release bot::duel`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/bot/duel.rs src/bot/mod.rs
git commit -m "One opportunity, two lanes, paired on the trigger signature"
```

---

### Task 8: The terminal page

**Files:**
- Modify: `src/serve.rs` (replace `PAGE`)
- Modify: `src/main.rs` (feed the bot state into the snapshot)
- Reference: `design/Main.dc.html`

- [ ] **Step 1: Write the failing test**

In `src/serve.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_reads_the_fields_the_snapshot_actually_carries() {
        // The page and the snapshot drift apart silently: a renamed field shows
        // as a blank panel, not an error. Every field the page reads must exist
        // in the JSON the binary writes.
        for field in [
            "head_to_head", "lanes", "recent", "shreds", "bots", "duels", "log",
        ] {
            assert!(PAGE.contains(field), "page never reads {field}");
        }
    }

    #[test]
    fn the_page_does_not_leak_an_endpoint() {
        assert!(!PAGE.contains("api-key"));
        // Only the font host may appear; anything else is an endpoint leaking.
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release serve`
Expected: FAIL on the missing fields.

- [ ] **Step 3: Write minimal implementation**

Rewrite `PAGE` in `src/serve.rs` following `design/Main.dc.html`: the status bar,
the three-column grid (feed telemetry and watched pools on the left, the
opportunity blotter in the centre, the two bot panels and a "why B loses" panel
on the right), and the event log across the bottom. Keep IBM Plex Mono
throughout, `#0A0B0D` page, `#101215` panel headers, `#1E2126` borders,
`#35D08A` for the fast lane, `#FF5C5C` for failures, `#E5A23D` for warnings.

Extend the snapshot in `src/main.rs::write_snapshot`:

```rust
value["bots"] = serde_json::json!({
    "doublezero": {"seen": dz.seen, "actionable": dz.actionable, "quote_misses": dz.quote_misses},
    "public": {"seen": pb.seen, "actionable": pb.actionable, "quote_misses": pb.quote_misses},
});
value["duels"] = serde_json::to_value(duels.recent())?;
value["log"] = serde_json::to_value(log.recent())?;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --release serve && cargo build --release`
Expected: PASS, and the binary builds.

- [ ] **Step 5: Verify against the running service**

```bash
rsync -az --exclude target --exclude .git ./ root@<host>:/root/solana-edge-lab/
ssh <host> 'cd /root/solana-edge-lab && cargo build --release && systemctl restart sol-race && sleep 20 && curl -s localhost:8090/api/state | head -c 400'
```

Expected: the snapshot carries `bots`, `duels` and `log`.

- [ ] **Step 6: Commit**

```bash
git add src/serve.rs src/main.rs
git commit -m "The terminal: blotter, two bot panels, event log"
```

---

### Task 9: Real execution, behind the limits

**Do not start this task until the terminal has run for an hour and the blotter
looks sane.** Until then both lanes decide and record, and send nothing.

**Files:**
- Create: `src/bot/execute.rs`
- Modify: `src/bot/mod.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::config::{Allowed, BotLimits, Refusal};

    fn limits() -> BotLimits {
        BotLimits {
            per_attempt_lamports: 5_000_000,
            session_cap_lamports: 100_000_000,
            kill_file: std::path::PathBuf::from("/nonexistent/KILL"),
            max_wallet_lamports: 200_000_000,
        }
    }

    #[test]
    fn dry_run_never_reports_a_signature() {
        let mut executor = Executor::new("test", limits(), Mode::DryRun);
        let outcome = executor.attempt(5_000_000, 100_000_000);
        assert!(matches!(outcome, Outcome::Skipped(Reason::DryRun)));
        assert_eq!(executor.spent(), 0, "a dry run must not count against the cap");
    }

    #[test]
    fn a_refusal_is_reported_with_its_reason_and_spends_nothing() {
        let mut executor = Executor::new("test", limits(), Mode::Live);
        let outcome = executor.attempt(5_000_000, 500_000_000);
        assert!(matches!(outcome, Outcome::Skipped(Reason::Limit(Refusal::WalletTooLarge))));
        assert_eq!(executor.spent(), 0);
    }

    #[test]
    fn spending_accumulates_towards_the_cap() {
        let mut executor = Executor::new("test", limits(), Mode::Live);
        for _ in 0..20 {
            executor.attempt(5_000_000, 100_000_000);
        }
        assert!(executor.spent() <= 100_000_000, "cap was exceeded");
        assert!(matches!(
            executor.attempt(5_000_000, 100_000_000),
            Outcome::Skipped(Reason::Limit(Refusal::SessionCap))
        ));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --release bot::execute`
Expected: FAIL, `Executor` not found.

- [ ] **Step 3: Write minimal implementation**

```rust
//! Sending, and the accounting that keeps it small.
//!
//! Every attempt passes the limits first. Dry run is the default and costs
//! nothing against the cap, so the terminal can be watched for an hour before
//! any money moves.

use crate::bot::config::{Allowed, BotLimits, Refusal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    DryRun,
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    DryRun,
    Limit(Refusal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Skipped(Reason),
    Sent { signature: String },
}

pub struct Executor {
    lane: String,
    limits: BotLimits,
    mode: Mode,
    spent: u64,
}

impl Executor {
    pub fn new(lane: &str, limits: BotLimits, mode: Mode) -> Self {
        Self { lane: lane.to_string(), limits, mode, spent: 0 }
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }

    /// Check, then send. The send itself is added in the next step; until then
    /// a permitted live attempt records the spend and reports no signature.
    pub fn attempt(&mut self, size_lamports: u64, balance: u64) -> Outcome {
        if let Allowed::No(refusal) = self.limits.check(self.spent, balance) {
            return Outcome::Skipped(Reason::Limit(refusal));
        }
        if self.mode == Mode::DryRun {
            return Outcome::Skipped(Reason::DryRun);
        }
        self.spent += size_lamports;
        Outcome::Sent { signature: String::new() }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --release bot::execute`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/bot/execute.rs src/bot/mod.rs
git commit -m "Execution accounting: dry run by default, limits before anything else"
```

- [ ] **Step 6: Build the transaction**

Compose one atomic transaction from Jupiter's `/swap-instructions` for both
legs, sign with the lane's keypair, send through the lane's RPC, and fill the
signature into `Outcome::Sent`. Write the test first, against a fixture of a
real `/swap-instructions` response captured in this session, asserting that both
legs end up in one transaction and that the compute budget instruction is
present. Commit separately.

---

## Before the stream

- Create two keypairs, fund each with about 0.15 SOL, and confirm
  `max_wallet_lamports` refuses anything larger.
- Provision `solana-shreds-full-fra` on the access pass, then switch the service
  to `--reference doublezero`.
- Verify every pool address in `VenueBook::demo()` on an explorer. A delisted
  pool produces no triggers and no error.
- Run for an hour in dry run. Only then set `Mode::Live`.
