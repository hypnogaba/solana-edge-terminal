//! The public path, for comparison: an RPC WebSocket `logsSubscribe`.
//!
//! This is how a trader without the feed learns that a transaction exists. We
//! subscribe at `processed` commitment, which is the earliest an RPC will tell
//! anyone anything, so the baseline is the fastest form of the ordinary path
//! rather than a strawman.
//!
//! Votes are excluded on this side ("all" does not include them) and on the
//! shred side (`skip_vote_sigs`), so the two lanes see the same population.
//!
//! A lane can instead watch specific accounts, one `mentions` subscription per
//! address. That is what lets the RPC-fed bot exist at all: a logs notification
//! carries a signature and log lines but no account keys, so without `mentions`
//! this side could time a transaction and never tell what it touched.
//!
//! A lane never reveals where it points. The endpoint may be a commercial
//! provider whose name should not appear on a screen being broadcast, and the
//! query string usually carries an API key, so the host and any key are
//! scrubbed out of every log line, including the text of errors raised deep in
//! DNS or TLS. What is shown is the name the operator chose on the command
//! line, and nothing else. Snapshots only ever carry lane names.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message;

use crate::pipeline::SeenTx;

/// One named lane. `name` is what the scoreboard calls it.
#[derive(Debug, Clone)]
pub struct RpcLane {
    pub name: String,
    ws_url: String,
    host: String,
    /// Accounts to watch, one subscription each. Empty means the firehose.
    mentions: Vec<String>,
}

impl RpcLane {
    pub fn new(name: impl Into<String>, ws_url: impl Into<String>) -> Self {
        let ws_url = ws_url.into();
        Self { name: name.into(), host: host_of(&ws_url), ws_url, mentions: Vec::new() }
    }

    /// Watch these accounts instead of everything.
    pub fn watching(mut self, accounts: Vec<String>) -> Self {
        self.mentions = accounts;
        self
    }

    /// Remove anything identifying about the endpoint from text bound for a log.
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.replace(&self.ws_url, &format!("<{}>", self.name));
        if !self.host.is_empty() {
            out = out.replace(&self.host, &format!("<{}>", self.name));
        }
        redact_keys(&out)
    }

    /// Run until cancelled, reconnecting on failure. Never returns Ok.
    pub async fn run(self, tx: UnboundedSender<(String, SeenTx)>) -> Result<()> {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.connect_and_stream(&tx).await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(err) => {
                    tracing::warn!(
                        lane = %self.name,
                        error = %self.scrub(&format!("{err:#}")),
                        "lane dropped; reconnecting in {backoff:?}"
                    );
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    async fn connect_and_stream(&self, tx: &UnboundedSender<(String, SeenTx)>) -> Result<()> {
        let (mut socket, _) = tokio_tungstenite::connect_async(&self.ws_url)
            .await
            .with_context(|| format!("connect {}", self.name))?;
        tracing::info!(lane = %self.name, "connected");

        let mut watched = Watched::default();
        for request in subscribe_requests(&self.mentions) {
            socket.send(Message::Text(request.to_string())).await?;
        }

        while let Some(message) = socket.next().await {
            let message = message?;
            let text = match message {
                Message::Text(text) => text,
                Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Message::Ping(payload) => {
                    socket.send(Message::Pong(payload)).await?;
                    continue;
                }
                Message::Close(_) => break,
                _ => continue,
            };
            let at = Instant::now();
            // An ack maps our request id to the subscription id the server will
            // quote on every notification. Without that mapping a notification
            // cannot be attributed to the account it was watching, and the bot
            // would credit a trigger to the wrong pool.
            if watched.absorb_ack(&text, &self.mentions) {
                continue;
            }
            if let Some(mut seen) = parse_logs_notification(&text, at) {
                seen.account_keys = watched.accounts_for(&text);
                if tx.send((self.name.clone(), seen)).is_err() {
                    return Ok(()); // receiver gone
                }
            }
        }
        Ok(())
    }
}

/// One `logsSubscribe` request per watched account, or a single firehose
/// request when nothing is watched. Request ids start at 1 and index the list.
pub fn subscribe_requests(mentions: &[String]) -> Vec<serde_json::Value> {
    if mentions.is_empty() {
        return vec![serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "logsSubscribe",
            "params": ["all", {"commitment": "processed"}],
        })];
    }
    mentions
        .iter()
        .enumerate()
        .map(|(index, account)| {
            serde_json::json!({
                "jsonrpc": "2.0", "id": index + 1, "method": "logsSubscribe",
                "params": [{"mentions": [account]}, {"commitment": "processed"}],
            })
        })
        .collect()
}

/// Maps the server's subscription ids back to the accounts we asked about.
#[derive(Debug, Default)]
pub struct Watched {
    by_subscription: std::collections::HashMap<u64, String>,
}

impl Watched {
    /// Returns true when the frame was an ack and needs no further handling.
    pub fn absorb_ack(&mut self, text: &str, mentions: &[String]) -> bool {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return false;
        };
        let (Some(id), Some(subscription)) =
            (value.get("id").and_then(|v| v.as_u64()), value.get("result").and_then(|v| v.as_u64()))
        else {
            return false;
        };
        if let Some(account) = id.checked_sub(1).and_then(|i| mentions.get(i as usize)) {
            self.by_subscription.insert(subscription, account.clone());
        }
        true
    }

    /// The account a notification was watching, as a one-element key list.
    pub fn accounts_for(&self, text: &str) -> Vec<solana_pubkey::Pubkey> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Vec::new();
        };
        let Some(subscription) =
            value.get("params").and_then(|p| p.get("subscription")).and_then(|v| v.as_u64())
        else {
            return Vec::new();
        };
        self.by_subscription
            .get(&subscription)
            .and_then(|account| account.parse().ok())
            .map(|key| vec![key])
            .unwrap_or_default()
    }
}

/// Everything between `api-key=` (or `token=`) and the next separator.
fn redact_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("api-key=").or_else(|| rest.find("token=")) {
        let key_start = start + rest[start..].find('=').unwrap() + 1;
        out.push_str(&rest[..key_start]);
        out.push_str("REDACTED");
        let tail = &rest[key_start..];
        let end = tail
            .find(|c: char| c == '&' || c == ' ' || c == '"' || c == '\'' || c == ')')
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Host part of a ws:// or wss:// URL, without scheme, port, path or query.
fn host_of(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    host.split('@').next_back().unwrap_or(host)
        .split(':').next().unwrap_or(host)
        .to_string()
}

/// Pull the signature and slot out of a logsNotification.
///
/// Split out so it can be tested against a real captured frame: a silent parse
/// failure here would look exactly like "the public path is slow".
pub fn parse_logs_notification(text: &str, at: Instant) -> Option<SeenTx> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("method")?.as_str()? != "logsNotification" {
        return None;
    }
    let result = value.get("params")?.get("result")?;
    let slot = result.get("context")?.get("slot")?.as_u64()?;
    let signature = result.get("value")?.get("signature")?.as_str()?;
    // A logs notification carries no account keys, so this lane can time a
    // transaction but never trigger the bot off one.
    Some(SeenTx { signature: signature.to_string(), slot, at, account_keys: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape a real mainnet-beta logsSubscribe notification arrives in.
    const NOTIFICATION: &str = r#"{"jsonrpc":"2.0","method":"logsNotification","params":{
        "result":{"context":{"slot":378106471},"value":{
        "signature":"5vT9x8kCbA1YQ7t3nJ2mWq4pR6sD8fG1hK3jL5nP7qR9",
        "err":null,"logs":["Program 11111111111111111111111111111111 invoke [1]"]}},
        "subscription":24040}}"#;

    #[test]
    fn a_notification_yields_signature_and_slot() {
        let seen = parse_logs_notification(NOTIFICATION, Instant::now()).expect("parses");
        assert_eq!(seen.slot, 378106471);
        assert_eq!(seen.signature, "5vT9x8kCbA1YQ7t3nJ2mWq4pR6sD8fG1hK3jL5nP7qR9");
    }

    #[test]
    fn watching_nothing_asks_for_the_firehose() {
        let requests = subscribe_requests(&[]);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["params"][0], "all");
    }

    #[test]
    fn watching_accounts_asks_one_subscription_each() {
        let accounts = vec!["PoolA".to_string(), "PoolB".to_string()];
        let requests = subscribe_requests(&accounts);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["id"], 1);
        assert_eq!(requests[0]["params"][0]["mentions"][0], "PoolA");
        assert_eq!(requests[1]["id"], 2);
        assert_eq!(requests[1]["params"][0]["mentions"][0], "PoolB");
    }

    #[test]
    fn a_notification_is_attributed_to_the_account_its_subscription_watched() {
        // Getting this mapping wrong does not fail: it credits a trigger to the
        // wrong pool, and every number downstream stays plausible.
        let pool_a = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2".to_string();
        let pool_b = "HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ".to_string();
        let mentions = vec![pool_a.clone(), pool_b.clone()];
        let mut watched = Watched::default();
        assert!(watched.absorb_ack(r#"{"jsonrpc":"2.0","result":700,"id":1}"#, &mentions));
        assert!(watched.absorb_ack(r#"{"jsonrpc":"2.0","result":701,"id":2}"#, &mentions));

        let from_b = watched.accounts_for(
            r#"{"method":"logsNotification","params":{"result":{},"subscription":701}}"#,
        );
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_b[0].to_string(), pool_b);
    }

    #[test]
    fn a_notification_from_an_unknown_subscription_carries_no_account() {
        // Better an empty key list, which simply never triggers, than a guess.
        let watched = Watched::default();
        assert!(watched
            .accounts_for(r#"{"method":"logsNotification","params":{"subscription":9}}"#)
            .is_empty());
    }

    #[test]
    fn the_subscription_ack_is_not_a_transaction() {
        // Treating {"result":24040,"id":1} as data would put a bogus entry into
        // the race on every reconnect.
        assert!(parse_logs_notification(r#"{"jsonrpc":"2.0","result":24040,"id":1}"#,
                                        Instant::now()).is_none());
    }

    #[test]
    fn the_endpoint_never_reaches_a_log_line() {
        // On air, a DNS or TLS error must not be the thing that names the
        // provider we are measuring against.
        let lane = RpcLane::new(
            "commercial",
            "wss://rpc.example-provider.com/?api-key=abc123def456",
        );
        let raw = "failed to lookup address information for \
                   rpc.example-provider.com: nodename nor servname provided";
        let scrubbed = lane.scrub(raw);
        assert!(!scrubbed.contains("example-provider"), "got {scrubbed}");
        assert!(scrubbed.contains("<commercial>"));

        let with_key = lane.scrub("connect wss://rpc.example-provider.com/?api-key=abc123def456 failed");
        assert!(!with_key.contains("abc123def456"), "got {with_key}");
        assert!(!with_key.contains("example-provider"), "got {with_key}");
    }

    #[test]
    fn host_parsing_survives_the_url_shapes_we_actually_pass() {
        assert_eq!(host_of("wss://api.mainnet-beta.solana.com"), "api.mainnet-beta.solana.com");
        assert_eq!(host_of("wss://host.example.com:8899/path?api-key=x"), "host.example.com");
        assert_eq!(host_of("wss://user:pass@host.example.com/"), "host.example.com");
    }

    #[test]
    fn junk_never_panics() {
        for text in ["", "not json", "{}", r#"{"method":"logsNotification"}"#] {
            assert!(parse_logs_notification(text, Instant::now()).is_none());
        }
    }
}
