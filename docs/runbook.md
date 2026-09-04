# Runbook

Everything you need to run this, test it, and talk about it on air.

Addresses and identifiers are written as `<placeholders>`: this file is public,
and the host running it also runs other things. Fill in your own.

## The two terminals

| What | Address | What it shows |
|---|---|---|
| Solana | https://solana.<your-domain> | the shred feed, the pools, two bots on the same events |
| Kalshi | https://kalshi.<your-domain> | the latency benchmark, live since August |
| Kalshi duel | https://kalshi.<your-domain>/duel | two bots on the Kalshi feed |

Reach the host through a Cloudflare tunnel, so the machine's own address is
nowhere public and no inbound port is open. Anything of the shape
`<ip>.nip.io` carries the address in the hostname itself and is not a substitute;
`trycloudflare.com` names change on every restart, so never put one in a video
description.

## What is still missing

Three things, none of them code.

**1. The Solana shred feed.** The terminal runs without it and says so on its
face. To get it, ask DoubleZero to provision one feed on the access pass:

> Please provision feed `solana-shreds-full-fra`
> (`4Fc1Fyd1x8BoWYPWN8vFhbP6fpgayybQuLUSPRwfE7Wi`) on access pass
> `<your access pass>` — DZ ID
> `<your DZ ID>`, IP `<your host IP>`,
> Frankfurt (`fr2-dzx-001`). The seat shows `edge_seat: 2 feed(s)` with only
> `edge-kalshi-perps-tob` joined.

When it is granted, on the host:

```bash
doublezero connect multicast \
  --subscribe-feed kalshi-perps-tob solana-shreds-full
doublezero status          # expect both groups under Multicast Groups
systemctl restart sol-race
```

Passing BOTH feeds matters: the command sets the whole set, and passing only the
new one would drop the Kalshi feed the other terminal runs on.

**2. A second RPC key.** One key is not two lanes. Sharing the trading bot's key
gives `429 Too Many Requests` on every reconnect, which on screen looks exactly
like a dead feed. Any provider will do; put it in `/root/solana-edge-terminal/.env`:

```
RPC_WS_COMMERCIAL=wss://<your endpoint>/?api-key=<key>
RPC_WS_PUBLIC=wss://api.mainnet-beta.solana.com
RPC_WS_POOLS=wss://api.mainnet-beta.solana.com
```

`RPC_WS_POOLS` carries 17 account subscriptions and should be its own endpoint,
not one a lane is also using; they compete for the same connection budget. It
points at the public RPC today because that is the only endpoint answering.

Never put a URL with a key on the command line: `ps` and `systemctl status` show
it to every user on the box. The `env:VAR` form exists for this.

**3. Two wallets**, about 0.15 SOL each, if you want real orders. Nothing is
sent without them, and nothing is sent even with them until execution is
switched on deliberately.

## Running it

```bash
systemctl status sol-race          # the Solana terminal
systemctl restart sol-race
journalctl -u sol-race -f          # what it is doing

systemctl status dz-race edge-web  # the Kalshi side
```

Rebuilding after a change:

```bash
cd /root/solana-edge-terminal
cargo build --release && systemctl restart sol-race
```

`cargo test` does **not** refresh the release binary. Running the tests and then
restarting the service will silently run the old build. This has cost an hour
twice; always `cargo build --release`.

## Testing it before the video

Work down this list. Each step says what good looks like.

**1. The feed is up.**

```bash
doublezero status
```

Expect `BGP Session Up`, `Frankfurt`, and the groups you subscribed to. If
`Multicast Groups` does not list a Solana group, the feed is not provisioned and
step 2 will be empty.

**2. Packets are actually arriving.**

```bash
cd /root/solana-edge-terminal
./target/release/solana-edge-terminal probe --group 233.84.178.1 --port 7733 --seconds 10
```

Expect `capture works` with tens of thousands of packets. `no packets` means the
group is not joined, whatever `status` says.

**3. The terminal is live.**

```bash
curl -s https://solana.<your-domain>/api/state | head -c 400
```

Expect `shreds` counters climbing and `triggers` for each lane. Open the page and
check the top left corner says `FEED UP` in green.

**4. Both lanes are producing triggers.** In the page's right column, both bot
panels must show `triggers seen` climbing. If one is stuck at zero, that lane is
not connected: check `journalctl -u sol-race -n 30` for `429` or `413`.

**5. Events are being paired.** The blotter fills only with events **both** lanes
saw. If it stays on "waiting for an event both lanes saw", step 4 is the reason.

**6. Let it soak.** Leave it running for at least an hour before the stream. The
statistics live in memory, so a restart clears them, and a blotter with four rows
looks like a prototype.

**7. Rehearse the reconnect.** The most watchable moment is the feed coming up on
camera, and it is also the riskiest. Practise it once:

```bash
doublezero disconnect
# the page goes red, counters stop
doublezero connect multicast --subscribe-feed kalshi-perps-tob solana-shreds-full
# 20 to 30 seconds later the page goes green and the counters move
systemctl restart sol-race
```

Do this at least once before you are live. If it takes longer than a minute, do
not do it on air.

## On air

Three beats, in this order.

**The pipe.** One command in a terminal, and the page changes. That is the whole
onboarding story: a Linux box with a public IP, an access pass, one `connect`.

**What arrives.** The counters: packets, shreds, FEC sets, entries,
transactions. This is block data reaching you while the block is still being
built.

**What it buys.** Two bots, same code, same rules, different pipe. The blotter
column that matters is `LOST`: how much of the gap had already gone by the time
the slower lane looked.

### What to say

**Say this:** with the feed you see the market earlier, and here is the number,
per event, live.

**Do not say the bot makes money.** It mostly does not. Over 3,881 triggers the
widest gap between two pools of one pair was 27 basis points, against fees from
6 to over 200, because other bots close the real gaps within a slot. That is why
the terminal shows rejections as first-class results.

**Do not claim to beat professional MEV.** They sit beside validators. A box in
Frankfurt loses most of those races, and saying otherwise is the one claim a
technical audience will catch instantly.

**Do not name the commercial RPC provider.** The binary will not print it, but
you might. The lanes are called "commercial" and "public" for this reason.

**Expect the slow lane to win sometimes.** It does, it is in the data, and
showing it is why anyone should believe the rest.

## When it breaks

| What you see | What it is |
|---|---|
| `no packets` from probe, but `status` looks fine | the feed is not provisioned on the access pass |
| a lane stuck at zero triggers | `429` on a shared key, or `413` from the public RPC; check the journal |
| blotter empty while counters move | only one lane is delivering, so nothing can be paired |
| the page shows old numbers | the binary was not rebuilt; `cargo build --release` |
| a `trycloudflare.com` link is dead | those change on every restart; use the tunnel hostname |
| a hostname fails to resolve for you but works from the box | your own resolver is holding a stale entry, often a negative one cached from before the record existed. `dig` bypasses it and will look fine while `curl` does not; flush the local cache before concluding anything is broken |
| the terminal died mid-stream | check `journalctl -u sol-race`; packets that panic the decoder are counted and dropped, so this should not happen |

## Where the code is

Public, one squashed commit: https://github.com/hypnogaba/solana-edge-terminal
Private, full history: https://github.com/hypnogaba/solana-edge-terminal
The Kalshi side: https://github.com/hypnogaba/kalshi-edge-lab

## Which feeds it reads, and where each is set

The terminal's SOURCES panel lists every feed and, beside it, the flag that
selects it. Nothing there is a URL: the RPC endpoints carry keys, so the panel
publishes the name of the variable holding the endpoint, which is also what an
operator needs in order to find it.

| Feed | Kind | Set by |
|---|---|---|
| `doublezero` | multicast shreds | `--iface doublezero1 --group 233.84.178.1 --port 7733` |
| any RPC lane | rpc websocket | `--lane <name>=env:VAR`, the URL in the unit's `Environment=` |
| pool prices | rpc websocket | `--pool-ws env:VAR --watch-pools` |

All of them live in `/etc/systemd/system/sol-race.service`. Adding a second
shred group once the feed is provisioned is a repeated `--group`. A lane given
as `name=wss://...` rather than `name=env:VAR` is reported on the page as a
problem, because a URL in argv is readable by every user on the box, in `ps`
and in `systemctl status`.

## Rehearsing before the feed exists

The live terminal cannot show a race until there is a second lane to race
against. `sol-rehearse.service` serves the same page on `127.0.0.1:8091`
driven entirely by invented data, so the stream can be rehearsed and recorded
beforehand. The page carries a banner saying so; nothing on it is a
measurement.

```
systemctl restart sol-rehearse          # a fresh session from the same seed
ssh -N -L 8098:127.0.0.1:8091 <host>    # then open http://127.0.0.1:8098
```

It is published at `rehearse.speedlab.pro`, through the same named tunnel as
the other two terminals: one ingress line plus a proxied CNAME. The page is
public, so the banner is the whole safeguard against a viewer taking an
invented head start for a measured one.

`--seed` replays the same session, so a take can be repeated.

## Connecting to it

Three terminals, all behind one named Cloudflare tunnel. Nothing binds a public
interface, so the tunnel is the only way in and the host's address is not in
DNS at all.

| Address | What it shows |
|---|---|
| `solana.speedlab.pro` | the live run |
| `rehearse.speedlab.pro` | the same screen on invented data, banner across the top |
| `kalshi.speedlab.pro` | the Kalshi race |

`/debug` on either DZ terminal carries the plumbing: shred pipeline counters,
per-lane latency, per-bot decisions, and every trigger.

To reach one that is not published, forward it rather than opening a port:

```
ssh -N -L 8098:127.0.0.1:8091 root@<host>     # then http://127.0.0.1:8098
```

## Connecting the shred feed

The feed is a multicast group on the DoubleZero tunnel interface. Request it on
the access pass, then point the run at it:

```
--iface doublezero1 --group 233.84.178.1 --port 7733
```

A second group is another `--group`. A plain UDP socket receives nothing on
this interface, which is why the capture path is AF_PACKET: the feed arrives
inside a GRE tunnel. `probe` counts packets on a group and is the fastest way
to tell whether the feed is actually being delivered:

```
solana-edge-lab probe --iface doublezero1 --group 233.84.178.1 --port 7733
```

Zero packets after ten seconds means the feed is not provisioned yet, not that
the decoder is broken.

## The wallet, and turning trading on

Trading is automatic already: the bot prices every trigger, decides, and hands
what clears to the executor without anyone touching it. What was missing was a
way to arm it, and a wallet.

**Arming.** `--mode dry-run` decides and records, sends nothing. `--mode
simulate` builds, signs and asks the cluster what would happen, still sending
nothing. `--mode live` sends. Live refuses to start while no broker is wired
in, rather than looking armed and doing nothing.

**Funding.** Two wallets, one per lane, so each bot spends its own money and
neither can explain away a loss with the other's balance. Keep each small:
this is a demonstration, and the guard rails are set for that.

- `--wallet <address>` is read only. It is how the screen knows the balance.
  Nothing in this process can sign, and the key never comes near it.
- The signing key belongs in the unit file's `Environment=`, never in argv: a
  URL or a key in argv is readable by every user on the box, in `ps` and in
  `systemctl status`.
- Fund it with a normal transfer to that address. Around 0.15 SOL per wallet is
  enough for the default 0.005 SOL attempt size plus fees.

**What stops it spending.** Three limits, checked before every attempt, in this
order:

| Guard | Flag | What it does |
|---|---|---|
| kill file | `--kill-file` | while the file exists nothing is sent, whatever else is set |
| session cap | `--session-cap-lamports` | most a lane may commit in one session |
| wallet ceiling | `--max-wallet-lamports` | refuses to trade a wallet holding more than this, so pointing the demo at a funded wallet by mistake stops rather than trades |

`touch data/KILL` stops trading immediately without restarting anything.
