//! What a live run does before it looks at a single new signal.
//!
//! [`run`] takes the journal, the ladder state, the stale-order flags, the P&L ledger and
//! the venue balances, and does, in this order:
//!
//! 1. **Reconcile** ([`crate::reconcile`](mod@crate::reconcile)), then book every new fill:
//!    * a filled market buy sets the fill-based cost basis and pairs a resting limit sell
//!      at fill +target net of the sell fee (no pair while the bull sell policy governs
//!      the coin: the buy is a core add);
//!    * a filled deploy buy is a core add: cost basis, no pair, no trade window. A fill on
//!      Revolut X for a coin routed elsewhere stays out of the ladder state;
//!    * a filled sell is logged to the P&L ledger and opens a sell window.
//! 2. **Retry failed limit sells**: adopt the order when it rests at the venue after all
//!    (same id, or a price within 0.3%), else place it again, five tries at most.
//! 3. **Manage resting limit sells**: move a sell whose price fell below the current
//!    cost-basis target up to it (a trailing coin's target rides the venue price instead),
//!    restoring the old cover at once if the venue refuses the new price; flag orders
//!    older than the regime's TTL, once per re-nag window.
//! 4. **Holding drift**: compare each coin's venue holding with the last snapshot plus
//!    what the journal booked since, per order. Venues whose balance read failed are
//!    skipped and carry their snapshot forward. The first run with no per-order snapshot
//!    only writes one.
//!
//! Steps 2 and 3 place orders and are skipped while trading is blocked; drift always runs.
//! Every step reports [`Outcome`]s whose texts are the mail and decision-log lines.
//!
//! A step that fails outright (an unknown venue, an order that cannot be printed) ends
//! that step with one `reconcile: …` or `housekeeping: …` error, and what it changed
//! before stays changed. The callers own every file: nothing here reads or writes one.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rungbot_core::housekeeping::{self as hk, Fees};
use serde_json::{json, Map, Value};

use crate::ids;
use crate::journal::{client_id, suffixed, Journal, Order, Side};
use crate::pyfmt::{self, g};
use crate::reconcile::{reconcile, settle_cancel, Settled, VenueSource};
use crate::venue::{venue_id_matches, Balance, ParsedOrder, Venue};

/// Venues balances are read from, in this order.
pub const BALANCE_VENUES: [&str; 3] = ["binance", "gate", "revx"];

/// A limit sell whose placement failed is retried this many times, then handed over.
pub const MAX_SELL_RETRIES: i64 = 5;

/// A resting order this close to a failed sell's price is that sell (0.3%).
pub const ADOPT_PRICE_TOLERANCE: f64 = 0.003;

/// Reprice when the order sits below this share of its target; a trailing coin uses
/// the wider band so a climbing price does not churn a cancel and re-place every run.
pub const REPRICE_BAND: f64 = 0.999;
pub const REPRICE_BAND_TRAILING: f64 = 0.98;

/// Text for a value that is missing where a line has to print it.
const NONE_FORMAT: &str = "unsupported format string passed to NoneType.__format__";

/// Where each coin trades.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub sym: String,
    pub exch: String,
    pub pair: String,
    pub quote: String,
}

/// The knobs housekeeping reads. Defaults are the documented defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Take-profit over the fill, in percent, net of the sell fee.
    pub target_pct: f64,
    pub fees: Fees,
    /// How long a filled sell blocks the buy side, in hours.
    pub window_hours: f64,
    /// Stale-order warning age in a bull market, days. 0 turns the warning off.
    pub limit_ttl_days: f64,
    pub limit_ttl_days_bear: f64,
    pub limit_ttl_days_chop: f64,
    /// Re-flag a still-stale order after this many days. 0 = flag once, ever.
    pub ttl_renag_days: f64,
    /// Coin -> venue and pair, in the order drift reports coins.
    pub routing: Vec<Route>,
    /// Seed cost basis per coin, used until the state records one.
    pub entries: BTreeMap<String, f64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            target_pct: 10.0,
            fees: Fees::default(),
            window_hours: 24.0,
            limit_ttl_days: 14.0,
            limit_ttl_days_bear: 45.0,
            limit_ttl_days_chop: 30.0,
            ttl_renag_days: 30.0,
            routing: Vec::new(),
            entries: BTreeMap::new(),
        }
    }
}

impl Settings {
    fn route(&self, sym: &str) -> Option<&Route> {
        self.routing.iter().find(|r| r.sym == sym)
    }

    pub fn limit_target(&self, fill_price: f64, exch: &str) -> f64 {
        hk::limit_target(fill_price, self.target_pct, self.fees.pct(exch))
    }
}

/// How a result reads: done, warn or err.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Done,
    Warn,
    Err,
}

impl Level {
    pub fn as_str(&self) -> &'static str {
        match self {
            Level::Done => "done",
            Level::Warn => "warn",
            Level::Err => "err",
        }
    }
}

/// One result line. `hk` marks housekeeping (never rolled back with a signal);
/// `committed` means an order went to a venue.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub sym: Option<String>,
    pub side: Option<String>,
    pub mode: String,
    pub hk: bool,
    pub committed: bool,
    pub level: Level,
    pub text: String,
}

impl Outcome {
    fn new(sym: Option<&str>, level: Level, text: String) -> Outcome {
        Outcome {
            sym: sym.map(str::to_string),
            side: None,
            mode: "live".into(),
            hk: true,
            committed: false,
            level,
            text,
        }
    }

    fn side(mut self, side: &str) -> Outcome {
        self.side = Some(side.into());
        self
    }

    fn committed(mut self) -> Outcome {
        self.committed = true;
        self
    }

    /// The result as a JSON object: `{sym, side, mode, hk, committed, done|warn|err}`,
    /// absent keys left out.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(s) = &self.sym {
            m.insert("sym".into(), json!(s));
        }
        if let Some(s) = &self.side {
            m.insert("side".into(), json!(s));
        }
        m.insert("mode".into(), json!(self.mode));
        if self.hk {
            m.insert("hk".into(), json!(true));
        }
        if self.committed {
            m.insert("committed".into(), json!(true));
        }
        m.insert(self.level.as_str().into(), json!(self.text));
        Value::Object(m)
    }
}

/// The ladder state: `{SYM: {...}, "_bal": {...}, ...}`. Housekeeping reads and writes
/// `cost_basis`, `win_until` and `win_dir` per coin and the `_bal` snapshot, and keeps
/// every other key as it found it.
pub type LadderState = Map<String, Value>;

/// Stale-order flags: client id -> when it was last flagged.
pub type TtlWarned = IndexMap<String, f64>;

/// The realized-P&L ledger, one record per filled sell, oldest first.
pub type PnlLedger = Vec<Value>;

/// What housekeeping changes.
pub struct Books<'a> {
    pub journal: &'a mut Journal,
    pub ladder: &'a mut LadderState,
    pub ttl: &'a mut TtlWarned,
    pub pnl: &'a mut PnlLedger,
}

/// Venue balances for this run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Balances {
    /// venue -> asset -> free and locked.
    pub full: BTreeMap<String, BTreeMap<String, Balance>>,
    /// Venues whose balance read failed this run.
    pub failed: BTreeSet<String>,
}

impl Balances {
    /// The free amount of `asset` on `exch`, 0 when unknown.
    pub fn free(&self, exch: &str, asset: &str) -> f64 {
        self.full
            .get(exch)
            .and_then(|b| b.get(asset))
            .map_or(0.0, |b| b.free)
    }
}

/// Read every venue's balances. A venue that fails is recorded in `failed` with an empty
/// book, and reported as `{venue} balances: {error}` (not housekeeping: it is mailed as
/// an error every run the venue stays down).
pub fn fetch_balances(venues: &dyn VenueSource, mode: &str) -> (Balances, Vec<Outcome>) {
    let mut b = Balances::default();
    let mut out = Vec::new();
    for name in BALANCE_VENUES {
        let read = venues
            .venue(name)
            .and_then(|v| v.balances_full().map_err(|e| e.to_string()));
        match read {
            Ok(full) => {
                b.full.insert(name.into(), full);
            }
            Err(e) => {
                b.full.insert(name.into(), BTreeMap::new());
                b.failed.insert(name.into());
                out.push(Outcome {
                    sym: None,
                    side: None,
                    mode: mode.into(),
                    hk: false,
                    committed: false,
                    level: Level::Err,
                    text: format!("{name} balances: {e}"),
                });
            }
        }
    }
    (b, out)
}

/// What this run knows besides the stores.
#[derive(Debug, Clone, Copy)]
pub struct RunCtx<'a> {
    pub now: f64,
    /// Trading is blocked (disarmed or halted): no retry, no reprice.
    pub blocked: bool,
    /// The regime's market label (`bull`, `bear`, `chop`, ...), `None` when unreadable.
    pub market: Option<&'a str>,
    /// Coins whose sell side trails the price this run.
    pub trailing: &'a BTreeSet<String>,
    /// Coins the bull sell policy governs this run.
    pub policy: &'a BTreeSet<String>,
}

/// Run every housekeeping step; see the module docs.
pub fn run(
    s: &Settings,
    ctx: &RunCtx,
    bal: &Balances,
    books: &mut Books,
    venues: &dyn VenueSource,
) -> Vec<Outcome> {
    let mut out = Vec::new();
    let rec = reconcile(books.journal, venues, ctx.now);
    for a in &rec.adopted {
        let o = &a.order;
        out.push(
            Outcome::new(
                Some(&o.sym),
                Level::Done,
                format!(
                    "ZONE RESIZED AT THE VENUE: {} rung {} @ ${} is now ${} (was ${}) — adopted \
                     the replacement order, the level is unchanged",
                    o.sym,
                    o.rung.map_or("None".into(), |r| r.to_string()),
                    g(o.price.unwrap_or(0.0), 6),
                    pyfmt::fixed(o.quote.unwrap_or(0.0), 2),
                    pyfmt::fixed(a.was_quote, 2),
                ),
            )
            .committed(),
        );
    }
    if let Err(e) = book_fills(s, ctx, bal, books, venues, &rec.filled, &mut out) {
        out.push(Outcome::new(None, Level::Err, format!("reconcile: {e}")));
    }
    let steps = (|| {
        if !ctx.blocked {
            retry_failed_limit_sells(books.journal, venues, &mut out)?;
            manage_resting_sells(s, ctx, books, venues, &mut out)?;
        }
        detect_drift(s, ctx.now, bal, books.journal, books.ladder, &mut out);
        Ok::<(), String>(())
    })();
    if let Err(e) = steps {
        out.push(Outcome::new(None, Level::Err, format!("housekeeping: {e}")));
    }
    out
}

/// The client for `exch`. A venue the run does not trade is an error that is just its
/// name, quoted: `'kraken'`.
fn client<'a>(venues: &'a dyn VenueSource, exch: &str) -> Result<&'a dyn Venue, String> {
    if !BALANCE_VENUES.contains(&exch) {
        return Err(pyfmt::repr_str(exch));
    }
    venues.venue(exch)
}

fn nonzero(v: Option<f64>) -> Option<f64> {
    v.filter(|x| *x != 0.0)
}

/// The recorded cost basis of `sym`, if any (zero counts as none).
pub fn cost_basis(state: &LadderState, sym: &str) -> Option<f64> {
    nonzero(
        state
            .get(sym)
            .and_then(Value::as_object)
            .and_then(|c| c.get("cost_basis"))
            .and_then(Value::as_f64),
    )
}

/// Fold a buy fill into `sym`'s cost basis. See [`hk::weighted_cost_basis`].
pub fn set_cost_basis(
    s: &Settings,
    state: &mut LadderState,
    sym: &str,
    held: f64,
    fill_quote: f64,
    fill_base: f64,
    fill_price: f64,
) {
    let new = hk::weighted_cost_basis(
        cost_basis(state, sym),
        s.entries.get(sym).copied(),
        held,
        fill_quote,
        fill_base,
        fill_price,
    );
    let coin = state
        .entry(sym.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !coin.is_object() {
        *coin = Value::Object(Map::new());
    }
    if let Some(c) = coin.as_object_mut() {
        c.insert("cost_basis".into(), json!(new));
    }
}

fn place(
    v: &dyn Venue,
    pair: &str,
    base: f64,
    price: f64,
    cid: &str,
) -> Result<ParsedOrder, String> {
    v.limit_sell(pair, base, price, Some(cid))
        .and_then(|raw| v.parse_order(&raw))
        .map_err(|e| e.to_string())
}

fn status_or_open(p: &ParsedOrder) -> String {
    if p.status.is_empty() {
        "open".into()
    } else {
        p.status.clone()
    }
}

fn limit_sell_row(cid: &str, sym: &str, exch: &str, pair: &str, base: f64, price: f64) -> Order {
    Order {
        client_id: cid.into(),
        sym: sym.into(),
        exch: exch.into(),
        pair: pair.into(),
        side: "sell".into(),
        kind: "limit_sell".into(),
        base: Some(base),
        price: Some(price),
        ..Default::default()
    }
}

/// Place the resting limit sell paired with a buy fill of `fill_base` at `fill_price`:
/// at fill +target net of the sell fee, journaled as `pending` before the venue call.
/// The id is this slot's sell id for `rung` plus `cid_suffix`. The outcome is not marked
/// housekeeping; a caller booking a reconciled fill marks it.
#[allow(clippy::too_many_arguments)]
pub fn pair_limit_sell(
    s: &Settings,
    j: &mut Journal,
    v: &dyn Venue,
    exch: &str,
    pair: &str,
    sym: &str,
    fill_base: f64,
    fill_price: f64,
    now: f64,
    rung: i64,
    cid_suffix: &str,
) -> Result<Outcome, String> {
    let lim = s.limit_target(fill_price, exch);
    let scid = ids::safe_cid(&(client_id(sym, Side::Sell, now, rung) + cid_suffix))
        .map_err(|e| e.to_string())?;
    let mut row = limit_sell_row(&scid, sym, exch, pair, fill_base, lim);
    row.status = "pending".into();
    row.rung = Some(rung);
    row.ts = Some(now);
    j.record(row);
    let mut res = match place(v, pair, fill_base, lim, &scid) {
        Ok(lf) => {
            j.update(&scid, |o| {
                o.status = status_or_open(&lf);
                o.order_id = Some(lf.order_id.clone());
            });
            Outcome::new(
                Some(sym),
                Level::Done,
                format!("LIMIT SELL placed @ ${}", g(lim, 6)),
            )
        }
        Err(e) => {
            j.update(&scid, |o| {
                o.status = "error".into();
                o.last_error = Some(e.clone());
            });
            Outcome::new(
                Some(sym),
                Level::Err,
                format!("LIMIT-SELL FAILED (auto-retries next runs): {e}"),
            )
        }
    };
    res.hk = false;
    Ok(res)
}

/// Append a realized-P&L record for a filled sell and return the realized USD (`None`
/// without a cost basis).
#[allow(clippy::too_many_arguments)]
pub fn log_pnl(
    ledger: &mut PnlLedger,
    sym: &str,
    qty: f64,
    quote: f64,
    avg_price: Option<f64>,
    cost_basis: Option<f64>,
    now: f64,
    kind: &str,
) -> Option<f64> {
    let realized = hk::realized(qty, quote, cost_basis);
    ledger.push(json!({
        "ts": now,
        "iso": rungbot_core::iso8601_micros(now),
        "sym": sym,
        "kind": kind,
        "qty": qty,
        "quote": quote,
        "avg_price": avg_price,
        "cost_basis": cost_basis,
        "realized_usd": realized,
        "realized_pct": hk::realized_pct(avg_price, cost_basis),
    }));
    realized
}

/// `; realized +1.23 USD (+4.5%)`, or nothing without a realized amount.
pub fn pnl_tail(realized: Option<f64>, cost_basis: Option<f64>, avg_price: Option<f64>) -> String {
    let Some(r) = realized else {
        return String::new();
    };
    let pct = hk::realized_pct(avg_price, cost_basis)
        .map(|p| format!(" ({p:+.1}%)"))
        .unwrap_or_default();
    format!("; realized {r:+.2} USD{pct}")
}

/// Step 1's second half: book each fill reconcile returned. See the module docs.
fn book_fills(
    s: &Settings,
    ctx: &RunCtx,
    bal: &Balances,
    books: &mut Books,
    venues: &dyn VenueSource,
    fills: &[Order],
    out: &mut Vec<Outcome>,
) -> Result<(), String> {
    let now = ctx.now;
    for o in fills {
        let sym = o.sym.as_str();
        let fb = o.filled_base.unwrap_or(0.0);
        let fq = o.filled_quote.unwrap_or(0.0);
        let fill_price = || nonzero(o.avg_price).unwrap_or(if fb != 0.0 { fq / fb } else { 0.0 });
        let head = format!("~{} {sym} (~${}", g(fb, 6), pyfmt::fixed(fq, 2));
        match o.kind.as_str() {
            "market_buy" => {
                let fp = fill_price();
                // The balance was read after this fill landed: leave it out of "held".
                let held = (bal.free(&o.exch, sym) - fb).max(0.0);
                set_cost_basis(s, books.ladder, sym, held, fq, fb, fp);
                if ctx.policy.contains(sym) {
                    out.push(
                        Outcome::new(
                            Some(sym),
                            Level::Done,
                            format!(
                                "BUY FILLED {head}); core add under the bull sell policy (no pair)"
                            ),
                        )
                        .side("buy")
                        .committed(),
                    );
                    continue;
                }
                let v = client(venues, &o.exch)?;
                let mut res = pair_limit_sell(
                    s,
                    books.journal,
                    v,
                    &o.exch,
                    &o.pair,
                    sym,
                    fb,
                    fp,
                    now,
                    o.rung.unwrap_or(1),
                    "d",
                )?;
                res.text = format!("BUY FILLED {head}); {}", res.text);
                res.hk = true;
                out.push(res.side("buy").committed());
            }
            "deploy_buy" => {
                if o.exch == "revx" && s.route(sym).is_none_or(|r| r.exch != "revx") {
                    out.push(
                        Outcome::new(
                            Some(sym),
                            Level::Done,
                            format!(
                                "REVX DEPLOY FILLED {head}) — held on revx, outside ladder-state"
                            ),
                        )
                        .side("buy")
                        .committed(),
                    );
                    continue;
                }
                let fp = fill_price();
                let held = (bal.free(&o.exch, sym) - fb).max(0.0);
                set_cost_basis(s, books.ladder, sym, held, fq, fb, fp);
                let partial = if o.partial == Some(true) {
                    " (partial, the rest was cancelled)"
                } else {
                    ""
                };
                out.push(
                    Outcome::new(
                        Some(sym),
                        Level::Done,
                        format!(
                            "DEPLOY BUY FILLED {head} @ ${}){partial} — core add: cost basis \
                             updated, no paired sell",
                            g(fp, 6)
                        ),
                    )
                    .side("buy")
                    .committed(),
                );
            }
            kind => {
                let cb = cost_basis(books.ladder, sym).or(nonzero(s.entries.get(sym).copied()));
                let kind = if kind.is_empty() { "limit_sell" } else { kind };
                let realized = log_pnl(books.pnl, sym, fb, fq, o.avg_price, cb, now, kind);
                out.push(
                    Outcome::new(
                        Some(sym),
                        Level::Done,
                        format!(
                            "LIMIT SELL FILLED {head}){}",
                            pnl_tail(realized, cb, o.avg_price)
                        ),
                    )
                    .side("sell")
                    .committed(),
                );
                if let Some(c) = books.ladder.get_mut(sym).and_then(Value::as_object_mut) {
                    c.insert("win_until".into(), json!(now + s.window_hours * 3600.0));
                    c.insert("win_dir".into(), json!("sell"));
                }
            }
        }
    }
    Ok(())
}

/// Step 2: retry every limit sell whose placement failed. See the module docs.
pub fn retry_failed_limit_sells(
    j: &mut Journal,
    venues: &dyn VenueSource,
    out: &mut Vec<Outcome>,
) -> Result<(), String> {
    let rows: Vec<Order> = j.errored(Some("limit_sell")).into_iter().cloned().collect();
    for o in rows {
        let (sym, exch, pair, cid) = (&o.sym, &o.exch, &o.pair, &o.client_id);
        let (Some(base), Some(lim)) = (nonzero(o.base), nonzero(o.price)) else {
            continue;
        };
        if sym.is_empty() || exch.is_empty() || pair.is_empty() || cid.is_empty() {
            continue;
        }
        let tries = o.retries.unwrap_or(0);
        if tries >= MAX_SELL_RETRIES {
            if o.gave_up != Some(true) {
                j.update(cid, |r| r.gave_up = Some(true));
                out.push(Outcome::new(
                    Some(sym),
                    Level::Warn,
                    format!(
                        "limit sell failed {tries}x, giving up -- place ~{} {sym} @ ${} manually",
                        g(base, 6),
                        g(lim, 6)
                    ),
                ));
            }
            continue;
        }
        let v = client(venues, exch)?;
        let resting = match v.open_orders(pair) {
            Ok(r) => r,
            Err(e) => {
                out.push(Outcome::new(
                    Some(sym),
                    Level::Warn,
                    format!("limit-sell retry: open_orders failed: {e}"),
                ));
                continue;
            }
        };
        let adopted = resting.iter().find(|x| {
            venue_id_matches(exch, &x.client_id, cid)
                || nonzero(x.price).is_some_and(|p| (p / lim - 1.0).abs() < ADOPT_PRICE_TOLERANCE)
        });
        if let Some(a) = adopted {
            j.update(cid, |r| {
                r.status = "open".into();
                r.order_id = Some(a.order_id.clone());
                r.last_error = None;
            });
            let price = a.price.ok_or(NONE_FORMAT)?;
            out.push(Outcome::new(
                Some(sym),
                Level::Done,
                format!(
                    "adopted resting limit sell @ ${} (original placement had succeeded)",
                    g(price, 6)
                ),
            ));
            continue;
        }
        let placed = ids::safe_cid(&format!("{cid}x{}", tries + 1))
            .map_err(|e| e.to_string())
            .and_then(|rcid| place(v, pair, base, lim, &rcid));
        match placed {
            Ok(lf) => {
                j.update(cid, |r| {
                    r.status = status_or_open(&lf);
                    r.order_id = Some(lf.order_id.clone());
                    r.retries = Some(tries + 1);
                    r.last_error = None;
                });
                out.push(
                    Outcome::new(
                        Some(sym),
                        Level::Done,
                        format!("RETRY OK: limit sell {} {sym} @ ${}", g(base, 6), g(lim, 6)),
                    )
                    .committed(),
                );
            }
            Err(e) => {
                j.update(cid, |r| {
                    r.retries = Some(tries + 1);
                    r.last_error = Some(e.clone());
                });
                out.push(Outcome::new(
                    Some(sym),
                    Level::Warn,
                    format!(
                        "limit-sell retry {}/{MAX_SELL_RETRIES} failed: {e}",
                        tries + 1
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Should `cid` be flagged stale now? Records the flag when it says yes, and prunes the
/// flags of orders no longer in `open` (when given).
pub fn ttl_should_warn(
    ttl: &mut TtlWarned,
    cid: &str,
    now: f64,
    open: Option<&BTreeSet<String>>,
    renag_days: f64,
) -> bool {
    if !hk::ttl_due(ttl.get(cid).copied(), now, renag_days) {
        return false;
    }
    ttl.insert(cid.to_string(), now);
    if let Some(open) = open {
        ttl.retain(|k, _| open.contains(k) || k == cid);
    }
    true
}

/// The 30-minute slot a reprice or restore id is suffixed with.
fn slot(now: f64) -> i64 {
    ((now / 1800.0).floor() as i64) % 100_000
}

/// Step 3: move resting limit sells up to a risen target, flag stale ones. See the
/// module docs.
pub fn manage_resting_sells(
    s: &Settings,
    ctx: &RunCtx,
    books: &mut Books,
    venues: &dyn VenueSource,
    out: &mut Vec<Outcome>,
) -> Result<(), String> {
    let now = ctx.now;
    let ttl_days = hk::effective_ttl_days(
        s.limit_ttl_days,
        s.limit_ttl_days_bear,
        s.limit_ttl_days_chop,
        ctx.market,
    );
    let rows: Vec<Order> = books
        .journal
        .open_orders(Some("limit_sell"))
        .into_iter()
        .cloned()
        .collect();
    let open: BTreeSet<String> = rows.iter().map(|o| o.client_id.clone()).collect();
    for o in rows {
        let (sym, exch, pair, cid) = (&o.sym, &o.exch, &o.pair, &o.client_id);
        let Some(order_id) = o.order_id.clone().filter(|id| !id.is_empty()) else {
            continue;
        };
        if sym.is_empty() || exch.is_empty() || pair.is_empty() || cid.is_empty() {
            continue;
        }
        let (base, price) = (o.base, o.price);
        let mut tgt = cost_basis(books.ladder, sym).map(|cb| s.limit_target(cb, exch));
        let v = client(venues, exch)?;
        let mut band = REPRICE_BAND;
        if ctx.trailing.contains(sym) {
            if let Ok(p) = v.price(pair) {
                tgt = Some(tgt.unwrap_or(0.0).max(s.limit_target(p, exch)));
                band = REPRICE_BAND_TRAILING;
            }
        }
        let reprice = match (nonzero(tgt), nonzero(price), nonzero(base)) {
            (Some(t), Some(p), Some(b)) if p < t * band => Some((t, p, b)),
            _ => None,
        };
        if let Some((tgt, price, base)) = reprice {
            let ncid = suffixed(cid, &format!("u{}", slot(now)));
            if books.journal.exists(&ncid) {
                continue;
            }
            if let Err(e) = v.cancel(pair, &order_id) {
                out.push(Outcome::new(
                    Some(sym),
                    Level::Warn,
                    format!(
                        "reprice: cancel failed (may have just filled, reconciles next run): {e}"
                    ),
                ));
                continue;
            }
            books.journal.update(cid, |r| {
                r.status = "canceled".into();
                r.note = Some("repriced up to new cost-basis target".into());
            });
            let mut base = base;
            if let Settled::Booked(done) = settle_cancel(books.journal, v, cid, now) {
                // Part sold before the cancel: that part is booked (its P&L comes with the
                // next reconcile); re-place only what is still held.
                base = (base - done.filled_base.unwrap_or(0.0)).max(0.0);
                if base <= 0.0 {
                    continue;
                }
            }
            let rung = o.rung.unwrap_or(1);
            let mut row = limit_sell_row(&ncid, sym, exch, pair, base, tgt);
            row.status = "pending".into();
            row.rung = Some(rung);
            row.ts = Some(now);
            books.journal.record(row);
            match place(v, pair, base, tgt, &ncid) {
                Ok(lf) => {
                    books.journal.update(&ncid, |r| {
                        r.status = status_or_open(&lf);
                        r.order_id = Some(lf.order_id.clone());
                    });
                    out.push(
                        Outcome::new(
                            Some(sym),
                            Level::Done,
                            format!(
                                "REPRICED limit sell up: ${} -> ${} (cost basis rose; never \
                                 sell below entry +{}%)",
                                g(price, 6),
                                g(tgt, 6),
                                pyfmt::fixed(s.target_pct, 0)
                            ),
                        )
                        .committed(),
                    );
                }
                Err(e) => {
                    books.journal.update(&ncid, |r| {
                        r.status = "error".into();
                        r.last_error = Some(e.clone());
                    });
                    // The cancel went through, so the position is uncovered right now.
                    // Restore cover at the old price, the one the venue already accepted.
                    let rcid = suffixed(cid, &format!("v{}", slot(now)));
                    if books.journal.exists(&rcid) {
                        continue;
                    }
                    match place(v, pair, base, price, &rcid) {
                        Ok(rf) => {
                            let mut row = limit_sell_row(&rcid, sym, exch, pair, base, price);
                            row.status = status_or_open(&rf);
                            row.order_id = Some(rf.order_id.clone());
                            row.rung = Some(rung);
                            row.ts = Some(now);
                            row.note = Some(format!(
                                "cover restored at prior price after {ncid} was rejected"
                            ));
                            books.journal.record(row);
                            // Retire the failed one so the retry path cannot add a second cover.
                            books.journal.update(&ncid, |r| {
                                r.status = "canceled".into();
                                r.note = Some(format!("cover restored as {rcid}"));
                            });
                            out.push(
                                Outcome::new(
                                    Some(sym),
                                    Level::Warn,
                                    format!(
                                        "reprice to ${} rejected ({e}); cover RESTORED at ${} -- \
                                         position is protected, retries next run",
                                        g(tgt, 6),
                                        g(price, 6)
                                    ),
                                )
                                .committed(),
                            );
                        }
                        Err(e2) => out.push(Outcome::new(
                            Some(sym),
                            Level::Warn,
                            format!(
                                "reprice re-place failed ({e}) AND restore failed ({e2}) -- {} \
                                 {sym} is UNPROTECTED, place @ ${} manually",
                                g(base, 6),
                                g(price, 6)
                            ),
                        )),
                    }
                }
            }
        } else if ttl_days > 0.0
            && now - nonzero(o.ts).unwrap_or(now) > ttl_days * 86_400.0
            && ttl_should_warn(books.ttl, cid, now, Some(&open), s.ttl_renag_days)
        {
            let (base, price) = (base.ok_or(NONE_FORMAT)?, price.ok_or(NONE_FORMAT)?);
            out.push(Outcome::new(
                Some(sym),
                Level::Warn,
                format!(
                    "resting limit sell ({} {sym} @ ${}) is older than {}d -- price never \
                     reached the target even for this regime; review whether to keep it",
                    g(base, 6),
                    g(price, 6),
                    pyfmt::fixed(ttl_days, 0)
                ),
            ));
        }
    }
    Ok(())
}

/// Step 4: holding drift. See the module docs.
pub fn detect_drift(
    s: &Settings,
    now: f64,
    bal: &Balances,
    j: &Journal,
    state: &mut LadderState,
    out: &mut Vec<Outcome>,
) {
    let snap = state
        .get("_bal")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let prev_ts = snap.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let prev_hold = snap
        .get("hold")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let prev_booked = snap.get("booked").and_then(Value::as_object);
    let num = |v: Option<&Value>| v.and_then(Value::as_f64);

    let mut cur: IndexMap<String, f64> = IndexMap::new();
    let mut skipped: BTreeSet<&str> = BTreeSet::new();
    for r in &s.routing {
        if bal.failed.contains(&r.exch) {
            skipped.insert(&r.sym);
            if let Some(v) = num(prev_hold.get(&r.sym)) {
                cur.insert(r.sym.clone(), v);
            }
            continue;
        }
        let b = bal
            .full
            .get(&r.exch)
            .and_then(|b| b.get(&r.sym))
            .copied()
            .unwrap_or_default();
        cur.insert(r.sym.clone(), b.free + b.locked);
    }
    let mut booked = Map::new();
    let mut explained: IndexMap<String, f64> = cur.keys().map(|k| (k.clone(), 0.0)).collect();
    for o in j.orders.values() {
        let sign = if o.side == "buy" { 1.0 } else { -1.0 };
        let mut q = o.booked_base() * sign;
        let was = prev_booked
            .and_then(|b| num(b.get(&o.client_id)))
            .unwrap_or(0.0);
        if skipped.contains(o.sym.as_str()) {
            q = was; // the recovery run explains it
        } else if let Some(e) = explained.get_mut(&o.sym) {
            *e += q - was;
        }
        if q != 0.0 {
            booked.insert(o.client_id.clone(), json!(q));
        }
    }
    // No per-order snapshot yet: write one, judge next run.
    if prev_ts != 0.0 && prev_booked.is_some() {
        for (sym, have) in &cur {
            let Some(prev) = num(prev_hold.get(sym)) else {
                continue;
            };
            if skipped.contains(sym.as_str()) {
                continue;
            }
            let expect = prev + explained[sym];
            if hk::drift_exceeds(*have, expect) {
                out.push(Outcome::new(
                    Some(sym),
                    Level::Warn,
                    format!(
                        "holding drift: have ~{}, journal expects ~{} -- manual trade or \
                         transfer? cost basis and ladder ledgers may be stale for {sym}",
                        g(*have, 6),
                        g(expect, 6)
                    ),
                ));
            }
        }
    }
    let hold: Map<String, Value> = cur.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    state.insert(
        "_bal".into(),
        json!({"ts": now, "hold": hold, "booked": booked}),
    );
}
