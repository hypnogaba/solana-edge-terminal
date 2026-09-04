# solana-edge-terminal

**The same market, seen twice.** A transaction reaches this machine two ways:
as shreds over [DoubleZero](https://doublezero.xyz) Edge, while the block is
still being built, and over an ordinary RPC WebSocket, once a node has
processed it and decided to tell you. This measures the gap between those two
moments, and then shows what the gap is worth: two identical arbitrage bots,
one on each feed, looking at the same event a few hundred milliseconds apart.

Everything is measured on one host with one clock. A delta here is a real
difference in arrival, never skew between machines.

## What it does

```
DoubleZero shreds ──┐
                    ├── same trigger ── same decision function ── two outcomes
RPC WebSocket ──────┘
```

- **Reads the feed.** Shreds arrive as multicast on a GRE tunnel, where an
  ordinary UDP socket receives nothing. They are captured with AF_PACKET the
  way tcpdump does it, then decoded to entries and transactions.
- **Times both paths.** The same transaction is matched across lanes by its
  signature and timed on one monotonic clock.
- **Prices the market itself.** Orca Whirlpool accounts are read straight off
  the chain over `accountSubscribe` and decoded in memory, so the decision path
  makes no network call at all.
- **Decides identically on both lanes.** One pure function, called twice. The
  only difference between the two bots is when they were told to look.

## What it found

Two honest results from live running, both of which shaped the design.

**A quote service cannot sit in this path.** The first version asked Jupiter for
prices per trigger. It was rate limited out within seconds of a real trigger
rate: 1,236 of 1,465 quotes failed. Worse, it put a 130 ms network hop inside
the very thing being measured. Prices now come from the chain.

**Simple cross-pool arbitrage is almost never profitable at this size.** Over
3,881 triggers on 17 pools, the widest gap between two pools of the same pair
was 27 basis points, against fee tiers that start at 6 and go past 200. Other
bots close the real gaps within a slot. So the terminal reports the rejections
as first-class results, and the headline is not profit: it is that the two
lanes, looking at the same event, **do not see the same market**. The early
lane sees a gap the late lane no longer sees.

## Running it

Both subcommands need `CAP_NET_RAW`.

```bash
# Prove the capture path on any multicast group this host is subscribed to.
solana-edge-terminal probe --group 233.84.178.3 --port 31000 --port 41000

# The terminal.
solana-edge-terminal race \
  --group 233.84.178.1 --port 7733 \
  --reference doublezero \
  --lane public=env:RPC_WS_PUBLIC \
  --watch-pools --pool-ws env:RPC_WS_PUBLIC \
  --http 127.0.0.1:8090
```

A lane URL given as `name=env:VAR` is read from the environment. Never pass one
on the command line: argv carries the API key into `ps` and `systemctl status`,
where every user on the box can read it. Lane names come from the command line
and nothing else about an endpoint is ever printed, including inside the text
of DNS and TLS errors.

`deploy/sol-race.service` runs it under systemd.

## Honesty rules

These are enforced in tests, not just written down.

- One host, one clock.
- Matched by signature, the network's own identity for a transaction.
- A lane is credited only with what it delivered. A transaction one lane never
  reported is dropped from the comparison, so a feed that is fast but lossy
  cannot hide the loss inside a latency number.
- First arrival per lane wins; a repeated copy cannot improve a lane's number.
- The page says on its face when the shred feed is absent, rather than quietly
  comparing two RPC lanes while the viewer believes otherwise.
- Nothing is hardcoded that can be read from the source of truth, and what is
  hardcoded was checked against it. Two pool addresses written by hand were
  wrong and produced no triggers and no error at all.

## Layout

| Path | What it is |
|---|---|
| `src/capture.rs` | AF_PACKET reader for the DoubleZero tunnel |
| `src/pipeline.rs` | shreds to entries to transaction signatures |
| `src/rpc_lane.rs` | an RPC WebSocket as a lane, per-account or firehose |
| `src/race.rs` | matching and rolling latency statistics |
| `src/bot/whirlpool.rs` | pool price, decoded from account bytes |
| `src/bot/prices.rs` | the live price book, fed by `accountSubscribe` |
| `src/bot/arb.rs` | the decision, pure |
| `src/bot/lane.rs` | one lane's bot |
| `src/bot/duel.rs` | one event, as each lane saw it |
| `src/bot/config.rs` | hard limits, kept away from the strategy |
| `src/serve.rs` | the terminal page |
| `design/` | the page's design canvas |
| `docs/` | the spec and the implementation plan |

## Standing on

Shred decoding, FEC reconstruction and deshredding come from
[`solana-stream-sdk`](https://github.com/ValidatorsDAO/solana-stream)
(Apache-2.0), which exposes those as separate steps rather than only as a
socket loop. That matters here: DoubleZero delivers over a tunnel where the
SDK's own receiver would get nothing, so this owns the packets and borrows the
decoding.

## Status

The shred lane is not yet provisioned on this host's DoubleZero access pass, so
it is silent and the terminal says so. Everything around it is proven: the
capture path against a live multicast group, the decode path by a round trip
through the same `Shredder` a validator uses, and the matcher and the bots
against live mainnet traffic.

No orders are sent. Execution is written but disabled, and turning it on is a
deliberate, separate step behind hard limits and a kill file.

Apache-2.0.
