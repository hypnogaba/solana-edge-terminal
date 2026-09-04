//! A page to watch the race on, and the JSON behind it.
//!
//! Deliberately tiny and dependency-free: a hand-rolled responder over a tokio
//! listener, two fixed routes, no file serving and no paths taken from the
//! request. A web framework here would add minutes to every build and buy
//! nothing, and this process is a measuring instrument, not a web server.
//!
//! The page is built to be read off a video capture: dark, few numbers, each
//! one large. It states what is not yet running rather than showing a plausible
//! blank, because a blank on a live stream reads as "it is broken".

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

/// The latest snapshot, as JSON. Shared with the race loop.
pub type Shared = Arc<RwLock<String>>;

/// How long a client gets to send its request line before being dropped.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One plain GET against a loopback address, so the terminal can show the
/// Kalshi lab's measurement beside its own without either process learning
/// about the other. A dependency on an HTTP client to read one local JSON
/// document would be a poor trade.
async fn fetch_local(upstream: SocketAddr, path: &str) -> Result<String> {
    let mut socket = tokio::time::timeout(
        REQUEST_TIMEOUT,
        tokio::net::TcpStream::connect(upstream),
    )
    .await??;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).await?;
    let mut body = Vec::new();
    tokio::time::timeout(REQUEST_TIMEOUT, socket.read_to_end(&mut body)).await??;
    let text = String::from_utf8_lossy(&body).into_owned();
    let start = text
        .find("\r\n\r\n")
        .map(|at| at + 4)
        .context("upstream sent no header break")?;
    Ok(text[start..].to_string())
}

pub async fn serve(addr: SocketAddr, state: Shared) -> Result<()> {
    serve_with(addr, state, None).await
}

/// `beside` is another terminal on this host whose state is offered at
/// `/api/beside`, so one screen can switch between two markets.
pub async fn serve_with(
    addr: SocketAddr,
    state: Shared,
    beside: Option<SocketAddr>,
) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "dashboard on http://{addr}/");
    loop {
        let (mut socket, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(%err, "accept failed");
                continue;
            }
        };
        let state = state.clone();
        let beside = beside;
        tokio::spawn(async move {
            // The dashboard is reachable through a tunnel during a stream, so a
            // client that connects and then says nothing must not be able to
            // hold a task open. One read, one write, one close.
            let mut buf = [0u8; 1024];
            let read = match tokio::time::timeout(REQUEST_TIMEOUT, socket.read(&mut buf)).await {
                Ok(Ok(read)) => read,
                _ => return,
            };
            let request = String::from_utf8_lossy(&buf[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");

            let response = match path {
                "/api/state" => {
                    let body = state.read().await.clone();
                    http("200 OK", "application/json", &body)
                }
                "/api/beside" => match beside {
                    // The page asks for it only when the switch is offered, and
                    // an upstream that is down must read as down rather than as
                    // a market with nothing happening in it.
                    Some(upstream) => match fetch_local(upstream, "/api/state").await {
                        Ok(body) => http("200 OK", "application/json", &body),
                        Err(err) => http(
                            "502 Bad Gateway",
                            "application/json",
                            &format!("{{\"error\":\"{}\"}}", err.to_string().replace('"', "'")),
                        ),
                    },
                    None => http("404 Not Found", "application/json", "{\"error\":\"not offered\"}"),
                },
                "/" | "/index.html" => http("200 OK", "text/html; charset=utf-8", &page(PAGE)),
                "/debug" => http("200 OK", "text/html; charset=utf-8", &page(DEBUG_PAGE)),
                _ => http("404 Not Found", "text/plain", "not here\n"),
            };
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
    }
}

fn http(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Shared head: fonts, tokens, and the type scale both pages use.
const HEAD: &str = r##"<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  /* Palette from finos/perspective's pro-dark theme, which is the grid FINOS
     ships for financial data. Sizing and the full cell grid from
     Gioni06/terminal.css. Both use the system monospace stack and no webfont,
     which also means the page cannot lose its type mid-stream because a font
     host is slow. */
  :root{
    color-scheme: dark;
    --ink:#242526; --panel:#2a2c2f; --sunk:#1e1f20;
    --line:#4c505b; --row:#3b3f46;
    --fg:#ffffff; --body:#c5c9d0; --soft:#c5c9d0; --faint:#61656e;
    --win:#7dc3f0; --warn:#cc7830; --lose:#ff9485; --active:#2770a9;
    --u:14px; --lh:1.4; --space:10px;
    --mono:"ui-monospace","SFMono-Regular","SF Mono","Menlo","Monaco",
           "Consolas","Liberation Mono","DejaVu Sans Mono",monospace;
  }
  *{box-sizing:border-box}
  @media (prefers-reduced-motion:reduce){*{transition:none!important}}
  html,body{height:100%;margin:0;background:var(--ink);color:var(--body)}
  body{font-family:var(--mono);font-size:var(--u);line-height:var(--lh);
       font-variant-numeric:tabular-nums;display:flex;flex-direction:column;overflow:hidden}
  a{color:var(--win)}
  .strip{flex:none;display:flex;align-items:center;flex-wrap:wrap;
         padding:calc(var(--space)/2) 0;background:var(--panel);
         border-bottom:1px solid var(--line)}
  .strip .c{padding:0 var(--space);white-space:nowrap;color:var(--soft)}
  .strip .brand{font-weight:700;letter-spacing:.08em;color:var(--fg)}
  .strip b{font-weight:700;color:var(--fg)}
  .strip .grow{flex:1}
  .on{color:var(--win)} .warn{color:var(--warn)} .bad{color:var(--lose)}
  .dim{color:var(--soft)} .faint{color:var(--faint)}
</style>"##;

const PAGE: &str = r##"<!doctype html>
<html lang="en" data-theme="dark">
<title>DZ Edge terminal</title>
{{HEAD}}
<style>
  /* Table density straight from terminal.css: 5px cells (its --global-space
     halved), a 1px line on every cell, collapsed borders. The full grid is
     what makes a wall of numbers scannable, and it is why a terminal looks
     like a terminal rather than like a list. */
  main{flex:1;min-height:0;display:flex;flex-direction:column;
       padding:0 var(--space) var(--space);gap:0}
  .stats{flex:none;display:flex;gap:calc(var(--space)*3);align-items:baseline;
         padding:var(--space) 0;border-bottom:1px solid var(--line)}
  .stat .n{font-size:1.5em;font-weight:700;color:var(--fg);line-height:1.1}
  .stat .n.off{color:var(--faint);font-weight:400}
  .stat .n.go{color:var(--win)}
  .stat .k{color:var(--faint);letter-spacing:.06em;text-transform:uppercase;font-size:.85em}
  h2{flex:none;font:inherit;font-weight:700;letter-spacing:.1em;text-transform:uppercase;
     color:var(--fg);margin:0;padding:var(--space) 0 calc(var(--space)/2)}
  .cols{flex:0 1 auto;min-height:0;display:grid;
        grid-template-columns:minmax(0,1.2fr) minmax(0,1fr);
        gap:0 calc(var(--space)*2);align-content:start}
  @media (max-width:1000px){ .cols{grid-template-columns:1fr} }
  .col{min-height:0;display:flex;flex-direction:column}
  .scroll{flex:0 1 auto;min-height:0;overflow:auto;max-height:58vh}
  table{width:100%;border-collapse:collapse;white-space:nowrap}
  th,td{border:1px solid var(--row);padding:calc(var(--space)/2);
        line-height:var(--lh);vertical-align:middle}
  th{position:sticky;top:0;z-index:1;background:var(--panel);color:var(--faint);
     text-align:left;font-weight:400;letter-spacing:.06em;border-color:var(--line)}
  tbody tr:hover td{background:var(--sunk)}
  .num{text-align:right}
  .w-pair{width:14ch} .w-n{width:8ch} .w-age{width:8ch}
  .chip{padding:0 calc(var(--space)/2);border:1px solid var(--line);color:var(--soft)}
  .chip.go{color:var(--ink);background:var(--win);border-color:var(--win);font-weight:700}
  .empty{color:var(--faint);padding:calc(var(--space)/2) 0}
  .events{flex:none;max-height:24vh;overflow:auto}
  footer{flex:none;display:flex;gap:calc(var(--space)*2);align-items:center;
         padding:calc(var(--space)/2) var(--space);color:var(--faint);
         background:var(--panel);border-top:1px solid var(--line)}
  footer a{color:var(--soft)}
  body[data-market="kalshi"]{--win:#4cbb8a}
  .switch button{font:inherit;color:var(--faint);background:none;cursor:pointer;
                 border:1px solid var(--line);padding:0 calc(var(--space)/2)}
  .switch button+button{border-left:none}
  .switch button.on{color:var(--ink);background:var(--win);border-color:var(--win);
                    font-weight:700}
  .banner{flex:none;padding:calc(var(--space)/2) var(--space);background:var(--warn);
          color:#111;font-weight:700;letter-spacing:.08em;text-transform:uppercase}
</style>

<div class="banner" id="banner" hidden></div>
<div class="strip">
  <span class="c brand">DZ EDGE</span>
  <span class="c switch" id="switch" hidden>
    <button type="button" data-market="solana" class="on">SOLANA</button><button
            type="button" data-market="kalshi">KALSHI</button>
  </span>
  <span class="c" id="feed"></span>
  <span class="c" id="slot"></span>
  <span class="c" id="mode"></span>
  <span class="c grow"></span>
  <span class="c" id="up"></span>
</div>

<main>
  <div class="stats">
    <div class="stat"><div class="n" id="heroN">—</div><div class="k" id="heroK">head start</div></div>
    <div class="stat"><div class="n" id="balN">—</div><div class="k" id="balK">balance</div></div>
    <div class="stat"><div class="n" id="pnlN">—</div><div class="k" id="pnlK">earned</div></div>
    <div class="stat"><div class="n" id="tradeN">—</div><div class="k" id="tradeK">trades</div></div>
    <div class="stat"><div class="n" id="wideN">—</div><div class="k" id="wideK">left after fees</div></div>
    <div class="stat"><div class="n" id="trigN">—</div><div class="k">triggers</div></div>
  </div>

  <div class="cols">
    <div class="col">
      <h2 id="mkH">markets</h2>
      <div class="scroll"><div id="mk"></div></div>
    </div>
    <div class="col">
      <h2 id="plH">pools</h2>
      <div class="scroll"><div id="pl"></div></div>
    </div>
  </div>

  <div class="cols">
    <div class="col">
      <h2 id="evH">trades</h2>
      <div class="events" id="ev"></div>
    </div>
    <div class="col">
      <h2 id="latH">latency</h2>
      <div id="lat"></div>
      <h2>sources</h2>
      <div class="events" id="src"></div>
    </div>
  </div>
</main>

<footer>
  <span id="wallet"></span>
  <span class="grow" style="flex:1"></span>
  <a href="/debug">every trigger →</a>
</footer>

<script>
const $ = id => document.getElementById(id);
const n = v => (v === null || v === undefined) ? "…" : Number(v).toLocaleString();
const ms = v => (v === null || v === undefined) ? "…" : Number(v).toFixed(0) + " ms";
const secs = v => (v === null || v === undefined) ? "" : n(Math.round(v/100)/10) + "s";
/* Pool prices run from 0.0000009 to 33 million on one screen. Fixed precision
   turns the large ones into exponents, which read as an error. */
const price = v => {
  const a = Math.abs(v);
  const dp = a >= 1000 ? 2 : a >= 1 ? 4 : a >= 0.001 ? 6 : 9;
  return Number(v).toLocaleString("en-US",
    {minimumFractionDigits: dp, maximumFractionDigits: dp});
};

const HISTORY = new Map();
const KEPT = 180;
function remember(rows){
  for(const r of rows){
    if(!HISTORY.has(r.pair)) HISTORY.set(r.pair, []);
    const seen = HISTORY.get(r.pair);
    seen.push(r.priced && !r.stale ? r.gap_bps : null);
    if(seen.length > KEPT) seen.shift();
  }
}

/* The gap over the last three minutes against the line it has to clear. A
   flat line under the threshold is the honest picture of a quiet market, and
   it is a picture rather than the same number printed again. */
function spark(pair, needs){
  const seen = HISTORY.get(pair) || [];
  const real = seen.filter(v => v !== null);
  if(real.length < 2) return '<span class="faint">…</span>';
  const top = Math.max(needs || 0, ...real) * 1.15 || 1;
  const w = 150, h = 15;
  const x = i => (i / Math.max(1, seen.length - 1) * w).toFixed(1);
  const y = v => (h - v / top * h).toFixed(1);
  let d = "", pen = false;
  seen.forEach((v, i) => {
    if(v === null){ pen = false; return; }
    d += (pen ? "L" : "M") + x(i) + " " + y(v);
    pen = true;
  });
  const line = needs ? `<line x1="0" y1="${y(needs)}" x2="${w}" y2="${y(needs)}"
    stroke="var(--faint)" stroke-width="1" stroke-dasharray="2 3"/>` : "";
  const over = real[real.length - 1] >= (needs || 0);
  return `<svg width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" aria-hidden="true">
    ${line}<path d="${d}" fill="none" stroke-width="1.3"
    stroke="${over ? "var(--win)" : "var(--warn)"}"/></svg>`;
}

/* One row per pair, updating in place. The useful fact about a market that is
   not tradeable is not "no", it is how far off it is: a gap four bps short of
   the fees is a different picture from one that is forty short. */
function markets(rows){
  if(!rows.length) return '<div class="empty">no pools priced yet</div>';
  const body = rows.map(r => {
    if(!r.priced) return `<tr><td class="w-pair">${r.pair}</td>
      <td class="num w-n faint">…</td><td class="num w-n faint">…</td>
      <td class="num w-n faint">…</td><td style="width:158px"></td><td class="faint">${
        r.pools_priced ? `${r.pools_priced} of ${r.pools_total} pools reporting · a gap needs two`
                       : `none of ${r.pools_total} pools reporting`}</td>
      <td class="num w-age faint">…</td></tr>`;
    if(r.stale) return `<tr><td class="w-pair">${r.pair}</td>
      <td class="num w-n faint">…</td><td class="num w-n faint">…</td>
      <td class="num w-n faint">…</td><td style="width:158px">${spark(r.pair, r.needs_bps)}</td>
      <td class="faint">${(r.route || "").replace("|", "↔")} · prices too old to compare</td>
      <td class="num w-age bad">${secs(r.age_ms)}</td></tr>`;
    const go = r.short_by_bps === 0;
    return `<tr>
      <td class="w-pair">${r.pair}</td>
      <td class="num w-n ${go ? "on" : ""}">${n(r.gap_bps)}</td>
      <td class="num w-n dim">${n(r.needs_bps)}</td>
      <td class="num w-n ${go ? "on" : "faint"}">${go ? "clears" : "-" + n(r.short_by_bps)}</td>
      <td style="width:158px">${spark(r.pair, r.needs_bps)}</td>
      <td class="dim">${(r.route || "").replace("|", "↔")}</td>
      <td class="num w-age ${r.age_ms > 2000 ? "warn" : "faint"}">${secs(r.age_ms)}</td>
    </tr>`;
  }).join("");
  return `<table><thead><tr>
    <th class="w-pair">PAIR</th><th class="num w-n">GAP bps</th>
    <th class="num w-n">FEES bps</th><th class="num w-n">SHORT BY</th>
    <th>LAST 3 MIN</th><th>BEST ROUTE</th><th class="num w-age">AGE</th>
  </tr></thead><tbody>${body}</tbody></table>`;
}

/* The readings the pair rows are computed from. A viewer who does not trust
   the gap asks where it came from, and this is the answer on the same screen. */
function poolTable(rows){
  if(!rows.length) return '<div class="empty">no pools configured</div>';
  const body = rows.map(r => `<tr>
    <td class="dim">${r.pair}</td>
    <td>${r.label}</td>
    <td class="num w-n ${r.price == null ? "faint" : ""}">${r.fee_bps ? r.fee_bps : "…"}</td>
    <td class="num" style="width:14ch">${r.price == null ? '<span class="faint">no price yet</span>'
      : price(r.price)}</td>
    <td class="num w-age ${r.age_ms > 5000 ? "warn" : "faint"}">${secs(r.age_ms)}</td>
  </tr>`).join("");
  return `<table><thead><tr>
    <th class="dim">PAIR</th><th>POOL</th><th class="num w-n">FEE bps</th>
    <th class="num">PRICE</th><th class="num w-age">AGE</th>
  </tr></thead><tbody>${body}</tbody></table>`;
}

/* The lead distribution as BlockRazor's shred-stats publishes it: the ladder
   from P1 to P99, and the lead ladder counted only over the races the feed
   won. One median is not a latency claim, because the tail is what decides
   whether a bot gets there first on the trade that pays. */
function ladder(lane){
  const all = lane && lane.quantiles || [];
  if(!all.length) return '<div class="empty">no shared event yet · '
    + 'the ladder needs both lanes to see the same transaction</div>';
  const led = lane.led_quantiles || [];
  const head = all.map(q => `<th class="num">${q.at}</th>`).join("");
  const row = qs => qs.length
    ? qs.map(q => `<td class="num">${Number(q.lead_ms).toFixed(0)}</td>`).join("")
    : all.map(() => '<td class="num faint">…</td>').join("");
  return `<table><thead><tr><th></th>${head}</tr></thead><tbody>
    <tr><td class="dim">all ms</td>${row(all)}</tr>
    <tr><td class="dim">when ahead</td>${row(led)}</tr>
  </tbody></table>`;
}

/* Where the data comes from and where each feed is configured. Without this
   a quiet screen and a feed that never connected look exactly alike, and the
   answer lived only in the unit file. */
function sourceTable(rows, shredsUp, triggers){
  if(!rows.length) return '<div class="empty">no sources declared</div>';
  const body = rows.map(r => {
    const live = r.kind === "multicast shreds" ? shredsUp : (triggers[r.name] || 0) > 0;
    return `<tr>
      <td><span class="chip ${live ? "go" : ""}">${live ? "up" : "down"}</span></td>
      <td>${r.name}</td>
      <td class="dim">${r.kind}</td>
      <td class="dim">${r.detail}</td>
      <td class="faint">${r.set_by}</td>
    </tr>`;
  }).join("");
  return `<table><thead><tr>
    <th></th><th>FEED</th><th>KIND</th><th>WHERE IT POINTS</th><th>SET BY</th>
  </tr></thead><tbody>${body}</tbody></table>`;
}

/* An arbitrage on a quarter of a SOL makes fractions of a thousandth of one.
   Four decimal places round every real profit to zero. */
const sol = v => (v === null || v === undefined) ? "…" : (Number(v)/1e9).toFixed(6);

/* Every attempt with what it was worth. A dry run books nothing, so the column
   is headed "would make" and the totals say so too: an expected number that
   reads as earnings is a lie with a decimal point. */
function tradeTable(rows, live){
  if(!rows.length) return '<div class="empty">no attempt yet · '
    + 'a row appears the moment a gap clears its fees</div>';
  const body = rows.slice(0, 80).map(t => {
    const real = t.booking === "realised";
    const gone = t.booking === "none";
    return `<tr>
      <td class="faint" style="width:8ch">${Number(t.at_s).toFixed(0)}s</td>
      <td class="w-pair">${t.pair}</td>
      <td class="dim">${t.route}</td>
      <td class="num w-n dim">${sol(t.size_lamports)}</td>
      <td class="num w-n warn">${n(t.gross_bps)}</td>
      <td class="num w-n ${gone ? "faint" : real ? "on" : ""}">${gone ? "—" : sol(t.net_lamports)}</td>
      <td class="${real ? "on" : gone ? "faint" : "dim"}">${t.outcome}</td>
    </tr>`;
  }).join("");
  return `<table><thead><tr>
    <th>AT</th><th class="w-pair">PAIR</th><th>ROUTE</th>
    <th class="num w-n">SIZE SOL</th><th class="num w-n">GAP bps</th>
    <th class="num w-n">${live ? "MADE SOL" : "WOULD MAKE"}</th><th>WHAT HAPPENED</th>
  </tr></thead><tbody>${body}</tbody></table>`;
}

/* The Kalshi lab measures the same claim on a market where the feed already
   exists, so the two sit behind one switch rather than on two screens. Its
   numbers are read through /api/beside and rendered here in the same language;
   nothing is recomputed, because a second opinion about someone else's
   measurement is how two screens start disagreeing. */
function kalshiMarkets(markets){
  const rows = Object.entries(markets || {})
    .sort((a, b) => (b[1].trades || 0) - (a[1].trades || 0));
  if(!rows.length) return '<div class="empty">no market on the feed</div>';
  const body = rows.map(([ticker, m]) => `<tr>
    <td class="w-pair">${ticker.replace(/^KX|PERP$/g, "")}</td>
    <td class="num">${price(m.bid)}</td>
    <td class="num">${price(m.ask)}</td>
    <td class="num dim">${price(m.last_price)}</td>
    <td class="${m.last_side === "buy" ? "on" : "bad"}">${m.last_side || ""}</td>
    <td class="num faint">${n(m.trades)}</td>
    <td class="num faint">${n(m.quotes)}</td>
  </tr>`).join("");
  return `<table><thead><tr>
    <th class="w-pair">MARKET</th><th class="num">BID</th><th class="num">ASK</th>
    <th class="num">LAST</th><th>SIDE</th><th class="num">TRADES</th><th class="num">QUOTES</th>
  </tr></thead><tbody>${body}</tbody></table>`;
}

function kalshiArms(arms){
  const channels = (arms || {}).channels || {};
  const body = Object.entries(channels).map(([id, c]) => `<tr>
    <td><span class="chip ${String(arms.selected) === id ? "go" : ""}">${
      String(arms.selected) === id ? "in use" : "spare"}</span></td>
    <td>channel ${id}</td>
    <td class="num">${n(c.frames)}</td>
    <td class="num ${c.wins ? "on" : "faint"}">${n(c.wins)}</td>
    <td class="num faint">${n(c.dropped_frames)}</td>
    <td class="num faint">${Number(c.last_seen_ms_ago || 0).toFixed(0)} ms ago</td>
  </tr>`).join("");
  return `<table><thead><tr>
    <th></th><th>PUBLISHER ARM</th><th class="num">FRAMES</th><th class="num">WINS</th>
    <th class="num">DROPPED</th><th class="num">LAST SEEN</th>
  </tr></thead><tbody>${body}</tbody></table>`;
}

function kalshiEvents(recent){
  if(!(recent || []).length) return '<div class="empty">no shared event yet</div>';
  const body = recent.slice(0, 60).map(e => {
    const dz = e.doublezero || {}, pub = e.public || {};
    return `<tr>
      <td class="w-pair">${e.market.replace(/^KX|PERP$/g, "")}</td>
      <td class="num">${price(e.trigger_price)}</td>
      <td class="num dim">${n(e.trigger_size)}</td>
      <td class="num on">${Number(e.lead_ms || 0).toFixed(1)} ms</td>
      <td class="${dz.filled ? "on" : "faint"}">dz ${dz.filled ? "filled" : (dz.reason || "no")}</td>
      <td class="${pub.filled ? "on" : "faint"}">public ${pub.filled ? "filled" : (pub.reason || "no")}</td>
    </tr>`;
  }).join("");
  return `<table><thead><tr>
    <th class="w-pair">MARKET</th><th class="num">PRICE</th><th class="num">SIZE</th>
    <th class="num">LEAD</th><th>DOUBLEZERO</th><th>PUBLIC</th>
  </tr></thead><tbody>${body}</tbody></table>`;
}

function kalshiLadder(lat){
  const steps = [["P10", lat.p10_ms], ["P50", lat.p50_ms], ["P90", lat.p90_ms],
                 ["P95", lat.p95_ms], ["P99", lat.p99_ms],
                 ["MIN", lat.min_ms], ["MAX", lat.max_ms]];
  const lag = (lat.arms || {}).loser_lag_ms || {};
  const second = [["P10", lag.p10], ["P50", lag.p50], ["P90", lag.p90],
                  ["P99", lag.p99], ["MAX", lag.max], ["", null], ["", null]];
  const cell = v => v === null || v === undefined
    ? '<td class="num faint">…</td>'
    : `<td class="num">${Number(v).toFixed(1)}</td>`;
  return `<table><thead><tr><th></th>${
    steps.map(([at]) => `<th class="num">${at}</th>`).join("")}</tr></thead><tbody>
    <tr><td class="dim">vs public ms</td>${steps.map(([, v]) => cell(v)).join("")}</tr>
    <tr><td class="dim">slow arm ms</td>${second.map(([, v]) => cell(v)).join("")}</tr>
  </tbody></table>`;
}

function renderKalshi(k){
  const dz = k.dz_live || {}, lat = k.latency || {}, duel = k.duel || {};
  const board = duel.scoreboard || {}, h2h = duel.head_to_head || {};
  const up = k.dz_feed === "live";
  $("banner").hidden = true;
  $("feed").innerHTML = up
    ? `<b class="on">FEED UP</b> <span class="dim">${dz.group_code || "doublezero"}</span>`
    : '<b class="bad">FEED DOWN</b>';
  $("slot").innerHTML = `<b>${Number(dz.rates ? dz.rates.msgs_per_s : 0).toFixed(0)}</b> msg/s`;
  $("mode").innerHTML = `<b class="warn">${(duel.mode || "paper").toUpperCase()}</b>`;
  $("up").textContent = "up " + Math.round(dz.uptime_s || 0) + "s";

  $("heroN").textContent = `${Number(lat.sooner_p50_ms || 0).toFixed(1)} ms`;
  $("heroN").className = "n go";
  $("heroK").textContent =
    `sooner · ${Number(lat.win_rate || 0).toFixed(1)}% first, of ${n(lat.n)} matched`;
  $("balN").textContent = `${Number(mine.fill_rate || 0).toFixed(1)}%`;
  $("balN").className = "n go";
  $("balK").textContent =
    `filled, against ${Number(theirs.fill_rate || 0).toFixed(1)}% on the public path`;
  const mine = board.doublezero || {}, theirs = board.public || {};
  $("balN").style.color = "";
  $("wideN").style.color = "";
  $("tradeN").textContent = n(mine.fills);
  $("tradeN").className = "n";
  $("tradeK").textContent = `fills of ${n(mine.intents)} intents · public ${n(theirs.fills)}`;
  // Both sides of this paper duel lose money, and the screen has to say so.
  // The feed's edge here is that it sees first and fills more, and that its
  // execution costs a little less per contract; it is not that the strategy
  // works. Showing a fill count without the markout beside it would let a
  // viewer read "more fills" as "more money".
  const mark = Number(mine.markout_per_contract || 0);
  const theirMark = Number(theirs.markout_per_contract || 0);
  $("pnlN").textContent = mark.toFixed(2);
  $("pnlN").className = "n" + (mark >= 0 ? " go" : "");
  $("pnlN").style.color = mark >= 0 ? "" : "var(--lose)";
  $("pnlK").textContent = `cents per contract, paper · public ${theirMark.toFixed(2)}`
    + ` · ${n(mine.contracts)} contracts`;
  $("wideN").textContent = n(h2h.dz_only_filled);
  $("wideN").className = "n go";
  $("wideK").textContent = `taken by the feed alone · public alone ${n(h2h.public_only_filled)} · both ${n(h2h.both_filled)}`;
  $("trigN").textContent = n((dz.totals || {}).trades);
  $("trigN").className = "n";

  $("mkH").innerHTML = `markets <span class="faint">· ${dz.market_count || 0} on the feed · `
    + `${n((dz.totals || {}).quotes)} quotes</span>`;
  $("mk").innerHTML = kalshiMarkets(dz.markets);
  $("plH").innerHTML = 'publisher arms <span class="faint">· the feed carries two, '
    + 'and the terminal follows whichever is ahead</span>';
  $("pl").innerHTML = kalshiArms(dz.arms);
  $("evH").innerHTML = `shared events <span class="faint">· ${n(h2h.n)} in the window · `
    + `both filled ${n(h2h.both_filled)} · neither ${n(h2h.neither_filled)}</span>`;
  $("ev").innerHTML = kalshiEvents(duel.recent);
  $("latH").innerHTML = 'latency <span class="faint">· negative is the feed arriving '
    + 'first, which is the whole claim</span>';
  $("lat").innerHTML = kalshiLadder(lat);
  $("src").innerHTML = sourceTable([
    {name: "doublezero", kind: "multicast", detail:
      `${lat.dz_group || ""}:${(dz.ports || {}).mktdata || ""} · ${dz.device || ""} · ${dz.metro || ""}`,
     set_by: "kalshi-edge-lab dz-feed.service"},
    {name: "public", kind: "websocket", detail: lat.public_ws || "",
     set_by: "kalshi-edge-lab dz-race.service"},
  ], up, {public: 1});
  $("wallet").innerHTML = `${lat.method || ""}`;
}

let MARKET = "solana";
async function tick(){
  if(MARKET === "kalshi"){
    try {
      const k = await (await fetch("/api/beside", {cache:"no-store"})).json();
      if(!k.error) renderKalshi(k);
    } catch(e) { /* the switch stays where it is; the last screen holds */ }
    return;
  }
  let s;
  try { s = await (await fetch("/api/state", {cache:"no-store"})).json(); }
  catch(e) { return; }
  if(!s || !s.lanes) return;

  const shreds = s.shreds || {};
  const live = shreds.packets > 0;
  // A screen of invented numbers that does not say so is worse than no screen.
  $("banner").hidden = !s.rehearsal;
  if(s.rehearsal) $("banner").textContent =
    "rehearsal · every number on this page is invented · nothing here is a measurement";
  $("feed").innerHTML = live
    ? '<b class="on">FEED UP</b> <span class="dim">doublezero · shreds</span>'
    : '<b class="bad">FEED DOWN</b> <span class="dim">rpc only</span>';
  $("up").textContent = "up " + Math.round(s.uptime_s) + "s";

  const duels = s.duels || [];
  $("slot").innerHTML = duels.length ? "slot <b>" + n(duels[0].slot) + "</b>" : "";

  const bots = s.bots || {};
  const fast = s.reference_lane || "";
  const paired = duels.filter(d => (d.views || []).length >= 2);
  const lane = (s.lanes || [])[0] || {};

  if(paired.length){
    const leads = paired.map(d => d.head_start_ms).filter(v => v != null).sort((x,y)=>x-y);
    $("heroN").textContent = ms(leads[Math.floor(leads.length/2)]);
    $("heroN").className = "n go";
    $("heroK").textContent = `head start · ${paired.length} shared`;
  } else if(lane.median_lead_ms != null){
    $("heroN").textContent = ms(lane.median_lead_ms);
    $("heroN").className = "n go";
    $("heroK").textContent = "head start";
  } else {
    $("heroN").textContent = "—";
    $("heroN").className = "n off";
    $("heroK").textContent = "head start · needs a second lane";
  }

  // Money. Nothing here may present an expectation as a receipt.
  const money = s.money || {};
  const armed = s.mode === "live";
  if(s.wallet === null || s.wallet === undefined){
    $("balN").textContent = "—";
    $("balN").className = "n off";
    $("balK").textContent = "balance · no wallet attached";
  } else {
    $("balN").textContent = sol(s.wallet.lamports);
    $("balN").className = "n";
    const change = (s.wallet.lamports || 0) - (s.wallet.started_lamports || 0);
    $("balK").innerHTML = "balance SOL · " + (change === 0 ? "unchanged"
      : `<span class="${change > 0 ? "on" : "bad"}">${change > 0 ? "+" : ""}${sol(change)}</span>`);
  }
  const banked = (money.sent || 0) > 0;
  $("pnlN").style.color = "";
  const shown = banked ? money.realised_lamports : money.expected_lamports;
  $("pnlN").textContent = sol(shown);
  $("pnlN").className = "n" + (shown ? (banked ? " go" : "") : " off");
  $("pnlK").textContent = banked ? "earned SOL"
                                 : `would have made SOL · ${s.mode || "dry run"}`;
  $("tradeN").textContent = n(money.attempts);
  $("tradeN").className = "n" + (money.attempts ? "" : " off");
  $("tradeK").textContent = armed ? `sent ${n(money.sent)} · refused ${n(money.refused)}`
                                 : `found · sent 0 · refused ${n(money.refused)}`;

  // The raw widest gap promised an opportunity that could not exist: the
  // session's widest was 197 bps between two pools charging 216 between them.
  // What is left after the fees is the number that means something.
  const widest = Math.max(...Object.values(bots).map(b => b.best_gap_bps || 0), 0);
  const net = Math.max(...Object.values(bots).map(b => b.best_net_bps || 0), 0);
  $("wideN").textContent = net ? n(net) : "0";
  $("wideN").className = "n" + (net ? "" : " off");
  $("wideK").textContent = widest
    ? `bps left after fees · widest raw gap ${n(widest)}`
    : "bps left after fees";
  const allTriggers = Object.values(s.triggers || {}).reduce((a,b)=>a+b,0);
  $("trigN").textContent = n(allTriggers);
  $("trigN").className = "n" + (allTriggers ? "" : " off");

  const mk = s.markets || [];
  remember(mk);
  $("mk").innerHTML = markets(mk);
  const clearing = mk.filter(r => r.priced && r.short_by_bps === 0).length;
  $("mkH").innerHTML = `markets <span class="faint">· ${mk.filter(r=>r.priced).length} priced`
    + (clearing ? ` · ${clearing} over the fees` : "") + `</span>`;

  const pl = s.pools || [];
  $("pl").innerHTML = poolTable(pl);
  const reporting = pl.filter(r => r.price != null).length;
  $("plH").innerHTML = `pools <span class="faint">· ${reporting} of ${pl.length} reporting</span>`;

  $("lat").innerHTML = ladder(lane);
  $("latH").innerHTML = "latency <span class=\"faint\">· "
    + (lane.reference_first_pct === null || lane.reference_first_pct === undefined
        ? "no first-arrival share yet"
        : `${lane.reference_first_pct}% first arrivals · ${n(lane.n_window)} shared`)
    + "</span>";

  $("src").innerHTML = sourceTable(s.sources || [], live, s.triggers || {});

  $("ev").innerHTML = tradeTable(s.trades || [], banked);
  // "It did not trade" is not an answer. This is the answer.
  const why = Object.entries((bots[fast] || bots[Object.keys(bots)[0]] || {}).declined || {})
    .sort((a, b) => b[1] - a[1])
    .map(([reason, count]) => `${reason.replace(/_/g, " ")} ${n(count)}`)
    .join(" · ");
  $("evH").innerHTML = `trades <span class="faint">· ${n(money.attempts)} attempts`
    + (money.attempts ? ` · best find ${sol(money.best_lamports)} SOL` : "")
    + (why ? ` · declined: ${why}` : "") + `</span>`;

  const exec = (s.execution || {})[fast] || {};
  // The word LIVE is evidence, not configuration. A screen reading LIVE while
  // nothing has been sent is the worst label available, so a live run that has
  // sent nothing yet reads ARMED and says so.
  const trading = exec.sent > 0;
  $("mode").innerHTML = trading ? '<b class="on">LIVE</b>'
    : armed ? '<b class="warn">ARMED</b> <span class="dim">live, nothing sent yet</span>'
    : `<b class="warn">${(s.mode || "dry run").toUpperCase()}</b>`;
  $("wallet").innerHTML = trading
    ? `sent <b>${n(exec.sent)}</b> · spent <b>${sol(exec.spent)} SOL</b>`
    : "nothing has been sent";
}
// The switch appears only when a second market is actually offered.
fetch("/api/beside", {cache:"no-store"}).then(r => {
  if(r.ok) $("switch").hidden = false;
}).catch(() => {});
$("switch").addEventListener("click", event => {
  const button = event.target.closest("button");
  if(!button) return;
  MARKET = button.dataset.market;
  document.body.dataset.market = MARKET;
  for(const one of $("switch").querySelectorAll("button")){
    one.className = one.dataset.market === MARKET ? "on" : "";
  }
  tick();
});
tick(); setInterval(tick, 1000);
</script>
"##;

/// Everything an operator needs and a viewer does not.
const DEBUG_PAGE: &str = r##"<!doctype html>
<html lang="en" data-theme="dark">
<title>DZ Edge diagnostics</title>
{{HEAD}}
<style>
  main{flex:1;min-height:0;overflow-y:auto;padding:1.4em}
  .grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(270px,1fr));gap:1.2em;
        max-width:1400px}
  section{border:1px solid var(--line);border-radius:8px;overflow:hidden}
  h2{margin:0;padding:.5em 1em;background:var(--panel);border-bottom:1px solid var(--line);
     font-family:var(--mono);font-weight:700;
     letter-spacing:.14em;color:var(--soft)}
  .rows{padding:.8em 1em;display:grid;gap:.45em}
  .kv{display:flex;justify-content:space-between;gap:1em}
  .kv span{color:var(--soft)} .kv i{font-style:normal;color:var(--fg)}
  h3{font:inherit;font-weight:700;letter-spacing:.1em;text-transform:uppercase;
     color:var(--soft);margin:1.6em 0 .5em}
  table{width:100%;border-collapse:collapse;white-space:nowrap}
  th,td{border:1px solid var(--row);padding:calc(var(--space)/2);line-height:var(--lh)}
  th{text-align:left;font-weight:400;color:var(--faint);border-color:var(--line)}
  .num{text-align:right}
</style>
<div class="strip">
  <div class="c brand">DZ-EDGE</div>
  <div class="c">diagnostics</div>
  <div class="grow"></div>
  <div class="c"><a href="/">back to the terminal</a></div>
</div>
<main><div class="grid" id="grid"></div>
<h3 id="tapeH">every trigger</h3><div id="tape"></div></main>
<script>
const n = v => (v === null || v === undefined) ? "…" : Number(v).toLocaleString();
const ms = v => (v === null || v === undefined) ? "…" : Number(v).toFixed(0) + " ms";

/* Collapse readings that say the same thing. Forty triggers can hit one pool
   inside a single slot and read identically; printing each one fills the page
   with a single fact repeated, which looks broken rather than busy. */
function group(rows, keyOf){
  const out = [];
  for(const r of rows){
    const key = keyOf(r);
    const last = out[out.length - 1];
    if(last && last.key === key){ last.count += 1; continue; }
    out.push({ key, count: 1, d: r });
  }
  return out;
}

function tape(duels, fast){
  const groups = group(duels, d => {
    const v = d.views[0] || {};
    return [d.slot, d.pair, v.route, v.gap_bps, v.tradeable, d.views.length].join("|");
  });
  const body = groups.slice(0, 400).map(g => {
    const d = g.d;
    const a = d.views.find(v => v.lane === fast) || d.views[0] || {};
    const b = d.views.find(v => v.lane !== a.lane);
    return `<tr>
      <td class="faint">${n(d.slot)}</td>
      <td>${d.pair}</td>
      <td class="dim">${(a.route || "").replace("|", "↔")}</td>
      <td class="num ${a.gap_bps ? "warn" : "faint"}">${n(a.gap_bps)}</td>
      <td class="num faint">${b ? n(b.gap_bps) : ""}</td>
      <td class="num ${d.gap_lost_bps > 0 ? "on" : "faint"}">${d.gap_lost_bps > 0 ? "-" + n(d.gap_lost_bps) : ""}</td>
      <td class="num">${b ? ms(d.head_start_ms) : ""}</td>
      <td class="num faint">${g.count > 1 ? "×" + g.count : ""}</td>
      <td class="${a.tradeable ? "on" : "faint"}">${a.tradeable ? "worth taking" : "under the fees"}</td>
    </tr>`;
  }).join("");
  return `<table><thead><tr>
    <th>SLOT</th><th>PAIR</th><th>POOLS</th><th class="num">GAP bps</th>
    <th class="num">OTHER LANE</th><th class="num">LOST</th><th class="num">HEAD START</th>
    <th class="num">SEEN</th><th>VERDICT</th>
  </tr></thead><tbody>${body}</tbody></table>`;
}
function block(title, rows){
  return `<section><h2>${title}</h2><div class="rows">` + rows.map(([k,v]) =>
    `<div class="kv"><span>${k}</span><i>${v}</i></div>`).join("") + `</div></section>`;
}
async function tick(){
  let s;
  try { s = await (await fetch("/api/state", {cache:"no-store"})).json(); }
  catch(e) { return; }
  const sh = s.shreds || {};
  const out = [block("SHRED PIPELINE", [
    ["packets", n(sh.packets)], ["shreds", n(sh.shreds)],
    ["fec sets rebuilt", n(sh.batches_ready)], ["entries", n(sh.entries)],
    ["transactions", n(sh.transactions)], ["deshred errors", n(sh.deshred_errors)],
    ["decoder panics", n(sh.panics)],
  ])];
  for(const [k, v] of Object.entries(s.lanes ? {} : {})) {}
  for(const l of (s.lanes || [])){
    out.push(block("LATENCY vs " + l.lane.toUpperCase(), [
      ["median", n(l.median_lead_ms)], ["p10 / p90", n(l.p10_lead_ms) + " / " + n(l.p90_lead_ms)],
      ["reference first", (l.reference_first_pct ?? "…") + "%"],
      ["matched in window", n(l.n_window)], ["seen on that lane", n(l.seen)],
    ]));
  }
  for(const [name, b] of Object.entries(s.bots || {})){
    out.push(block("BOT " + name.toUpperCase(), [
      ["triggers priced", n(b.seen)], ["tradeable", n(b.actionable)],
      ["gap under cost", n(b.below_cost)], ["widest gap seen", n(b.best_gap_bps) + " bps"],
      ["nothing to compare", n(b.no_price)], ["bad reference data", n(b.implausible)],
      ["repeats dropped", n(b.duplicates)],
    ]));
  }
  for(const [name, e] of Object.entries(s.execution || {})){
    out.push(block("EXECUTION " + name.toUpperCase(), [
      ["offered", n(e.offered)], ["refused by limits", n(e.refused)],
      ["stopped by kill file", n(e.refused_killed)], ["over session cap", n(e.refused_cap)],
      ["wallet too large", n(e.refused_wallet)], ["build failed", n(e.build_failed)],
      ["sent", n(e.sent)], ["spent", ((e.spent||0)/1e9).toFixed(6) + " SOL"],
    ]));
  }
  out.push(block("TRIGGERS", Object.entries(s.triggers || {}).map(([k,v]) => [k, n(v)])));
  out.push(block("SESSION", [
    ["reference lane", s.reference_lane || "…"], ["uptime", Math.round(s.uptime_s) + "s"],
    ["duels paired", n(s.duel_paired)], ["awaiting a pair", n(s.awaiting_pair)],
  ]));
  document.getElementById("grid").innerHTML = out.join("");

  const duels = s.duels || [];
  if(duels.length){
    const shown = group(duels, d => {
      const v = d.views[0] || {};
      return [d.slot, d.pair, v.route, v.gap_bps, v.tradeable, d.views.length].join("|");
    }).length;
    document.getElementById("tapeH").innerHTML =
      `every trigger <span class="faint">· ${shown} distinct readings from ${duels.length}</span>`;
    document.getElementById("tape").innerHTML = tape(duels, s.reference_lane || "");
  } else {
    document.getElementById("tape").innerHTML =
      '<div style="color:var(--faint)">nothing has touched a watched pool yet</div>';
  }
}
tick(); setInterval(tick, 1000);
</script>
"##;

fn page(body: &str) -> String {
    body.replace("{{HEAD}}", HEAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_reads_only_fields_the_snapshot_carries() {
        // The page and the snapshot drift apart silently: a renamed field shows
        // as a blank panel, never as an error.
        // The stream page carries the claim; diagnostics carry the plumbing.
        // Between them every field the binary writes has a reader.
        let both = format!("{PAGE}{DEBUG_PAGE}");
        for field in [
            "shreds", "lanes", "duels", "bots", "triggers", "reference_lane",
            "duel_paired", "gap_lost_bps", "views", "markets", "short_by_bps", "fee_bps", "sources", "set_by", "rehearsal", "quantiles", "led_quantiles", "money", "trades", "declined", "best_net_bps", "tradeable", "best_gap_bps",
            "implausible", "no_price", "execution", "refused_killed",
        ] {
            assert!(both.contains(field), "no page reads {field}");
        }
    }

    /// Every bare `name(` call in a page's script, ignoring method calls,
    /// keywords and the handful of browser globals the pages use.
    fn bare_calls(script: &str) -> std::collections::BTreeSet<String> {
        const BUILTIN: &[&str] = &[
            "if", "for", "while", "switch", "catch", "return", "typeof", "function",
            "fetch", "setInterval", "Number", "String", "Object", "Array", "Math",
            "JSON", "Boolean", "Date", "Map", "Set",
            // Scripts build inline styles, so CSS functions appear in them too.
            "var", "url", "calc", "clamp", "minmax", "rgb", "rgba",
        ];
        let bytes: Vec<char> = script.chars().collect();
        let mut found = std::collections::BTreeSet::new();
        for (i, c) in bytes.iter().enumerate() {
            if *c != '(' {
                continue;
            }
            let mut start = i;
            while start > 0
                && (bytes[start - 1].is_alphanumeric()
                    || bytes[start - 1] == '_'
                    || bytes[start - 1] == '$')
            {
                start -= 1;
            }
            if start == i || bytes[start].is_ascii_digit() {
                continue;
            }
            // A method call carries its receiver, which the page does not define.
            if start > 0 && bytes[start - 1] == '.' {
                continue;
            }
            let name: String = bytes[start..i].iter().collect();
            if !BUILTIN.contains(&name.as_str()) {
                found.insert(name);
            }
        }
        found
    }

    /// The names bound by every `function name(a, b)` signature in a script.
    fn parameters(script: &str) -> std::collections::BTreeSet<String> {
        let mut names = std::collections::BTreeSet::new();
        for rest in script.split("function ").skip(1) {
            let Some(open) = rest.find('(') else { continue };
            let Some(close) = rest[open..].find(')') else { continue };
            for part in rest[open + 1..open + close].split(',') {
                let name = part.trim();
                if !name.is_empty() {
                    names.insert(name.to_string());
                }
            }
        }
        names
    }

    /// Every `const`/`let` name declared inside one function body.
    fn declarations(body: &str) -> Vec<String> {
        let mut names = Vec::new();
        for keyword in ["const ", "let "] {
            for rest in body.split(keyword).skip(1) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                    .collect();
                // `const {a, b} =` and `const [x] =` destructure; skip them.
                if !name.is_empty() && rest[name.len()..].trim_start().starts_with('=') {
                    names.push(name);
                }
            }
        }
        names
    }

    /// The body of each `function name(...) { ... }`, by brace depth.
    ///
    /// Byte indices throughout: the pages carry non-ASCII text, so mixing a
    /// byte offset from `find` into a `Vec<char>` reads the wrong region and
    /// silently returns a body that is not the function's. That is how the
    /// first version of this test passed on the very bug it was written for.
    fn function_bodies(script: &str) -> Vec<(String, String)> {
        let bytes = script.as_bytes();
        let mut out = Vec::new();
        for (start, _) in script.match_indices("function ") {
            let head = start + "function ".len();
            let Some(paren) = script[head..].find('(') else { continue };
            let name = script[head..head + paren].trim().to_string();
            let Some(open) = script[head..].find('{') else { continue };
            let open = head + open;
            let mut depth = 0i32;
            let mut end = None;
            for index in open..bytes.len() {
                match bytes[index] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(index);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(end) = end {
                out.push((name, script[open + 1..end].to_string()));
            }
        }
        out
    }

    #[test]
    fn no_function_on_a_page_declares_the_same_name_twice() {
        // Two `const live` in one function is a SyntaxError, which means the
        // whole script never runs and every panel on the page stays empty. It
        // is not caught by the undefined-helper test, because nothing is
        // undefined: the file simply never parses. It shipped once.
        for (label, page) in [("stream", PAGE), ("diagnostics", DEBUG_PAGE)] {
            let open = page.find("<script>").expect("a page has a script");
            for (name, body) in function_bodies(&page[open..]) {
                let mut seen = std::collections::BTreeSet::new();
                for declared in declarations(&body) {
                    assert!(
                        seen.insert(declared.clone()),
                        "{label} page declares {declared} twice inside {name}()"
                    );
                }
            }
        }
    }

    #[test]
    fn a_losing_paper_duel_is_not_shown_as_a_win() {
        // Both sides of the Kalshi duel lose money: the feed is at -1.11 per
        // contract against the public path's -1.20. Its edge is that it sees
        // first, fills more, and costs a little less to execute. A screen that
        // showed the fill count without the markout beside it would let "more
        // fills" read as "more money", which is the opposite of the truth.
        assert!(PAGE.contains("markout_per_contract"));
        assert!(PAGE.contains("cents per contract, paper"));
        // Green is reserved for a number that is actually positive.
        assert!(PAGE.contains("mark >= 0 ? \" go\" : \"\""));
        assert!(PAGE.contains("var(--lose)"));
    }

    #[test]
    fn each_market_carries_its_own_accent() {
        // Two markets on one screen, and a glance has to say which is on it.
        assert!(PAGE.contains("body[data-market=\"kalshi\"]{--win:"));
        assert!(PAGE.contains("document.body.dataset.market = MARKET"));
    }

    #[test]
    fn the_diagnostics_page_carries_none_of_the_stream_page_s_script() {
        // The two pages share several anchors, `async function tick(){` and
        // `tick(); setInterval` among them, so an edit aimed at one lands in
        // both. That put the Kalshi renderer and the market switch into the
        // diagnostics page, where the elements they address do not exist,
        // which throws on the first tick and leaves the page blank.
        for name in ["renderKalshi", "kalshiMarkets", "MARKET", "api/beside"] {
            assert!(
                !DEBUG_PAGE.contains(name),
                "the diagnostics page carries {name}, which belongs to the stream page"
            );
        }
    }

    #[test]
    fn every_helper_a_page_calls_is_defined_on_that_page() {
        // A call to a helper that was never defined throws on the first tick
        // and stops the rest of it, so the page keeps rendering whatever it had
        // drawn before the throw. It looks like a half-empty screen, never like
        // an error, and it shipped exactly once for that reason.
        for (label, page) in [("stream", PAGE), ("diagnostics", DEBUG_PAGE)] {
            // CSS has calls of its own (minmax, clamp) that no script defines.
            let open = page.find("<script>").expect("a page has a script");
            let script = &page[open..];
            for name in bare_calls(script) {
                let defined = script.contains(&format!("function {name}("))
                    || script.contains(&format!("const {name} ="))
                    || script.contains(&format!("let {name} ="))
                    // A callback arrives as a parameter, so its name is bound
                    // by the signature rather than by a declaration.
                    || parameters(script).contains(&name);
                assert!(defined, "{label} page calls {name}() but never defines it");
            }
        }
    }

    #[test]
    fn the_page_never_carries_an_endpoint_or_a_key() {
        // Stronger than blocking one provider's name, and it names nobody:
        // the page has no business holding any URL at all. It fetches one
        // relative path and renders what the binary hands it.
        assert!(!PAGE.contains("api-key"));
        // The only URLs allowed are the font host. Everything else the page
        // needs comes from one relative path on this binary, so any other
        // host appearing here would be an endpoint leaking onto a screen.
        let both = format!("{PAGE}{HEAD}{DEBUG_PAGE}");
        for (index, _) in both.match_indices("://") {
            let tail = &both[index + 3..];
            let host: String = tail.chars().take_while(|c| *c != '/' && *c != '"').collect();
            // There is no longer any exception, not even a font host: the
            // pages use the system monospace stack, so they load nothing from
            // anywhere and cannot lose their type mid-stream.
            panic!("page carries a URL to {host}");
        }
    }

    #[test]
    fn the_page_says_when_the_shred_feed_is_absent() {
        // Showing an RPC-only comparison while the viewer believes they are
        // watching the shred feed is worse than showing nothing.
        assert!(PAGE.contains("FEED DOWN"));
        assert!(PAGE.contains("rpc only"));
    }

    #[test]
    fn the_reference_lane_keeps_the_fast_slot_even_when_it_is_silent() {
        // Promoting a working lane into the A slot would label the slow side as
        // the fast one, which is exactly the confusion this page exists to
        // prevent.
        assert!(PAGE.contains("const fast = s.reference_lane"));
        // And with no second lane the head start says what is missing, rather
        // than printing a zero that reads as "no advantage".
        assert!(PAGE.contains("needs a second lane"));
        assert!(!PAGE.contains("$(\"heroN\").textContent = \"0\""));
    }

    #[test]
    fn the_page_says_it_is_not_trading() {
        // The mode name is whatever the run was started with, uppercased, so
        // the badge cannot claim a mode the binary is not in.
        assert!(PAGE.contains("(s.mode || \"dry run\").toUpperCase()"));
        assert!(PAGE.contains("nothing has been sent"));
    }

    #[test]
    fn the_stream_page_carries_no_engineering_counters() {
        // A viewer has no use for fec sets or dropped repeats, and every extra
        // row on the screen costs attention that belongs on the claim.
        for noise in ["fec sets", "repeats dropped", "decoder panics", "bad reference data"] {
            assert!(!PAGE.contains(noise), "{noise} belongs on the diagnostics page");
            assert!(DEBUG_PAGE.contains(noise), "{noise} went missing entirely");
        }
    }

    #[test]
    fn identical_readings_are_collapsed_rather_than_repeated() {
        // Forty triggers can hit one pool inside a single slot and read the
        // same. Printing each one filled the screen with one fact repeated,
        // which looks like a broken table rather than a busy market.
        assert!(DEBUG_PAGE.contains("function group("));
        assert!(DEBUG_PAGE.contains("distinct readings from"));
        // And the stream page does not carry a per-trigger tape at all: one row
        // per market that updates in place, the log behind a link.
        assert!(PAGE.contains("every trigger →"));
    }

    #[test]
    fn the_page_says_dry_run_until_something_is_actually_sent() {
        // "live" on a screen while nothing has been sent is the worst possible
        // label, so the mode is derived from the send count rather than from
        // configuration, which can say live while the wallet is empty.
        assert!(PAGE.contains("exec.sent > 0"), "mode must come from the send count");
        assert!(PAGE.contains("LIVE"));
        // Configured live but nothing sent is its own state, and it is not
        // called LIVE. The earnings figure follows the same rule.
        assert!(PAGE.contains("ARMED"));
        assert!(PAGE.contains("const banked = (money.sent || 0) > 0"));
        // The badge prints the mode the run was started in rather than a fixed
        // string, so there is no "DRY RUN" literal left to check.
        assert!(PAGE.contains("(s.mode || \"dry run\").toUpperCase()"));
        // And a missing wallet is named, never shown as a balance of zero.
        assert!(PAGE.contains("no wallet attached"));
        assert!(PAGE.contains("s.wallet === null"));
    }
}
