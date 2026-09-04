//! Measure and use the DoubleZero Edge Solana shred feed.
//!
//! Two subcommands:
//!
//!   probe  count what actually arrives on a multicast group. Works on any DZ
//!          group this host is subscribed to, so the capture path can be proven
//!          before the shred feed is provisioned.
//!   race   the real thing: shreds on one side, ordinary RPC WebSockets on the
//!          other, one host, one clock, matched by transaction signature.
//!
//! Both need CAP_NET_RAW: shreds arrive on a GRE tunnel where an ordinary UDP
//! socket receives nothing (see capture.rs).

mod bot;
mod capture;
mod pipeline;
mod race;
mod rehearse;
mod rpc_lane;
mod serve;
mod sources;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use solana_stream_sdk::shreds_udp::ShredsUdpConfig;
use tokio::sync::{mpsc, RwLock};

use crate::bot::arb::Costs;
use crate::bot::config::BotLimits;
use crate::bot::duel::DuelBook;
use crate::bot::execute::{ExecutionStats, Executor, Mode};
use crate::bot::lane::{LaneBot, LaneStats, TriggerEvent};
use crate::bot::pools::PoolBook;
use crate::bot::prices::{subscribe_pools, PriceBook};
use crate::capture::{CapturedPacket, Filter};
use crate::pipeline::{SeenTx, ShredPipeline};
use crate::race::{Race, DEFAULT_REFERENCE_LANE};
use crate::rpc_lane::RpcLane;

#[derive(Parser)]
#[command(about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Count packets on a multicast group, to prove the capture path works.
    Probe {
        #[arg(long, default_value = "doublezero1")]
        iface: String,
        #[arg(long)]
        group: Ipv4Addr,
        #[arg(long = "port", required = true)]
        ports: Vec<u16>,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Race the shred feed against ordinary RPC WebSockets.
    Race {
        #[arg(long, default_value = "doublezero1")]
        iface: String,
        /// Leader shreds group. Root/retransmit groups can be added with
        /// repeated --group once the feed is provisioned.
        #[arg(long, default_value = "233.84.178.1")]
        group: Ipv4Addr,
        #[arg(long = "port", default_values_t = [7733u16])]
        ports: Vec<u16>,
        /// A lane as name=wss://url, or name=env:VAR to take the URL from the
        /// environment. Use env: for anything carrying a key: a URL in argv is
        /// readable by every user on the box, in ps and in systemctl status.
        #[arg(long = "lane", required = true)]
        lanes: Vec<String>,
        #[arg(long, default_value = "data/solana_race.json")]
        out: PathBuf,
        #[arg(long, default_value_t = 60.0)]
        window_min: f64,
        #[arg(long, default_value_t = 1000)]
        flush_ms: u64,
        /// Lane everything is measured against. Defaults to the shred feed;
        /// point it at an RPC lane to exercise the matcher without shreds.
        #[arg(long, default_value = DEFAULT_REFERENCE_LANE)]
        reference: String,
        /// Serve the dashboard here. Omit to run headless.
        #[arg(long)]
        http: Option<SocketAddr>,
        /// RPC WebSocket the pool prices are read from, or env:VAR.
        #[arg(long)]
        pool_ws: Option<String>,
        /// Watch the demo pools instead of every transaction. The RPC lanes
        /// subscribe per pool, which is the only way that side learns what a
        /// transaction touched.
        #[arg(long, default_value_t = false)]
        watch_pools: bool,
        /// Size of one arbitrage attempt, in lamports.
        #[arg(long, default_value_t = 5_000_000)]
        trade_lamports: u64,
        /// How much better than break-even a gap has to be before the bot acts.
        #[arg(long, default_value_t = 5)]
        margin_bps: u64,
        /// What landing the atomic transaction costs, win or lose.
        #[arg(long, default_value_t = 5_000)]
        network_fee_lamports: u64,
        /// How old a pool price may be and still be acted on.
        #[arg(long, default_value_t = 5_000)]
        max_price_age_ms: u64,
        /// Most a lane may commit in one session.
        #[arg(long, default_value_t = 100_000_000)]
        session_cap_lamports: u64,
        /// Refuse to trade a wallet holding more than this. Guards against
        /// pointing the demo at a funded wallet by mistake.
        #[arg(long, default_value_t = 200_000_000)]
        max_wallet_lamports: u64,
        /// While this file exists, nothing is sent, whatever else is set.
        #[arg(long, default_value = "data/KILL")]
        kill_file: PathBuf,
        /// dry-run decides and records; simulate builds, signs and asks the
        /// cluster what would happen; live sends. Live needs a broker and a
        /// wallet, and refuses to start without them rather than looking armed.
        #[arg(long, default_value = "dry-run")]
        mode: String,
        /// Another terminal on this host to offer beside this one, as
        /// host:port. Its state is proxied at /api/beside and the page grows a
        /// switch. Loopback only: this is one process reading another's screen.
        #[arg(long)]
        beside: Option<SocketAddr>,
        /// Wallet the balance is read from, as a base58 address. Read only:
        /// nothing here can sign, and the key never comes near this process.
        #[arg(long)]
        wallet: Option<String>,
    },
    /// Drive the terminal with invented data, to rehearse the stream before
    /// the shred feed exists. Every number is made up and the page says so.
    Rehearse {
        #[arg(long, default_value = "127.0.0.1:8091")]
        http: SocketAddr,
        /// Same seed replays the same session, so takes can be repeated.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value = "doublezero1")]
        iface: String,
        #[arg(long, default_value = "233.84.178.1")]
        group: Ipv4Addr,
        #[arg(long = "port", default_values_t = [7733u16])]
        ports: Vec<u16>,
        #[arg(long = "lane", default_values_t = [String::from("commercial=env:RPC_WS_COMMERCIAL")])]
        lanes: Vec<String>,
    },
}

async fn run_rehearsal(
    http: SocketAddr,
    seed: u64,
    feeds: Vec<crate::sources::Source>,
) -> Result<()> {
    let shared: serve::Shared = std::sync::Arc::new(tokio::sync::RwLock::new(String::from("{}")));
    tokio::spawn(serve::serve(http, shared.clone()));
    tracing::info!(%http, "rehearsal: every number on this page is invented");
    let mut session = crate::rehearse::Rehearsal::new(seed);
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    loop {
        ticker.tick().await;
        let value = session.step(&feeds);
        *shared.write().await = serde_json::to_string(&value)?;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Probe { iface, group, ports, seconds } => {
            probe(&iface, group, ports, Duration::from_secs(seconds)).await
        }
        Command::Rehearse { http, seed, iface, group, ports, lanes } => {
            let mut feeds = vec![crate::sources::shred_source(&iface, group, &ports)];
            feeds.extend(lanes.iter().map(|spec| crate::sources::lane_source(spec)));
            run_rehearsal(http, seed, feeds).await
        }
        Command::Race {
            iface, group, ports, lanes, out, window_min, flush_ms, reference, http,
            watch_pools, trade_lamports, margin_bps, network_fee_lamports, pool_ws,
            max_price_age_ms, session_cap_lamports, max_wallet_lamports, kill_file,
            mode, wallet, beside,
        } => {
            let mode = match mode.as_str() {
                "dry-run" => Mode::DryRun,
                "simulate" => Mode::Simulate,
                "live" => Mode::Live,
                other => anyhow::bail!(
                    "--mode {other:?} is not one of dry-run, simulate, live"
                ),
            };
            // A live run with nothing able to sign would look armed on the
            // screen and send nothing, which is the worst of both.
            anyhow::ensure!(
                mode == Mode::DryRun,
                "--mode {mode:?} needs a broker, and none is wired in yet: \
                 there is no wallet on this host and nothing here can sign"
            );
            let pool_ws = match pool_ws.as_deref().and_then(|v| v.strip_prefix("env:")) {
                Some(var) => Some(
                    std::env::var(var).with_context(|| format!("${var} is not set"))?,
                ),
                None => pool_ws,
            };
            let mut feeds = vec![crate::sources::shred_source(&iface, group, &ports)];
            feeds.extend(lanes.iter().map(|spec| crate::sources::lane_source(spec)));
            let lanes = lanes
                .iter()
                .map(|spec| parse_lane(spec, &reference))
                .collect::<Result<Vec<_>>>()?;
            anyhow::ensure!(
                reference == DEFAULT_REFERENCE_LANE
                    || lanes.iter().any(|lane| lane.name == reference),
                "reference lane {reference:?} is not one of the lanes given"
            );
            run_race(
                &iface,
                group,
                ports,
                lanes,
                reference,
                out,
                Duration::from_secs_f64(window_min * 60.0),
                Duration::from_millis(flush_ms),
                http,
                watch_pools,
                Costs {
                    network_fee_lamports,
                    margin_bps,
                    max_price_age: Duration::from_millis(max_price_age_ms),
                },
                trade_lamports,
                pool_ws,
                BotLimits {
                    per_attempt_lamports: trade_lamports,
                    session_cap_lamports,
                    kill_file,
                    max_wallet_lamports,
                },
                feeds,
                mode,
                beside,
            )
            .await
        }
    }
}

fn parse_lane(spec: &str, reference: &str) -> Result<RpcLane> {
    let (name, url) = spec
        .split_once('=')
        .with_context(|| format!("lane must be name=wss://url, got {spec:?}"))?;
    anyhow::ensure!(
        name != DEFAULT_REFERENCE_LANE || reference != DEFAULT_REFERENCE_LANE,
        "{DEFAULT_REFERENCE_LANE:?} names the shred feed and cannot be an RPC lane"
    );
    let url = match url.strip_prefix("env:") {
        Some(var) => std::env::var(var)
            .with_context(|| format!("lane {name:?} wants ${var}, which is not set"))?,
        None => url.to_string(),
    };
    anyhow::ensure!(
        url.starts_with("ws://") || url.starts_with("wss://"),
        "lane {name:?} needs a ws:// or wss:// URL"
    );
    Ok(RpcLane::new(name, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lane_url_can_come_from_the_environment() {
        // A URL in argv is readable by every user on the box. Keys belong in
        // the environment, and the error when the variable is missing must not
        // be a lane that silently never connects.
        std::env::set_var("SOL_EDGE_TEST_URL", "wss://provider.example/?api-key=k");
        let lane = parse_lane("commercial=env:SOL_EDGE_TEST_URL", "commercial").unwrap();
        assert_eq!(lane.name, "commercial");
        assert!(lane.scrub("connect to provider.example failed").contains("<commercial>"));

        let missing = parse_lane("commercial=env:SOL_EDGE_TEST_ABSENT", "commercial");
        assert!(missing.is_err());
    }

    #[test]
    fn a_lane_must_be_a_websocket() {
        assert!(parse_lane("public=https://api.example", "doublezero").is_err());
        assert!(parse_lane("public", "doublezero").is_err());
    }
}

async fn probe(iface: &str, group: Ipv4Addr, ports: Vec<u16>, run_for: Duration) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _reader = capture::spawn(iface, Filter::new(group, ports.clone()), tx)?;
    tracing::info!(%group, ?ports, iface, "probing for {run_for:?}");

    let deadline = Instant::now() + run_for;
    let (mut packets, mut bytes) = (0u64, 0usize);
    let mut smallest = usize::MAX;
    let mut largest = 0usize;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(packet)) => {
                packets += 1;
                bytes += packet.payload.len();
                smallest = smallest.min(packet.payload.len());
                largest = largest.max(packet.payload.len());
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    if packets == 0 {
        tracing::warn!(
            "no packets. Either this host is not subscribed to {group}, or the \
             interface name is wrong. Check: doublezero status"
        );
    } else {
        tracing::info!(
            packets,
            bytes,
            per_second = packets as f64 / run_for.as_secs_f64(),
            payload_min = smallest,
            payload_max = largest,
            "capture works"
        );
    }
    Ok(())
}

async fn run_race(
    iface: &str,
    group: Ipv4Addr,
    ports: Vec<u16>,
    lanes: Vec<RpcLane>,
    reference: String,
    out: PathBuf,
    window: Duration,
    flush: Duration,
    http: Option<SocketAddr>,
    watch_pools: bool,
    costs: Costs,
    trade_lamports: u64,
    pool_ws: Option<String>,
    limits: BotLimits,
    feeds: Vec<crate::sources::Source>,
    mode: Mode,
    beside: Option<SocketAddr>,
) -> Result<()> {
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel::<CapturedPacket>();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<(String, SeenTx)>();
    let _reader = capture::spawn(iface, Filter::new(group, ports), packet_tx)?;

    // Pools are known, checked addresses; prices are pushed to us.
    let started = Instant::now();
    let mut ledger = crate::bot::ledger::Ledger::default();
    let pools = Arc::new(PoolBook::demo());
    let prices = Arc::new(PriceBook::default());
    if watch_pools {
        let ws = pool_ws.clone().context(
            "--pool-ws is required with --watch-pools: pool prices come from an RPC WebSocket",
        )?;
        let pools = pools.clone();
        let prices = prices.clone();
        tokio::spawn(async move {
            if let Err(err) = subscribe_pools(ws, pools, prices).await {
                tracing::error!(%err, "pool prices stopped");
            }
        });
    }
    let bot_lane_names: Vec<String> = std::iter::once(DEFAULT_REFERENCE_LANE.to_string())
        .chain(lanes.iter().map(|lane| lane.name.clone()))
        .collect();
    let watched: Vec<String> =
        pools.all().iter().map(|pool| pool.address.to_string()).collect();
    for lane in lanes {
        let lane = if watch_pools { lane.watching(watched.clone()) } else { lane };
        let seen_tx = seen_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = lane.clone().run(seen_tx).await {
                tracing::error!(lane = %lane.name, %err, "lane stopped");
            }
        });
    }

    let mut pipeline = ShredPipeline::new(ShredsUdpConfig::defaults());
    let mut race = Race::new(window, &reference);
    let mut triggers: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    // One bot per lane. There is no thread and no queue: with prices in
    // memory a decision is arithmetic, and putting a queue in front of it
    // would add a delay to the very thing being measured.
    let mut bots: std::collections::HashMap<String, LaneBot> = std::collections::HashMap::new();
    // One executor per lane, alongside its bot. No broker is wired in: nothing
    // can be sent until a wallet exists, and the executor still counts what it
    // was offered so the terminal can show what a live run would have tried.
    let mut executors: std::collections::HashMap<String, Executor> =
        std::collections::HashMap::new();
    if watch_pools {
        for name in bot_lane_names {
            bots.insert(
                name.clone(),
                LaneBot::new(&name, pools.clone(), prices.clone(), costs, trade_lamports),
            );
            executors.insert(name.clone(), Executor::new(&name, limits.clone(), mode));
        }
    }
    let mut duels = DuelBook::new(Duration::from_secs(600));
    let mut ticker = tokio::time::interval(flush);

    let shared: serve::Shared = Arc::new(RwLock::new("{}".to_string()));
    if let Some(addr) = http {
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Err(err) = serve::serve_with(addr, shared, beside).await {
                tracing::error!(%err, "dashboard stopped");
            }
        });
    }

    loop {
        tokio::select! {
            Some(packet) = packet_rx.recv() => {
                for tx in pipeline.on_packet(&packet).await {
                    offer_trigger(
                        DEFAULT_REFERENCE_LANE, &tx, &pools, &mut bots, &mut executors,
                        &mut triggers, &mut duels, &mut ledger, started, 0,
                    );
                    race.observe(DEFAULT_REFERENCE_LANE, &tx);
                }
            }
            Some((lane, tx)) = seen_rx.recv() => {
                offer_trigger(
                    &lane, &tx, &pools, &mut bots, &mut executors, &mut triggers, &mut duels,
                    &mut ledger, started, 0,
                );
                race.observe(&lane, &tx);
            }
            _ = ticker.tick() => {
                race.evict();
                duels.evict();
                let stats: std::collections::HashMap<String, LaneStats> =
                    bots.iter().map(|(name, bot)| (name.clone(), bot.stats())).collect();
                let execution: std::collections::HashMap<String, ExecutionStats> = executors
                    .iter()
                    .map(|(name, executor)| (name.clone(), executor.stats()))
                    .collect();
                let markets = crate::bot::market::snapshot(
                    &pools, &prices, costs.margin_bps, costs.max_price_age, Instant::now(),
                );
                let pool_rows = crate::bot::market::pool_rows(&pools, &prices, Instant::now());
                let json = write_snapshot(
                    &out, &race, &pipeline, &triggers, &stats, &duels, &execution, &markets,
                    &pool_rows, &feeds, &ledger, mode,
                )?;
                *shared.write().await = json;
            }
            else => break,
        }
    }
    Ok(())
}

/// Write the snapshot to disk and hand the same JSON back for the dashboard.
/// Count the trigger and let that lane's bot judge it, once per watched pool.
///
/// Once per pool, not once per transaction. A router transaction can touch two
/// watched pools; the shred lane sees every key while an RPC lane learns about
/// exactly the one pool its subscription watches. Emitting one event per pool
/// is what lets the two lanes be compared on the same market.
fn offer_trigger(
    lane: &str,
    tx: &SeenTx,
    pools: &PoolBook,
    bots: &mut std::collections::HashMap<String, LaneBot>,
    executors: &mut std::collections::HashMap<String, Executor>,
    triggers: &mut std::collections::HashMap<String, u64>,
    duels: &mut DuelBook,
    ledger: &mut crate::bot::ledger::Ledger,
    started: Instant,
    wallet_lamports: u64,
) {
    let touched: Vec<_> = tx
        .account_keys
        .iter()
        .filter_map(|key| pools.find(key))
        .map(|pool| pool.address)
        .collect();
    if touched.is_empty() {
        return;
    }
    *triggers.entry(lane.to_string()).or_default() += 1;
    let Some(bot) = bots.get_mut(lane) else {
        return;
    };
    for pool in touched {
        let event = TriggerEvent {
            pool,
            signature: tx.signature.clone(),
            slot: tx.slot,
            at: tx.at,
        };
        // Judged as of now: the price book is what it is at the moment this
        // lane got round to looking, which is the point. The head start itself
        // is timed off the feed's arrival, inside the duel book.
        if let Some(found) = bot.on_trigger(&event, Instant::now()) {
            if let (Some(opportunity), Some(executor)) =
                (found.opportunity.as_ref(), executors.get_mut(lane))
            {
                // No broker and no wallet, so this can only ever refuse or say
                // "dry run". It is here so the limits are exercised every time
                // rather than first meeting real money on the day it matters.
                let outcome = executor.attempt(opportunity, wallet_lamports, None);
                ledger.record(crate::bot::ledger::Trade {
                    at_s: started.elapsed().as_secs_f64(),
                    slot: event.slot,
                    lane: lane.to_string(),
                    pair: found.pair_label,
                    route: format!("{} -> {}", opportunity.sell_on, opportunity.buy_on),
                    size_lamports: opportunity.in_lamports,
                    gross_bps: opportunity.gross_bps,
                    net_lamports: opportunity.net_lamports,
                    booking: booking_of(&outcome),
                    outcome: describe(&outcome),
                });
            }
            duels.record(lane, &found);
        }
    }
}

/// How much of an attempt is real, from what the executor did with it.
fn booking_of(outcome: &crate::bot::execute::Outcome) -> crate::bot::ledger::Booking {
    use crate::bot::execute::{Outcome, Reason};
    use crate::bot::ledger::Booking;
    match outcome {
        Outcome::Sent { .. } => Booking::Realised,
        Outcome::Simulated { ok: true, .. } => Booking::Simulated,
        // A dry run decided and stopped, which is a find rather than a refusal.
        Outcome::Skipped(Reason::DryRun) => Booking::Expected,
        _ => Booking::None,
    }
}

/// The outcome in the words the screen uses, so the page never has to know
/// the shape of the enum.
fn describe(outcome: &crate::bot::execute::Outcome) -> String {
    use crate::bot::execute::{Outcome, Reason};
    use crate::bot::config::Refusal;
    match outcome {
        Outcome::Sent { signature } => format!("sent {}", &signature[..signature.len().min(12)]),
        Outcome::Simulated { ok: true, .. } => "simulated ok".to_string(),
        Outcome::Simulated { note, .. } => format!("simulation failed: {note}"),
        Outcome::Skipped(Reason::DryRun) => "dry run".to_string(),
        Outcome::Skipped(Reason::BuildFailed(why)) => format!("build failed: {why}"),
        Outcome::Skipped(Reason::Limit(Refusal::Killed)) => "stopped by kill file".to_string(),
        Outcome::Skipped(Reason::Limit(Refusal::SessionCap)) => "over the session cap".to_string(),
        Outcome::Skipped(Reason::Limit(Refusal::WalletTooLarge)) => "wallet too large".to_string(),
    }
}

fn write_snapshot(
    out: &PathBuf,
    race: &Race,
    pipeline: &ShredPipeline,
    triggers: &std::collections::HashMap<String, u64>,
    bots: &std::collections::HashMap<String, LaneStats>,
    duels: &DuelBook,
    execution: &std::collections::HashMap<String, ExecutionStats>,
    markets: &[crate::bot::market::MarketRow],
    pool_rows: &[crate::bot::market::PoolRow],
    feeds: &[crate::sources::Source],
    ledger: &crate::bot::ledger::Ledger,
    mode: Mode,
) -> Result<String> {
    let counts = pipeline.counts();
    let mut value = serde_json::to_value(race.snapshot())?;
    value["shreds"] = serde_json::json!({
        "packets": counts.packets,
        "shreds": counts.shreds,
        "batches_ready": counts.batches_ready,
        "deshred_errors": counts.deshred_errors,
        "panics": counts.panics,
        "entries": counts.entries,
        "transactions": counts.transactions,
    });
    value["triggers"] = serde_json::to_value(triggers)?;
    value["bots"] = serde_json::to_value(bots)?;
    value["duels"] = serde_json::to_value(duels.recent())?;
    value["duel_wins"] = serde_json::to_value(duels.wins())?;
    value["duel_paired"] = serde_json::json!(duels.paired());
    value["execution"] = serde_json::to_value(execution)?;
    value["markets"] = serde_json::to_value(markets)?;
    value["pools"] = serde_json::to_value(pool_rows)?;
    value["sources"] = serde_json::to_value(feeds)?;
    value["trades"] = serde_json::to_value(ledger.recent())?;
    value["money"] = serde_json::to_value(ledger.totals())?;
    value["mode"] = serde_json::to_value(match mode {
        Mode::DryRun => "dry run",
        Mode::Simulate => "simulate",
        Mode::Live => "live",
    })?;
    // No wallet is attached on this host, and a balance of zero would read as
    // an empty wallet rather than as an absent one.
    value["wallet"] = serde_json::Value::Null;
    value["updated_at"] = serde_json::json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
    );
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string(&value)?;
    let tmp = out.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, out)?;
    Ok(json)
}
