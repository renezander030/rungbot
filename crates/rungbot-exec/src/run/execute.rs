//! Signals become orders: `dry` plans them, `live` places market orders, inside the
//! rails. A live run also does its housekeeping here, before the first new order.
//!
//! The rails, in the order a signal meets them:
//!
//! * live placement needs `trade_mode: live`, `live_trading_enabled` and no halt file;
//! * the venue price must sit within `max_slippage_pct` of the signal's price (bull
//!   policy sells are exempt);
//! * a sell must still clear entry +`target_pct` at the venue price;
//! * one order is clamped to `max_order_usd` (not a policy sell); an order under the
//!   venue minimum is skipped;
//! * per-day notional and order caps (not a policy sell), and a per-ISO-week cap on buys;
//! * the buy budget is the venue's free quote asset, less the pending onramp top-up,
//!   at most `usdc_bag_usd` when set, split evenly across the venue's coins.
//!
//! Every order is journaled and written to disk before the venue sees it, and its
//! answer written right after. A journal write that fails stops the run from placing
//! anything more: the signals left are skipped (their ladder entries roll back) and one
//! `fatal` result names the failure. Nothing here returns early past a venue call, so
//! the caller always gets every result and still saves the ladder state, the cap
//! counters and the P&L ledger.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use super::config::RunConfig;
use super::funding;
use super::signals::Signal;
use super::RunResult;
use crate::housekeeping::{
    self, cost_basis, fetch_balances, log_pnl, pair_limit_sell, pnl_tail, set_cost_basis, Balances,
    Books, LadderState, Persist, Route, RunCtx, Settings,
};
use crate::ids;
use crate::journal::{client_id, Order, Side};
use crate::pyfmt::{self, g};
use crate::reconcile::VenueSource;
use crate::venue::Venue;

/// Everything execution reads and writes besides the signals.
pub struct ExecIo<'a> {
    pub venues: &'a dyn VenueSource,
    pub books: Books<'a>,
    /// Writes the journal and the P&L ledger. The journal is written after every row is
    /// recorded and before the venue sees the order, so a crash can never leave an order
    /// the journal forgot.
    pub persist: &'a mut Persist,
}

/// What this run knows about the market and the policy.
#[derive(Debug, Clone, Default)]
pub struct ExecCtx {
    pub now: f64,
    pub halted: bool,
    /// The regime's market label, when it could be read.
    pub market: Option<String>,
    /// Coins whose sell side trails.
    pub trailing: BTreeSet<String>,
    /// The bull sell policy governs sells.
    pub policy_on: bool,
}

/// The housekeeping settings for this config.
pub fn hk_settings(cfg: &RunConfig) -> Settings {
    Settings {
        target_pct: cfg.target_pct,
        fees: rungbot_core::housekeeping::Fees {
            gate: cfg.fee_pct_gate,
            revx: cfg.fee_pct_revx,
            binance: cfg.fee_pct_binance,
        },
        window_hours: cfg.window_hours,
        limit_ttl_days: cfg.limit_ttl_days,
        limit_ttl_days_bear: cfg.limit_ttl_days_bear,
        limit_ttl_days_chop: cfg.limit_ttl_days_chop,
        ttl_renag_days: cfg.ttl_renag_days,
        routing: cfg
            .routing
            .iter()
            .map(|(s, r)| Route {
                sym: s.clone(),
                exch: r.exch.clone(),
                pair: r.pair.clone(),
                quote: r.quote.clone(),
            })
            .collect(),
        entries: cfg.entries.clone(),
    }
}

fn num(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

struct Ledgers {
    day: String,
    day_notional: f64,
    day_orders: i64,
    week: String,
    week_notional: f64,
}

impl Ledgers {
    fn load(state: &LadderState, now: f64) -> Ledgers {
        let day = rungbot_core::time::utc_day(now);
        let week = rungbot_core::time::iso_week(now);
        let d = state.get("_daily").filter(|v| v.is_object());
        let w = state.get("_weekly").filter(|v| v.is_object());
        let same_day = d.and_then(|d| d.get("date")).and_then(Value::as_str) == Some(&day);
        let same_week = w.and_then(|w| w.get("week")).and_then(Value::as_str) == Some(&week);
        Ledgers {
            day_notional: if same_day {
                num(d.and_then(|d| d.get("notional")))
            } else {
                0.0
            },
            day_orders: if same_day {
                num(d.and_then(|d| d.get("orders"))) as i64
            } else {
                0
            },
            week_notional: if same_week {
                num(w.and_then(|w| w.get("notional")))
            } else {
                0.0
            },
            day,
            week,
        }
    }

    fn store(&self, state: &mut LadderState) {
        state.insert(
            "_daily".into(),
            json!({"date": self.day, "notional": self.day_notional, "orders": self.day_orders}),
        );
        state.insert(
            "_weekly".into(),
            json!({"week": self.week, "notional": self.week_notional}),
        );
    }
}

/// The error line of a run stopped by a journal write that failed.
pub fn journal_fatal_text(e: &str) -> String {
    format!(
        "{e} -- stopped placing orders for this run. The journal on disk may be missing \
         an order a venue now holds: fix the disk, then run `rungbot-exec reconcile` \
         before the next run"
    )
}

fn res(sym: &str, side: &str, mode: &str) -> RunResult {
    RunResult {
        sym: Some(sym.into()),
        side: Some(side.into()),
        mode: Some(mode.into()),
        ..Default::default()
    }
}

/// Turn signals into orders. See the module docs.
pub fn execute_trades(
    cfg: &RunConfig,
    buys: &[Signal],
    sells: &[Signal],
    ctx: &ExecCtx,
    io: &mut ExecIo,
) -> Vec<RunResult> {
    let mode = cfg.trade_mode.as_str();
    if mode != "dry" && mode != "live" {
        return Vec::new();
    }
    if mode == "dry" && buys.is_empty() && sells.is_empty() {
        return Vec::new();
    }
    let now = ctx.now;
    let mut results: Vec<RunResult> = Vec::new();

    let (bal, errs) = fetch_balances(io.venues, mode);
    results.extend(errs.iter().map(RunResult::from_outcome));
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, r) in &cfg.routing {
        *counts.entry(r.exch.as_str()).or_insert(0) += 1;
    }

    let live = mode == "live";
    let block: Option<String> = if live && !cfg.live_trading_enabled {
        Some("LIVE_TRADING_ENABLED is not 'yes'".into())
    } else if live && ctx.halted {
        Some(format!("halt file present ({})", cfg.halt_file.display()))
    } else {
        None
    };

    let mut led = Ledgers::load(io.books.ladder, now);
    led.store(io.books.ladder);

    let settings = hk_settings(cfg);
    let policy_set: BTreeSet<String> = if ctx.policy_on {
        cfg.routing
            .iter()
            .map(|(s, _)| s.clone())
            .chain(cfg.watchlist.iter().map(|(s, _)| s.clone()))
            .collect()
    } else {
        BTreeSet::new()
    };

    if live {
        let rctx = RunCtx {
            now,
            blocked: block.is_some(),
            market: ctx.market.as_deref(),
            trailing: &ctx.trailing,
            policy: &policy_set,
        };
        let before = io.books.journal.clone();
        let out = housekeeping::run(&settings, &rctx, &bal, &mut io.books, io.venues, io.persist);
        results.extend(out.iter().map(RunResult::from_outcome));
        if *io.books.journal != before {
            let _ = io.persist.journal(io.books.journal);
        }
    }

    // The pending top-up to the second venue stays free on the onramp venue.
    let mut onramp_held = 0.0;
    let rxb = bal.full.get("revx").filter(|b| !b.is_empty());
    let gtb = bal.full.get("gate").filter(|b| !b.is_empty());
    if let (Some(rxb), Some(gtb)) = (rxb, gtb) {
        let routing: Vec<(String, String)> = cfg
            .routing
            .iter()
            .map(|(s, r)| (s.clone(), r.exch.clone()))
            .collect();
        let reserve = funding::onramp_reserve(
            funding::revx_stable_from(rxb),
            funding::gate_stable_from(gtb),
            &cfg.deploy_alloc,
            &routing,
        );
        let staged = rxb.get("USDC").map_or(0.0, |b| b.free);
        onramp_held = (reserve - staged).max(0.0);
    }

    let slippage_ok = |venue: f64, signal: f64| {
        !(signal != 0.0
            && cfg.max_slippage_pct > 0.0
            && (venue / signal - 1.0).abs() > cfg.max_slippage_pct / 100.0)
    };
    let daily_room = |led: &Ledgers, notional: f64| -> Option<String> {
        if cfg.max_daily_orders > 0 && led.day_orders >= cfg.max_daily_orders {
            return Some(format!(
                "daily order cap reached ({})",
                cfg.max_daily_orders
            ));
        }
        if cfg.max_daily_notional > 0.0 && led.day_notional + notional > cfg.max_daily_notional {
            return Some(format!(
                "daily notional cap reached (${:.0})",
                cfg.max_daily_notional
            ));
        }
        None
    };
    let weekly_room = |led: &Ledgers, notional: f64| -> Option<String> {
        if cfg.max_weekly_notional > 0.0 && led.week_notional + notional > cfg.max_weekly_notional {
            return Some(format!(
                "weekly buy cap reached (${:.0}/wk)",
                cfg.max_weekly_notional
            ));
        }
        None
    };

    // ---- BUYS: a market buy, then its paired resting limit sell
    for r in buys {
        let Some(route) = cfg.route(&r.sym) else {
            continue;
        };
        if let Some(e) = io.persist.failed() {
            let mut x = res(&r.sym, "buy", mode);
            x.skip = Some(format!("not placed: {e}"));
            results.push(x);
            continue;
        }
        let (exch, pair, quote) = (
            route.exch.as_str(),
            route.pair.as_str(),
            route.quote.as_str(),
        );
        let client = match io.venues.venue(exch) {
            Ok(c) => c,
            Err(e) => {
                let mut x = res(&r.sym, "buy", mode);
                x.err = Some(e);
                results.push(x);
                continue;
            }
        };
        let mut stable = bal.free(exch, quote);
        if exch == "revx" && onramp_held != 0.0 {
            stable = (stable - onramp_held).max(0.0);
        }
        if cfg.usdc_bag_usd > 0.0 {
            stable = stable.min(cfg.usdc_bag_usd);
        }
        let base_bag = stable / *counts.get(exch).unwrap_or(&1) as f64;
        let mut notional = r.pct / 100.0 * base_bag;
        let (price, minq) = match client
            .price(pair)
            .and_then(|p| client.limits(pair).map(|l| (p, l.min_quote)))
        {
            Ok(v) => v,
            Err(e) => {
                let mut x = res(&r.sym, "buy", mode);
                x.err = Some(e.to_string());
                results.push(x);
                continue;
            }
        };
        if !slippage_ok(price, r.usd) {
            let mut x = res(&r.sym, "buy", mode);
            x.skip = Some(format!(
                "venue ${} diverges >{:.0}% from signal ${}",
                g(price, 6),
                cfg.max_slippage_pct,
                g(r.usd, 6)
            ));
            results.push(x);
            continue;
        }
        if cfg.max_order_usd > 0.0 && notional > cfg.max_order_usd {
            notional = cfg.max_order_usd;
        }
        if notional < minq.max(0.01) {
            let mut x = res(&r.sym, "buy", mode);
            x.skip = Some(format!("${notional:.2} {quote} < min ${minq:.2}"));
            results.push(x);
            continue;
        }
        let qty_est = if price != 0.0 { notional / price } else { 0.0 };
        let lim = price * (1.0 + cfg.target_pct / 100.0);
        if !live {
            let mut x = res(&r.sym, "buy", "dry");
            x.plan = Some(format!(
                "BUY ${notional:.2} {quote} of {pair} @ ~${}; then LIMIT SELL ~{} @ ${} (+{:.0}%)",
                pyfmt::float_repr(price),
                g(qty_est, 6),
                g(lim, 6),
                cfg.target_pct
            ));
            results.push(x);
        } else if let Some(b) = &block {
            let mut x = res(&r.sym, "buy", "live");
            x.skip = Some(format!("live blocked: {b}"));
            results.push(x);
        } else if let Some(cap) = daily_room(&led, notional).or_else(|| weekly_room(&led, notional))
        {
            let mut x = res(&r.sym, "buy", "live");
            x.skip = Some(cap);
            results.push(x);
        } else {
            let x = live_buy(
                cfg, &settings, ctx, io, client, exch, pair, r, notional, price, &bal,
            );
            if x.committed {
                led.day_orders += 1;
                led.day_notional += notional;
                led.week_notional += notional;
            }
            results.push(x);
        }
    }

    // ---- SELLS: take-profit exits
    for r in sells {
        let Some(route) = cfg.route(&r.sym) else {
            continue;
        };
        if let Some(e) = io.persist.failed() {
            let mut x = res(&r.sym, "sell", mode);
            x.skip = Some(format!("not placed: {e}"));
            results.push(x);
            continue;
        }
        let (exch, pair) = (route.exch.as_str(), route.pair.as_str());
        let client = match io.venues.venue(exch) {
            Ok(c) => c,
            Err(e) => {
                let mut x = res(&r.sym, "sell", mode);
                x.err = Some(e);
                results.push(x);
                continue;
            }
        };
        let held = bal.free(exch, &r.sym);
        let read = client.price(pair).and_then(|p| {
            let q = client.round_amount(pair, r.pct / 100.0 * held)?;
            let l = client.limits(pair)?;
            Ok((p, q, l))
        });
        let (price, mut qty, limits) = match read {
            Ok(v) => v,
            Err(e) => {
                let mut x = res(&r.sym, "sell", mode);
                x.err = Some(e.to_string());
                results.push(x);
                continue;
            }
        };
        let policy = r.policy.as_deref().is_some_and(|p| !p.is_empty());
        if !policy && !slippage_ok(price, r.usd) {
            let mut x = res(&r.sym, "sell", mode);
            x.skip = Some(format!(
                "venue ${} diverges >{:.0}% from signal ${}",
                g(price, 6),
                cfg.max_slippage_pct,
                g(r.usd, 6)
            ));
            results.push(x);
            continue;
        }
        let tgt = r
            .entry
            .filter(|e| *e != 0.0)
            .map(|e| e * (1.0 + cfg.target_pct / 100.0));
        if let Some(t) = tgt.filter(|t| *t != 0.0 && price < *t) {
            let mut x = res(&r.sym, "sell", mode);
            x.skip = Some(format!(
                "venue ${} below take-profit ${} (signal stale)",
                g(price, 6),
                g(t, 6)
            ));
            results.push(x);
            continue;
        }
        if !policy && cfg.max_order_usd > 0.0 && qty * price > cfg.max_order_usd {
            qty = match client.round_amount(pair, cfg.max_order_usd / price) {
                Ok(q) => q,
                Err(e) => {
                    let mut x = res(&r.sym, "sell", mode);
                    x.err = Some(e.to_string());
                    results.push(x);
                    continue;
                }
            };
        }
        let notional = qty * price;
        if qty < limits.min_base || notional < limits.min_quote.max(0.01) {
            let mut x = res(&r.sym, "sell", mode);
            x.skip = Some(format!(
                "{} {} (~${notional:.2}) below exchange min",
                g(qty, 6),
                r.sym
            ));
            results.push(x);
            continue;
        }
        if !live {
            let mut x = res(&r.sym, "sell", "dry");
            x.plan = Some(format!(
                "SELL {} {} (~${notional:.2}) on {pair} @ ~${}",
                g(qty, 6),
                r.sym,
                pyfmt::float_repr(price)
            ));
            results.push(x);
            continue;
        }
        if let Some(b) = &block {
            let mut x = res(&r.sym, "sell", "live");
            x.skip = Some(format!("live blocked: {b}"));
            results.push(x);
            continue;
        }
        if !policy {
            if let Some(cap) = daily_room(&led, notional) {
                let mut x = res(&r.sym, "sell", "live");
                x.skip = Some(cap);
                results.push(x);
                continue;
            }
        }
        let mut x = live_sell(cfg, ctx, io, client, exch, pair, r, qty, price);
        x.policy = policy;
        if x.committed {
            led.day_orders += 1;
            led.day_notional += notional;
        }
        results.push(x);
    }
    led.store(io.books.ladder);
    if let Some(e) = io.persist.failed() {
        results.push(RunResult {
            mode: Some(mode.into()),
            hk: true,
            fatal: true,
            err: Some(journal_fatal_text(e)),
            ..Default::default()
        });
    }
    results
}

/// A journaled market buy; on a fill, the fill-based cost basis and the paired sell.
#[allow(clippy::too_many_arguments)]
fn live_buy(
    cfg: &RunConfig,
    settings: &Settings,
    ctx: &ExecCtx,
    io: &mut ExecIo,
    client: &dyn Venue,
    exch: &str,
    pair: &str,
    r: &Signal,
    notional: f64,
    price: f64,
    bal: &Balances,
) -> RunResult {
    let now = ctx.now;
    let sym = r.sym.as_str();
    let mut out = res(sym, "buy", "live");
    let cid = match ids::safe_cid(&client_id(sym, Side::Buy, now, r.rung)) {
        Ok(c) => c,
        Err(e) => {
            out.err = Some(e.to_string());
            return out;
        }
    };
    if io.books.journal.exists(&cid) {
        out.skip = Some("already journaled this slot (idempotent)".into());
        return out;
    }
    io.books.journal.record(Order {
        client_id: cid.clone(),
        sym: sym.into(),
        exch: exch.into(),
        pair: pair.into(),
        side: "buy".into(),
        kind: "market_buy".into(),
        status: "pending".into(),
        quote: Some(notional),
        rung: Some(r.rung),
        ts: Some(now),
        ..Default::default()
    });
    if let Err(e) = io.persist.journal(io.books.journal) {
        io.books.journal.orders.shift_remove(&cid);
        out.skip = Some(format!("not placed: {e}"));
        return out;
    }
    let fill = client
        .market_buy(pair, notional, Some(&cid))
        .and_then(|raw| client.parse_order(&raw));
    let fill = match fill {
        Ok(f) => f,
        Err(e) => {
            io.books.journal.update(&cid, |o| {
                o.status = "error".into();
                o.last_error = Some(e.to_string());
            });
            let _ = io.persist.journal(io.books.journal);
            out.err = Some(format!("market buy: {e}"));
            return out;
        }
    };
    let base = fill.base_qty.filter(|b| *b != 0.0);
    if !fill.filled && base.is_none() {
        // Settles later: reconcile books the real fill and pairs the sell then.
        io.books.journal.update(&cid, |o| {
            o.status = if fill.status.is_empty() {
                "open".into()
            } else {
                fill.status.clone()
            };
            o.order_id = Some(fill.order_id.clone());
        });
        let _ = io.persist.journal(io.books.journal);
        out.committed = true;
        out.done = Some(format!(
            "BUY ~${notional:.2} placed, awaiting fill; limit sell pairs once the fill reconciles"
        ));
        return out;
    }
    let mut fb = base.unwrap_or(if price != 0.0 { notional / price } else { 0.0 });
    if fill.fee != 0.0 && fill.fee_asset.as_deref() == Some(sym) {
        fb = (fb - fill.fee).max(0.0);
    }
    let fp = fill.avg_price.filter(|p| *p != 0.0).unwrap_or(price);
    let fq = if fill.quote != 0.0 {
        fill.quote
    } else {
        fb * fp
    };
    io.books.journal.update(&cid, |o| {
        o.status = "filled".into();
        o.order_id = Some(fill.order_id.clone());
        o.filled_base = Some(fb);
        o.filled_quote = Some(fq);
        o.avg_price = Some(fp);
        o.filled_ts = Some(now);
    });
    let _ = io.persist.journal(io.books.journal);
    set_cost_basis(
        settings,
        io.books.ladder,
        sym,
        bal.free(exch, sym),
        fq,
        fb,
        fp,
    );
    out.committed = true;
    if ctx.policy_on {
        out.done = Some(format!(
            "BOUGHT ~{} {sym} (~${fq:.2}); core add under the bull sell policy (no +{:.0}% pair)",
            g(fb, 6),
            cfg.target_pct
        ));
        return out;
    }
    let paired = pair_limit_sell(
        settings,
        io.books.journal,
        io.persist,
        client,
        exch,
        pair,
        sym,
        fb,
        fp,
        now,
        r.rung,
        "",
    );
    let head = format!("BOUGHT ~{} {sym} (~${fq:.2})", g(fb, 6));
    match paired {
        Ok(p) if p.level == housekeeping::Level::Done => {
            out.done = Some(format!("{head}; {}", p.text))
        }
        Ok(p) => {
            out.err = Some(format!(
                "MANUAL ACTION: bought ~{} {sym} but {}",
                g(fb, 6),
                p.text
            ))
        }
        Err(e) => {
            out.err = Some(format!(
                "MANUAL ACTION: bought ~{} {sym} but the limit sell was not placed: {e}",
                g(fb, 6)
            ))
        }
    }
    out
}

/// A journaled market take-profit sell, logged to the P&L ledger.
#[allow(clippy::too_many_arguments)]
fn live_sell(
    cfg: &RunConfig,
    ctx: &ExecCtx,
    io: &mut ExecIo,
    client: &dyn Venue,
    exch: &str,
    pair: &str,
    r: &Signal,
    qty: f64,
    price: f64,
) -> RunResult {
    let now = ctx.now;
    let sym = r.sym.as_str();
    let mut out = res(sym, "sell", "live");
    // A leading `m` keeps market-sell ids apart from the paired limit sells.
    let cid = match ids::safe_cid(&format!("m{}", client_id(sym, Side::Sell, now, r.rung))) {
        Ok(c) => c,
        Err(e) => {
            out.err = Some(e.to_string());
            return out;
        }
    };
    if io.books.journal.exists(&cid) {
        out.skip = Some("already journaled this slot (idempotent)".into());
        return out;
    }
    io.books.journal.record(Order {
        client_id: cid.clone(),
        sym: sym.into(),
        exch: exch.into(),
        pair: pair.into(),
        side: "sell".into(),
        kind: "market_sell".into(),
        status: "pending".into(),
        base: Some(qty),
        rung: Some(r.rung),
        ts: Some(now),
        ..Default::default()
    });
    if let Err(e) = io.persist.journal(io.books.journal) {
        io.books.journal.orders.shift_remove(&cid);
        out.skip = Some(format!("not placed: {e}"));
        return out;
    }
    let fill = client
        .market_sell(pair, qty, Some(&cid))
        .and_then(|raw| client.parse_order(&raw));
    match fill {
        Ok(fill) => {
            io.books.journal.update(&cid, |o| {
                o.status = "filled".into();
                o.order_id = Some(fill.order_id.clone());
                o.filled_base = fill.base_qty;
                o.filled_quote = Some(fill.quote);
                o.avg_price = fill.avg_price;
                o.filled_ts = Some(now);
            });
            let _ = io.persist.journal(io.books.journal);
            let fq = if fill.quote != 0.0 {
                fill.quote
            } else {
                qty * price
            };
            let fqty = fill.base_qty.filter(|b| *b != 0.0).unwrap_or(qty);
            let cb = cost_basis(io.books.ladder, sym)
                .or_else(|| cfg.entries.get(sym).copied().filter(|e| *e != 0.0));
            let avg = fill.avg_price.filter(|p| *p != 0.0).unwrap_or(price);
            let realized = log_pnl(
                io.books.pnl,
                sym,
                fqty,
                fq,
                Some(avg),
                cb,
                now,
                "market_sell",
            );
            io.persist.pnl(io.books.pnl);
            out.committed = true;
            out.done = Some(format!(
                "SOLD ~{} {sym} (~${fq:.2}){}",
                g(fqty, 6),
                pnl_tail(realized, cb, Some(avg))
            ));
        }
        Err(e) => {
            io.books.journal.update(&cid, |o| {
                o.status = "error".into();
                o.last_error = Some(e.to_string());
            });
            let _ = io.persist.journal(io.books.journal);
            out.err = Some(format!("market sell: {e}"));
        }
    }
    out
}
