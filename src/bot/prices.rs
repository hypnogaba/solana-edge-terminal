//! What each pool costs, kept fresh by the chain pushing at us.
//!
//! One `accountSubscribe` per pool. The hot path never makes a network call:
//! a trigger arrives, the bot reads this book, and decides. That is both what a
//! real bot does and the only honest way to measure a feed's head start, since
//! any request in the path would be added to both lanes and drown the thing
//! being measured.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Instant;

use base64::Engine;
use solana_pubkey::Pubkey;

use crate::bot::whirlpool::{decode, PoolState};

#[derive(Debug, Clone, Copy)]
pub struct Priced {
    pub state: PoolState,
    pub slot: u64,
    pub at: Instant,
}

#[derive(Debug, Default)]
pub struct PriceBook {
    inner: RwLock<HashMap<Pubkey, Priced>>,
}

impl PriceBook {
    pub fn set(&self, pool: Pubkey, priced: Priced) {
        if let Ok(mut inner) = self.inner.write() {
            inner.insert(pool, priced);
        }
    }

    pub fn get(&self, pool: &Pubkey) -> Option<Priced> {
        self.inner.read().ok()?.get(pool).copied()
    }

    pub fn len(&self) -> usize {
        self.inner.read().map(|inner| inner.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forget everything. Called when the price feed drops: a price from before
    /// an outage is not a price, and keeping it lets the bot answer confidently
    /// with a market that has moved on since.
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.write() {
            inner.clear();
        }
    }
}

/// The account data and slot out of an `accountNotification`.
///
/// Split out and tested against a real frame because a silent parse failure
/// here leaves every price stale forever, which reads as "the market is quiet".
pub fn parse_account_notification(text: &str) -> Option<(u64, u64, Vec<u8>)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("method")?.as_str()? != "accountNotification" {
        return None;
    }
    let params = value.get("params")?;
    let subscription = params.get("subscription")?.as_u64()?;
    let result = params.get("result")?;
    let slot = result.get("context")?.get("slot")?.as_u64()?;
    let data = result.get("value")?.get("data")?;
    let encoded = data.get(0)?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    Some((subscription, slot, bytes))
}

/// Fold one notification into the book, given the pool it belongs to.
pub fn absorb(
    book: &PriceBook,
    pool: &crate::bot::pools::Pool,
    slot: u64,
    data: &[u8],
    at: Instant,
) -> bool {
    match decode(data, pool.decimals_a, pool.decimals_b) {
        Some(state) => {
            book.set(pool.address, Priced { state, slot, at });
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::pools::PoolBook;

    /// 97 real bytes of the deepest SOL/USDC Whirlpool, as base64.
    fn fixture_base64() -> String {
        const HEX: &str = "3f95d10ce180630913e441f83913ca68b0634fb025fdeaa88737e84110d1255e\
357b3377ddee1ccdff0400040090011405980f48d6e1c202000000000000000000710e1b5b51f5ff5200000000\
00000000a8ffff90b5dd290000000084742103";
        let bytes: Vec<u8> = (0..HEX.len() / 2)
            .map(|i| u8::from_str_radix(&HEX[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn notification(encoded: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"accountNotification","params":{{
                "result":{{"context":{{"slot":443961641}},"value":{{
                "lamports":70407360,"data":["{encoded}","base64"],
                "owner":"whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
                "executable":false,"rentEpoch":18446744073709551615}}}},
                "subscription":700}}}}"#
        )
    }

    #[test]
    fn a_real_notification_yields_the_slot_and_the_account_bytes() {
        let (subscription, slot, data) =
            parse_account_notification(&notification(&fixture_base64())).expect("parses");
        assert_eq!(subscription, 700);
        assert_eq!(slot, 443_961_641);
        // Enough bytes for the decoder to reach sqrt_price, which is the only
        // length that matters; the real account is 653 bytes.
        assert!(data.len() >= 81, "got {} bytes", data.len());
    }

    #[test]
    fn a_subscription_ack_is_not_a_price() {
        assert!(parse_account_notification(r#"{"jsonrpc":"2.0","result":700,"id":1}"#).is_none());
    }

    #[test]
    fn junk_never_panics() {
        for text in ["", "{}", "not json", r#"{"method":"accountNotification"}"#] {
            assert!(parse_account_notification(text).is_none());
        }
    }

    #[test]
    fn absorbing_a_notification_puts_a_usable_price_in_the_book() {
        let book = PriceBook::default();
        let pools = PoolBook::demo();
        let pool = pools.all()[0];
        let (_, slot, data) =
            parse_account_notification(&notification(&fixture_base64())).expect("parses");
        assert!(absorb(&book, &pool, slot, &data, Instant::now()));
        let priced = book.get(&pool.address).expect("priced");
        assert!((priced.state.price - 105.1).abs() < 0.5);
        assert_eq!(priced.slot, 443_961_641);
        assert_eq!(pool.pair, "SOL/USDC");
    }

    #[test]
    fn an_account_that_does_not_decode_leaves_the_book_alone() {
        // A pool that migrated, or a wrong address, must not put a garbage
        // price in the book where it would look like a huge opportunity.
        let book = PriceBook::default();
        let pool = PoolBook::demo().all()[0];
        assert!(!absorb(&book, &pool, 1, &[0u8; 20], Instant::now()));
        assert!(book.is_empty());
    }
}

/// Keep the book fresh from an RPC WebSocket, reconnecting for as long as it
/// runs. One `accountSubscribe` per pool, and a map from the subscription id
/// the server hands back to the pool it belongs to: attributing a price to the
/// wrong pool would invent a gap out of two unrelated markets.
pub async fn subscribe_pools(
    ws_url: String,
    pools: std::sync::Arc<crate::bot::pools::PoolBook>,
    book: std::sync::Arc<PriceBook>,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        let outcome = stream_once(&ws_url, &pools, &book).await;
        // Whatever ended the stream, every price in the book is now of unknown
        // age. Dropping them costs a few seconds of coverage; keeping them
        // costs correctness for as long as the outage lasted.
        book.clear();
        match outcome {
            Ok(()) => backoff = std::time::Duration::from_secs(1),
            Err(err) => tracing::warn!(%err, "pool prices dropped; reconnecting in {backoff:?}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
    }

    async fn stream_once(
        ws_url: &str,
        pools: &crate::bot::pools::PoolBook,
        book: &PriceBook,
    ) -> anyhow::Result<()> {
        let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await?;
        for (index, pool) in pools.all().iter().enumerate() {
            let request = serde_json::json!({
                "jsonrpc": "2.0", "id": index + 1, "method": "accountSubscribe",
                "params": [pool.address.to_string(), {"encoding": "base64", "commitment": "processed"}],
            });
            socket.send(Message::Text(request.to_string())).await?;
        }
        tracing::info!(pools = pools.all().len(), "subscribed to pool accounts");

        let mut by_subscription: HashMap<u64, usize> = HashMap::new();
        // A socket that dies without a FIN would otherwise leave this await
        // pending forever, and the book quietly frozen at the last prices.
        const SILENCE_LIMIT: std::time::Duration = std::time::Duration::from_secs(45);
        loop {
            let next = tokio::time::timeout(SILENCE_LIMIT, socket.next()).await;
            let Ok(Some(message)) = next else {
                anyhow::bail!("no pool update for {SILENCE_LIMIT:?}");
            };
            let text = match message? {
                Message::Text(text) => text,
                Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Message::Ping(payload) => {
                    socket.send(Message::Pong(payload)).await?;
                    continue;
                }
                Message::Close(_) => return Ok(()),
                _ => continue,
            };
            if let Some((id, subscription)) = parse_ack(&text) {
                if let Some(index) = id.checked_sub(1) {
                    by_subscription.insert(subscription, index as usize);
                }
                continue;
            }
            let Some((subscription, slot, data)) = parse_account_notification(&text) else {
                continue;
            };
            let at = Instant::now();
            let Some(pool) = by_subscription.get(&subscription).and_then(|i| pools.all().get(*i))
            else {
                continue;
            };
            if !absorb(book, pool, slot, &data, at) {
                tracing::warn!(pool = pool.label, "account did not decode as a Whirlpool");
            }
        }
    }
}

/// `(request id, subscription id)` out of a subscribe acknowledgement.
pub fn parse_ack(text: &str) -> Option<(u64, u64)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let id = value.get("id")?.as_u64()?;
    let subscription = value.get("result")?.as_u64()?;
    Some((id, subscription))
}
