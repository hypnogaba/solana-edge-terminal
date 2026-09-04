# Stream demo: the feed, the packets, and two bots racing

Design agreed 2026-09-03. Implementation follows this document.

## What the stream has to show

Three things, in this order, on one page plus a terminal:

1. **Connecting to the feed is one command.** The page shows a grey, disconnected
   state. The command runs on camera. Twenty to thirty seconds later the panel
   turns green and the counters start moving.
2. **Data is arriving, and it is early.** Live counters for what the feed
   delivers and what we decode out of it, and the slot currently being built.
3. **A bot trades on it by itself, and being early is what wins.** Two identical
   bots, one on the DoubleZero shred feed and one on an ordinary RPC WebSocket,
   each with its own wallet, competing for the same opportunities.

## Where the opportunities come from

The same token pair trades in several pools at once (Raydium, Orca, Meteora).
A large swap moves the price in one pool and not yet in the others, so for a few
hundred milliseconds the pools disagree. That disagreement is the opportunity:
buy where it is cheap, sell where it is dear.

The swap that creates the gap arrives in the shred feed, so the source of
opportunities is the feed itself. No external signal service is involved.

Both legs go in **one atomic transaction**: both execute or neither does. No
token is ever held, so there is no rug risk and no position to manage. A failed
attempt costs the network fee, about 0.000005 SOL, and nothing else. That
property is what makes trading live on air acceptable.

## What we claim, and what we do not

**Claim:** with the feed you see the opportunity earlier, and the earlier bot
takes it. Measured on one machine, one clock, matched by transaction signature.

**Do not claim** that this beats professional MEV operations. They run beside
validators and will win most races against a box in Frankfurt. Our two bots race
each other, and that comparison is the product claim.

**Do not claim** that it is profitable. At 0.005 SOL per attempt the money is
noise. The headline number is the head start in milliseconds and who took the
opportunity, never dollars.

**Known simplification:** the bots quote through Jupiter rather than keeping pool
reserves in memory, which adds roughly 100 ms before either can act. A
production bot would keep its own pool state. Both lanes pay the same delay, so
the comparison stays fair, but the absolute reaction time is slower than a real
operation's and the stream should say so.

## Architecture

Built on the existing `solana-edge-lab` binary, which already reads the feed and
races lanes. Three new pieces:

### 1. `pools` — what the bot knows about the market

Tracks a small, fixed set of pools for a few pairs. For each pool: which program,
which accounts, the last known reserves, and when they were last refreshed.
Reserves come from the RPC the bot is configured with; the feed supplies the
trigger, not the state.

Input: a decoded transaction. Output: "this transaction swapped in pool X".

### 2. `arb` — the decision, identical for both bots

Given a trigger swap in pool X for pair P, ask whether P is now out of line
across the pools we track by more than the round-trip cost (both pool fees, the
network fee, and a configured margin). If yes, produce an `Opportunity`: the
pair, the two pools, the direction, the size, and the gap in basis points.

Pure function of (trigger, pool state, config). No I/O, no wallet, no clock.
This is what makes the two bots provably identical: they call the same function.

### 3. `execute` — one wallet per lane

Builds the atomic swap, signs it with that lane's keypair, sends it, and records
what happened: submitted at, landed in slot, or failed and why.

Hard limits, enforced here and not in the strategy:
- 0.005 SOL per attempt
- 0.1 SOL cumulative per wallet per session
- a kill file: present means dry-run, whatever else is configured
- refuses to start if either wallet holds more than a configured maximum

### How they connect

One process, two lanes, as with the Kalshi lab and for the same reason: one
clock, and the decision code is provably the same because it is literally the
same call. The two wallets are real and their transactions are on chain, so the
"it is one process" objection is answered by anyone who checks the signatures.

```
shred feed ─→ decode ─→ trigger ─┐
                                 ├─→ arb::evaluate ─→ execute(wallet A)
RPC WebSocket ─→ trigger ────────┘   (same function)   execute(wallet B)
```

Each lane keeps its own trigger stream and its own wallet. An opportunity seen by
both lanes is paired by the trigger transaction's signature, which gives the head
start in milliseconds directly.

## The page

One page, served by the same binary, three sections matching the three things
above. Dark, large type, readable off a 1080p capture. Design canvas:
`design/Main.dc.html` and `design/BeforeConnect.dc.html`.

Section 3 shows, per opportunity: slot, pair, which two pools were out of line,
the gap in basis points, which lane saw it first and by how many milliseconds,
which bot took it, and the transaction signature. The wallet cards show each
bot's address, how many it landed, and its balance, so the audience can see real
money moving.

## Testing

Follows the same rule as the rest of this repo: test the behaviour that would
silently corrupt a measurement, not the constants.

- `arb::evaluate` is pure, so it gets table tests: a gap below cost yields
  nothing; a gap above cost yields the right direction and size; a stale pool
  reading is refused rather than treated as zero.
- The pairing of an opportunity across lanes gets the same treatment as the race
  matcher: a lane is credited only with what it delivered, and a repeated trigger
  cannot improve a lane's own number.
- Execution is tested against a local simulation, never by sending on mainnet in
  a test.
- The limits get their own tests, including that the kill file wins over every
  other setting.

## Open, to settle before the stream

- Two wallets have to be created and funded with about 0.15 SOL each.
- The feed `solana-shreds-full-fra` still has to be provisioned on the access
  pass; everything else runs without it, and the page says so on its face.
- Which pairs to track. SOL/USDC is certain; two or three more decided from what
  the feed actually shows once it is live.
