//! `data.json`: the dashboard's main snapshot of the book.
//!
//! Balances come from the venues (Gate: free and locked; Revolut X: its own account read
//! with a 30-minute last-good cache), prices from the venue tickers, the rest from the
//! run's files: ladder state, order journal, decision log, audit state and the deploy
//! streak. Nothing here writes to a venue.

use std::collections::BTreeMap;
use std::path::Path;

use rungbot_core::watch::json::{obj, Json};
use rungbot_core::watch::pyfmt::{fixed, sum as pysum};

use super::net::{self, NetError};
use super::{
    float_or0, head, key_str, mtime, py_float, py_sum, read_json_or, type_name, DashConfig, Io, Num,
};
use crate::run::config::RunConfig;
use crate::sellcheck;

/// Order statuses that are still resting.
pub const OPEN_ST: [&str; 6] = [
    "pending",
    "placed",
    "open",
    "new",
    "NEW",
    "PARTIALLY_FILLED",
];

const REVX_BASE: &str = "https://revx.revolut.com";

/// Seconds a Revolut X last-good read may stand in for a failed one.
pub const REVX_CACHE_MAX_S: f64 = 1800.0;

const REGIME_INPUTS: &str = "BTC vs its 100d and 200d SMA, plus how many coins sit above their own 30d SMA. Trend filter only: no valuation, flows, on-chain, funding or sentiment.";

/// The signal names in the order the regime reading lists them.
const SIGNAL_ORDER: [&str; 5] = [
    "above_sma30",
    "ret30_strong",
    "fresh_30d_high",
    "higher_lows",
    "insufficient_history",
];

fn is_open(status: Option<&Json>) -> bool {
    matches!(status, Some(Json::Str(s)) if OPEN_ST.contains(&s.as_str()))
}

fn f64_json(x: Option<f64>) -> Json {
    x.map(Json::Float).unwrap_or(Json::Null)
}

// ------------------------------------------------------------------ Revolut X

/// `float(x)` where a failure means "no quote" (thin pairs send `""`).
fn loose_float(v: Option<&Json>) -> Option<f64> {
    match v? {
        Json::Null | Json::Arr(_) | Json::Obj(_) => None,
        x => x.to_float(),
    }
}

/// The Revolut X account: balances, resting orders and spot mids. The reading, or the
/// cached one (marked `stale_s`) while it is under 30 minutes old, or `{"error": ...}`.
pub fn revx_read(d: &DashConfig, io: &mut Io) -> Json {
    match revx_live(d, io) {
        Ok(out) => out,
        Err(e) => {
            let now = (io.clock)();
            if let Ok(t) = std::fs::read_to_string(d.revx_cache_path()) {
                if let Ok(c @ Json::Obj(_)) = serde_json::from_str::<Json>(&t) {
                    let ts = c.get("ts").and_then(Json::num).unwrap_or(0.0);
                    if now - ts < REVX_CACHE_MAX_S {
                        if let (Some(Json::Obj(_)), Some(ts)) =
                            (c.get("data"), c.get("ts").and_then(Json::num))
                        {
                            let mut cached = c.get("data").cloned().unwrap_or_default();
                            cached.set("stale_s", Json::Int((now - ts).trunc() as i64));
                            return cached;
                        }
                    }
                }
            }
            obj(vec![("error", e.into())])
        }
    }
}

fn revx_call(io: &mut Io, path: &str) -> Result<Json, String> {
    let mut retried = false;
    loop {
        let ts = ((io.clock)() * 1000.0) as i64;
        let headers = (io.revx_auth)(path, ts)?;
        let h: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        match net::fetch(&io.http, &format!("{REVX_BASE}{path}"), None, &h, 10) {
            Err(NetError::Status(429)) if !retried => {
                retried = true;
                (io.sleep)(2.0);
            }
            Err(e) => return Err(e.typed()),
            Ok(v) => return Ok(v),
        }
    }
}

fn revx_live(d: &DashConfig, io: &mut Io) -> Result<Json, String> {
    let raw = revx_call(io, "/api/1.0/balances")?;
    let mut bals = Json::obj();
    for b in iter_py(&raw)? {
        let total = float_or0(b, "total")?;
        if total > 0.0 {
            let cur = b
                .get("currency")
                .ok_or_else(|| "KeyError: 'currency'".to_string())?;
            let mut row = Json::obj();
            for k in ["available", "reserved", "total"] {
                row.set(k, Json::Float(float_or0(b, k)?));
            }
            bals.set(&key_str(cur), row);
        }
    }
    (io.sleep)(0.5);
    let act = revx_call(io, "/api/1.0/orders/active")?;
    let active = match &act {
        Json::Obj(_) => act.get("data").cloned().unwrap_or(Json::Arr(Vec::new())),
        other => {
            return Err(format!(
                "AttributeError: '{}' object has no attribute 'get'",
                type_name(other)
            ))
        }
    };
    let mut mids: Vec<(String, f64)> = Vec::new();
    let mut eur_usd = None;
    (io.sleep)(0.5);
    // Display-only: a failure leaves whatever was read.
    if let Ok(t) = net::fetch(
        &io.http,
        &format!("{REVX_BASE}/api/1.0/public/tickers"),
        None,
        &[],
        10,
    ) {
        let rows = match &t {
            Json::Obj(_) => t.get("data").cloned().unwrap_or(Json::Arr(Vec::new())),
            _ => Json::Null,
        };
        let mut complete = matches!(rows, Json::Arr(_));
        for r in rows.items() {
            let (bid, ask) = (loose_float(r.get("bid")), loose_float(r.get("ask")));
            if let (Some(b), Some(a)) = (bid, ask) {
                if b != 0.0 && a != 0.0 {
                    let Some(sym) = r.get("symbol") else {
                        complete = false;
                        break;
                    };
                    let k = key_str(sym);
                    let m = (b + a) / 2.0;
                    match mids.iter_mut().find(|(s, _)| *s == k) {
                        Some(slot) => slot.1 = m,
                        None => mids.push((k, m)),
                    }
                }
            }
        }
        if complete {
            eur_usd = mids
                .iter()
                .find(|(s, _)| s == "USDC/EUR")
                .map(|(_, m)| *m)
                .filter(|m| *m != 0.0)
                .map(|m| 1.0 / m);
        }
    }
    let mid = |s: &str| mids.iter().find(|(x, _)| x == s).map(|(_, m)| *m);
    let mut rows: Vec<(String, f64, Json)> = Vec::new();
    for o in active.items() {
        let symbol = o.get("symbol");
        let spot = symbol.and_then(|s| s.as_str()).and_then(mid);
        let price = float_or0(o, "price")?;
        let qty = float_or0(o, "quantity")?;
        let sym = symbol
            .filter(|s| s.truthy())
            .map(key_str)
            .unwrap_or_default();
        let sym = sym.split('/').next().unwrap_or("").to_string();
        let created = o
            .get("created_date")
            .filter(|c| c.truthy())
            .and_then(Json::num);
        let ts = created.map(|c| c / 1000.0).filter(|t| *t != 0.0);
        let below = match spot {
            Some(s) if s != 0.0 && price != 0.0 => Some((1.0 - price / s) * 100.0),
            _ => None,
        };
        rows.push((
            sym.clone(),
            price,
            obj(vec![
                ("sym", sym.into()),
                ("side", o.get("side").cloned().unwrap_or(Json::Null)),
                ("price", price.into()),
                ("qty", qty.into()),
                ("usd", (price * qty).into()),
                ("below_spot_pct", f64_json(below)),
                ("ts", f64_json(ts)),
            ]),
        ));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)));
    let hold_value = py_sum(
        bals.entries()
            .iter()
            .filter(|(c, _)| c != "USD")
            .map(|(c, v)| {
                let total = v.get("total").and_then(Json::num).unwrap_or(0.0);
                match mid(&format!("{c}/USD")) {
                    Some(m) => Num::Float(total * m),
                    None => Num::Float(total * 0.0),
                }
            }),
    );
    let usd = bals.get("USD").cloned().unwrap_or_default();
    let usd_get = |k: &str| usd.get(k).cloned().unwrap_or(Json::Float(0.0));
    let total_value = Num::of(&usd_get("available"))
        .unwrap_or(Num::Float(0.0))
        .plus(Num::of(&usd_get("reserved")).unwrap_or(Num::Float(0.0)))
        .plus(hold_value);
    let out = obj(vec![
        ("balances", bals),
        ("orders", Json::Arr(rows.into_iter().map(|r| r.2).collect())),
        ("eur_usd", f64_json(eur_usd)),
        ("usd_free", usd_get("available")),
        ("usd_reserved", usd_get("reserved")),
        ("hold_value", hold_value.json()),
        ("total_value", total_value.json()),
    ]);
    let cache = obj(vec![
        ("ts", Json::Float((io.clock)())),
        ("data", out.clone()),
    ]);
    let _ = crate::store::write_atomic(&d.revx_cache_path(), &cache.dumps(None));
    Ok(out)
}

/// Iterate a decoded value the way a Python `for` loop does over it, or its error.
fn iter_py(v: &Json) -> Result<Vec<&Json>, String> {
    match v {
        Json::Arr(a) => Ok(a.iter().collect()),
        Json::Obj(_) | Json::Str(_) => {
            Err("AttributeError: 'str' object has no attribute 'get'".to_string())
        }
        other => Err(format!(
            "TypeError: '{}' object is not iterable",
            type_name(other)
        )),
    }
}

// ------------------------------------------------------------------ regime

/// The regime block: the label, the evidence behind it and how long it has held.
pub fn regime_block(cfg: &RunConfig, io: &mut Io, now: f64) -> Json {
    let (reg, hist) = match (io.regime)(now) {
        Ok(x) => x,
        Err(e) => return obj(vec![("error", head(&e, 200).into())]),
    };
    let mut coins = Json::obj();
    let empty = Json::obj();
    let reg_coins = reg.get("coins").filter(|c| c.truthy()).unwrap_or(&empty);
    // The reading lists coins in routing order; keep that order whatever the source.
    let mut order: Vec<&str> = cfg
        .routing
        .iter()
        .map(|(s, _)| s.as_str())
        .filter(|s| reg_coins.contains_key(s))
        .collect();
    for (s, _) in reg_coins.entries() {
        if !order.contains(&s.as_str()) {
            order.push(s);
        }
    }
    for sym in order {
        let c = reg_coins.get(sym).cloned().unwrap_or_default();
        if let Some(err) = c.get("error").filter(|e| e.truthy()) {
            coins.set(sym, obj(vec![("error", head(&err.py_str(), 80).into())]));
            continue;
        }
        let sig = c
            .get("signals")
            .filter(|s| s.truthy())
            .cloned()
            .unwrap_or_default();
        let mut names: Vec<&str> = SIGNAL_ORDER
            .iter()
            .copied()
            .filter(|k| sig.contains_key(k))
            .collect();
        for (k, _) in sig.entries() {
            if !names.contains(&k.as_str()) {
                names.push(k);
            }
        }
        let score = names
            .iter()
            .filter(|k| matches!(sig.get(k), Some(Json::Bool(true))))
            .count();
        let signals = Json::Obj(
            names
                .iter()
                .map(|k| {
                    (
                        k.to_string(),
                        Json::Bool(sig.get(k).is_some_and(Json::truthy)),
                    )
                })
                .collect(),
        );
        coins.set(
            sym,
            obj(vec![
                (
                    "running",
                    Json::Bool(c.get("running").is_some_and(Json::truthy)),
                ),
                ("score", Json::Int(score as i64)),
                ("signals", signals),
            ]),
        );
    }
    let g = |j: &Json, k: &str| j.get(k).cloned().unwrap_or(Json::Null);
    obj(vec![
        ("label", g(&reg, "market")),
        ("btc", g(&reg, "btc")),
        ("breadth", g(&reg, "breadth_above_sma30")),
        ("coins", coins),
        ("held_days", g(&hist, "held_days")),
        ("prev_label", g(&hist, "prev_label")),
        ("confirmed", g(&hist, "confirmed")),
        ("confirm_days", g(&hist, "confirm_days")),
        ("days_covered", g(&hist, "days_covered")),
        ("inputs", REGIME_INPUTS.into()),
    ])
}

// ------------------------------------------------------------------ decisions log

/// Python's `str.splitlines()`.
fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = s.char_indices().collect::<Vec<_>>();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let (pos, c) = b[i];
        let brk = matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if brk {
            out.push(&s[start..pos]);
            let mut next = pos + c.len_utf8();
            if c == '\r' && i + 1 < b.len() && b[i + 1].1 == '\n' {
                next += 1;
                i += 1;
            }
            start = next;
        }
        i += 1;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// The last `n` records of the decision log, oldest first (from its last 256 KiB).
pub fn tail(path: &Path, n: usize) -> Vec<Json> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let from = bytes.len().saturating_sub(256 * 1024);
    let text = String::from_utf8_lossy(&bytes[from..]).into_owned();
    let lines = splitlines(&text);
    let skip = lines.len().saturating_sub(n);
    lines[skip..]
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str::<Json>(l).ok())
        .collect()
}

fn ts_of(r: &Json) -> Result<f64, String> {
    match r.get("ts") {
        None => Ok(0.0),
        Some(v) => py_float(v),
    }
}

/// The newest non-run record per coin within `max_age_s`.
pub fn last_per_coin(
    recs: &[Json],
    now: f64,
    max_age_s: f64,
) -> Result<Vec<(String, Json)>, String> {
    let mut out: Vec<(String, Json)> = Vec::new();
    for r in recs.iter().rev() {
        let sym = match r.get("sym") {
            Some(Json::Str(s)) if !s.is_empty() => s.clone(),
            _ => continue,
        };
        if matches!(r.get("kind"), Some(Json::Str(k)) if k == "run")
            || out.iter().any(|(s, _)| *s == sym)
        {
            continue;
        }
        if now - ts_of(r)? > max_age_s {
            continue;
        }
        out.push((sym, r.clone()));
    }
    Ok(out)
}

/// `{kind: n}` over records at or after `since`, run lines excluded, in first-seen order.
pub fn counts_since(recs: &[Json], since: f64) -> Result<Json, String> {
    let mut c: Vec<(String, i64)> = Vec::new();
    for r in recs {
        if ts_of(r)? >= since && !matches!(r.get("kind"), Some(Json::Str(k)) if k == "run") {
            let k = key_str(r.get("kind").unwrap_or(&Json::Null));
            match c.iter_mut().find(|(x, _)| *x == k) {
                Some(slot) => slot.1 += 1,
                None => c.push((k, 1)),
            }
        }
    }
    Ok(Json::Obj(
        c.into_iter().map(|(k, n)| (k, Json::Int(n))).collect(),
    ))
}

// ------------------------------------------------------------------ the snapshot

fn read_journal(path: &Path) -> Result<Json, String> {
    if !path.exists() {
        return Ok(Json::obj());
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "{} exists but cannot be read ({e}); refusing to run on an empty state",
            path.display()
        )
    })?;
    match serde_json::from_str::<Json>(&text) {
        Ok(j @ Json::Obj(_)) => Ok(j),
        Ok(other) => Err(format!(
            "{} holds {}, expected an object",
            path.display(),
            type_name(&other)
        )),
        Err(e) => Err(format!(
            "{} exists but cannot be read ({e}); refusing to run on an empty state",
            path.display()
        )),
    }
}

fn churn_json(m: &crate::deploy::churn::Churn) -> Json {
    obj(vec![
        ("window_days", Json::Int(m.window_days)),
        ("roll_events", Json::Int(m.roll_events as i64)),
        ("roll_events_24h", Json::Int(m.roll_events_24h as i64)),
        ("rungs_rolled", Json::Int(m.rungs_rolled)),
        ("rungs_placed", Json::Int(m.rungs_placed as i64)),
        ("fills", Json::Int(m.fills as i64)),
        ("cancels_per_fill", f64_json(m.cancels_per_fill)),
        ("rung_lifetime_h_median", f64_json(m.rung_lifetime_h_median)),
        ("rung_lifetime_h_p25", f64_json(m.rung_lifetime_h_p25)),
        ("last_roll_ts", f64_json(m.last_roll_ts)),
        (
            "last_roll_exch",
            m.last_roll_exch
                .clone()
                .map(Json::Str)
                .unwrap_or(Json::Null),
        ),
    ])
}

fn sell_json(c: &sellcheck::SellCheck) -> Json {
    obj(vec![
        ("ok", Json::Bool(c.ok)),
        ("reason", c.reason.as_str().into()),
        ("qty", c.qty.into()),
        ("min_base", f64_json(c.min_base)),
        ("min_quote", f64_json(c.min_quote)),
        ("notional", c.notional.into()),
    ])
}

/// The journal's venue for a row: its own, else revx for a deploy-zone id, else the
/// legacy book before the reroute, else today's routing.
fn venue_of(cfg: &RunConfig, d: &DashConfig, o: &Json) -> Json {
    if let Some(v) = o.get("venue").filter(|v| v.truthy()) {
        return v.clone();
    }
    if matches!(o.get("client_id"), Some(Json::Str(c)) if c.starts_with("depx")) {
        return "revx".into();
    }
    let sym = o.get("sym").and_then(Json::as_str).unwrap_or("");
    let ts = o
        .get("ts")
        .filter(|t| t.truthy())
        .and_then(Json::to_float)
        .unwrap_or(0.0);
    if ts < d.legacy_before_ts {
        return d
            .legacy_venues
            .iter()
            .find(|(s, _)| s == sym)
            .map(|(_, v)| Json::Str(v.clone()))
            .unwrap_or(Json::Null);
    }
    cfg.route(sym)
        .map(|r| Json::Str(r.exch.clone()))
        .unwrap_or(Json::Null)
}

/// `o.get(a) or o.get(b)`.
fn or2(o: &Json, a: &str, b: &str) -> Json {
    match o.get(a).filter(|v| v.truthy()) {
        Some(v) => v.clone(),
        None => o.get(b).cloned().unwrap_or(Json::Null),
    }
}

fn ord_ts(o: &Json) -> Json {
    for k in ["filled_ts", "ts"] {
        if let Some(v) = o.get(k).filter(|v| v.truthy()) {
            return v.clone();
        }
    }
    Json::Int(0)
}

fn sort_key(v: &Json) -> f64 {
    v.to_float().unwrap_or(0.0)
}

/// Build `data.json` and print its log lines. An error means no file was written.
pub fn run(cfg: &RunConfig, d: &DashConfig, io: &mut Io) -> Result<(), String> {
    let mut errors: Vec<String> = Vec::new();
    let state = match read_json_or(&cfg.ladder_path(), Json::obj()) {
        j @ Json::Obj(_) => j,
        _ => Json::obj(),
    };
    let journal = read_journal(&cfg.journal_path())?;
    let now = (io.clock)();

    // Venue balances: Gate free and locked. A client that cannot be built leaves every
    // coin without a sell-path check, as one that cannot be read leaves zeros.
    let mut gate_full: BTreeMap<String, crate::venue::Balance> = BTreeMap::new();
    let clients = match (io.venues.venue("gate"), io.venues.venue("revx")) {
        (Ok(g), Ok(r)) => {
            match g.balances_full() {
                Ok(b) => gate_full = b,
                Err(e) => errors.push(format!("balances: {e}")),
            }
            Some((g, r))
        }
        (Err(e), _) | (_, Err(e)) => {
            errors.push(format!("balances: {e}"));
            None
        }
    };
    let client = |exch: &str| match (exch, clients) {
        ("gate", Some((g, _))) => Some(g),
        ("revx", Some((_, r))) => Some(r),
        _ => None,
    };

    let prices = match crate::run::fetch_prices(cfg, io.market, io.sleep) {
        Ok(p) => p,
        Err(e) => {
            errors.push(format!("prices: {e}"));
            BTreeMap::new()
        }
    };

    let revx = revx_read(d, io);
    if let Some(e) = revx.get("error").filter(|e| e.truthy()) {
        errors.push(format!("revx: {}", e.py_str()));
    } else if let Some(Json::Int(s)) = revx.get("stale_s").filter(|s| s.truthy()) {
        errors.push(format!(
            "revx: rate-limited, serving {}m-old cache",
            rungbot_core::watch::pyfmt::floordiv(*s, 60)
        ));
    }
    let revx_err = revx.get("error").filter(|e| e.truthy()).cloned();
    let rx_bal = if revx_err.is_some() {
        Json::obj()
    } else {
        revx.get("balances")
            .filter(|b| b.truthy())
            .cloned()
            .unwrap_or_default()
    };
    let rx_order_syms: Vec<Json> = if revx_err.is_some() {
        Vec::new()
    } else {
        revx.get("orders")
            .map(|o| {
                o.items()
                    .iter()
                    .map(|x| x.get("sym").cloned().unwrap_or(Json::Null))
                    .collect()
            })
            .unwrap_or_default()
    };
    let rx = |c: &str, k: &str| -> Result<f64, String> {
        float_or0(rx_bal.get(c).unwrap_or(&Json::obj()), k)
    };

    let empty = Json::obj();
    let mut coins: Vec<(Option<f64>, Json, String)> = Vec::new();
    let (mut total_value, mut unrealized) = (0.0f64, 0.0f64);
    for (sym, _) in &cfg.watchlist {
        let route = cfg.route(sym);
        let (exch, pair, quote) = match route {
            Some(r) => (
                Json::Str(r.exch.clone()),
                Json::Str(r.pair.clone()),
                Json::Str(r.quote.clone()),
            ),
            None => (Json::Null, Json::Null, Json::Null),
        };
        let (price, chg) = match prices.get(sym) {
            Some((p, c)) => (Some(*p), Some(*c)),
            None => (None, None),
        };
        let st = match state.get(sym) {
            Some(s @ Json::Obj(_)) => s,
            _ => &empty,
        };
        let entry = match st.get("cost_basis").filter(|c| c.truthy()) {
            Some(c) => c.clone(),
            None => cfg
                .entries
                .get(sym)
                .map(|e| Json::Float(*e))
                .unwrap_or(Json::Null),
        };
        let entry_f = entry.truthy().then(|| entry.to_float()).flatten();
        let gate_amt = gate_full.get(sym).map_or(0.0, |b| b.free);
        let revx_amt = rx(sym, "available")?;
        let held = pysum([gate_amt, revx_amt]);
        let price_t = price.filter(|p| *p != 0.0);
        let value = match price_t {
            Some(p) if held != 0.0 => held * p,
            _ => 0.0,
        };
        let pnl = match (price_t, entry_f) {
            (Some(p), Some(e)) => Some((p / e - 1.0) * 100.0),
            _ => None,
        };
        let sell_tgt = entry_f.map(|e| e * (1.0 + cfg.target_pct / 100.0));
        let win_until = st.get("win_until").and_then(Json::to_float).unwrap_or(0.0);
        total_value += value;
        if let (Some(p), Some(e)) = (price_t, entry_f) {
            if held != 0.0 {
                unrealized += held * (p - e);
            }
        }
        let exch_s = route.map(|r| r.exch.as_str());
        let mut venues = vec![exch.clone()];
        for (v, amt) in [("gate", gate_amt), ("revx", revx_amt)] {
            if Some(v) == exch_s {
                continue;
            }
            let quoting = v == "revx"
                && rx_order_syms
                    .iter()
                    .any(|s| s.as_str() == Some(sym.as_str()));
            if amt > 0.0 || quoting {
                venues.push(v.into());
            }
        }
        let to_sell = match (price_t, sell_tgt.filter(|t| *t != 0.0)) {
            (Some(p), Some(t)) => Some((p / t - 1.0) * 100.0),
            _ => None,
        };
        let sell_ready = match exch_s.and_then(client) {
            Some(v) => sell_json(&sellcheck::check(
                v,
                route.map(|r| r.pair.as_str()).unwrap_or(""),
                Some(held),
                price,
            )),
            None => obj(vec![
                ("ok", Json::Bool(false)),
                (
                    "reason",
                    format!("no {} client", exch_s.unwrap_or("None")).into(),
                ),
            ]),
        };
        let pass = |k: &str, dflt: Json| st.get(k).cloned().unwrap_or(dflt);
        let coin = obj(vec![
            ("sym", sym.as_str().into()),
            ("exch", exch),
            ("pair", pair),
            ("quote", quote),
            ("venues", Json::Arr(venues)),
            ("held", held.into()),
            ("price", f64_json(price)),
            ("chg24h", f64_json(chg)),
            ("value", value.into()),
            ("entry", entry),
            ("pnl", f64_json(pnl)),
            ("sell_target", f64_json(sell_tgt)),
            ("to_sell_pct", f64_json(to_sell)),
            ("to_buy_pct", f64_json(chg.map(|c| c + cfg.first_pct))),
            (
                "win_dir",
                if now < win_until {
                    pass("win_dir", "".into())
                } else {
                    "".into()
                },
            ),
            ("deployed_pct", pass("deployed_pct", Json::Float(0.0))),
            ("sold_pct", pass("sold_pct", Json::Float(0.0))),
            ("sell_ready", sell_ready),
        ]);
        coins.push((to_sell, coin, sym.clone()));
    }
    coins.sort_by(|a, b| {
        let k = |x: &Option<f64>| x.unwrap_or(-999.0);
        k(&b.0).total_cmp(&k(&a.0))
    });

    // Ladder churn: one CHURN line per cycle; the card reads the block.
    let churn_block = match crate::store::load_journal(&cfg.journal_path()) {
        Ok(j) => {
            let rows: Vec<&crate::journal::Order> = j.orders.values().collect();
            let m7 = crate::deploy::churn::metrics(&rows, now, 7);
            let m28 = crate::deploy::churn::metrics(&rows, now, 28);
            let _ = writeln!(io.out, "{}", crate::deploy::churn::summary(&m7));
            let mut b = churn_json(&m7);
            b.set("d28", churn_json(&m28));
            b
        }
        Err(e) => {
            errors.push(format!("churn: {e}"));
            Json::obj()
        }
    };

    // One SELL-CHECK line per cycle.
    let mut checks: BTreeMap<String, (bool, String)> = BTreeMap::new();
    for (_, c, sym) in &coins {
        let r = c.get("sell_ready").cloned().unwrap_or_default();
        checks.insert(
            sym.clone(),
            (
                r.get("ok").is_some_and(Json::truthy),
                r.get("reason")
                    .map(Json::py_str)
                    .unwrap_or_else(|| "None".into()),
            ),
        );
    }
    let ok_n = checks.values().filter(|(ok, _)| *ok).count();
    let bad: Vec<String> = checks
        .iter()
        .filter(|(_, (ok, _))| !ok)
        .map(|(s, (_, r))| format!("{s} {r}"))
        .collect();
    let mut line = format!("SELL-CHECK {ok_n}/{} sellable", checks.len());
    if !bad.is_empty() {
        line.push_str("; ");
        line.push_str(&bad.join("; "));
    }
    let _ = writeln!(io.out, "{line}");

    // Last decision per coin (3-day window) and today's counts.
    let rows: Vec<&Json> = journal.entries().iter().map(|(_, o)| o).collect();
    let recs = tail(&cfg.decisions_path(), 600);
    let decisions_block = (|| -> Result<Json, String> {
        let last = last_per_coin(&recs, now, 3.0 * 86_400.0)?;
        for (_, c, sym) in coins.iter_mut() {
            let rec = match last.iter().find(|(s, _)| s == sym) {
                None => Json::Null,
                Some((_, r)) => {
                    let f = |k: &str| r.get(k).cloned().ok_or_else(|| format!("'{k}'"));
                    obj(vec![
                        ("kind", f("kind")?),
                        ("text", f("text")?),
                        ("ts", f("ts")?),
                        ("src", f("src")?),
                    ])
                }
            };
            c.set("decision", rec);
        }
        let midnight = now - now.rem_euclid(86_400.0);
        let errored = rows
            .iter()
            .filter(|o| matches!(o.get("status"), Some(Json::Str(s)) if s == "error"))
            .count();
        let poll = rows
            .iter()
            .filter(|o| is_open(o.get("status")) && o.get("last_error").is_some_and(Json::truthy))
            .count();
        Ok(obj(vec![
            ("today", counts_since(&recs, midnight)?),
            ("journal_errors", Json::Int(errored as i64)),
            ("poll_errors", Json::Int(poll as i64)),
        ]))
    })()
    .unwrap_or_else(|e| {
        errors.push(format!("decisions: {e}"));
        obj(vec![
            ("today", Json::obj()),
            ("journal_errors", Json::Null),
            ("poll_errors", Json::Null),
        ])
    });

    // Stables: Gate USDT free + locked, Revolut X USD + USDC (+ EUR at the mid).
    let bal = |a: &str| gate_full.get(a).copied().unwrap_or_default();
    let eur_usd = revx
        .get("eur_usd")
        .filter(|v| v.truthy())
        .and_then(Json::num)
        .unwrap_or(1.0);
    let mut rx_stable = pysum([rx("USD", "total")?, rx("USDC", "total")?]);
    rx_stable += rx("EUR", "total")? * eur_usd;
    let rx_locked = pysum([rx("USD", "reserved")?, rx("USDC", "reserved")?]);
    let stable = (bal("USDT").free + bal("USDT").locked) + rx_stable;
    let stable_locked = bal("USDT").locked + rx_locked;
    let mut sells = Vec::new();
    for o in &rows {
        let is = |k: &str, v: &str| matches!(o.get(k), Some(Json::Str(s)) if s == v);
        if is("side", "sell") && is("status", "filled") {
            sells.push(Num::Float(float_or0(o, "filled_quote")?));
        }
    }
    let realized = py_sum(sells);

    // The onramp top-up card: hidden when the onramp venue could not be read.
    let gate_stable = pysum(["USDT", "USDC"].map(|a| bal(a).free + bal(a).locked));
    let funding = match &revx_err {
        Some(e) => obj(vec![("error", e.clone())]),
        None => {
            let alloc: Vec<(String, f64)> = cfg
                .deploy_alloc
                .iter()
                .map(|(s, w)| (s.to_uppercase(), *w))
                .collect();
            let routing: Vec<(String, String)> = cfg
                .routing
                .iter()
                .map(|(s, r)| (s.clone(), r.exch.clone()))
                .collect();
            let usdc_free = rx("USDC", "total")? - rx("USDC", "reserved")?;
            let c = crate::run::funding::card(rx_stable, gate_stable, usdc_free, &alloc, &routing);
            obj(vec![
                ("gate_stable", c.gate_stable.into()),
                ("gate_target", c.gate_target.into()),
                ("gate_share_pct", c.gate_share_pct.into()),
                ("gate_gap", c.gate_gap.into()),
                ("revx_stable", c.revx_stable.into()),
                ("deposit_headroom", c.deposit_headroom.into()),
                ("reserved_on_revx", c.reserved_on_revx.into()),
                ("revx_sendable", c.revx_sendable.into()),
                ("in_flight", Json::Bool(c.in_flight)),
                ("due", Json::Bool(c.due)),
                ("send_now", c.send_now.into()),
                ("revx_usdc_free", c.revx_usdc_free.into()),
            ])
        }
    };

    // Order log: every resting order and the newest 60 fills, newest first.
    let mut ordered: Vec<&Json> = rows.clone();
    ordered.sort_by(|a, b| sort_key(&ord_ts(b)).total_cmp(&sort_key(&ord_ts(a))));
    let row = |o: &Json| -> Json {
        let ts = ord_ts(o);
        obj(vec![
            (
                "client_id",
                o.get("client_id").cloned().unwrap_or(Json::Null),
            ),
            ("sym", o.get("sym").cloned().unwrap_or(Json::Null)),
            ("side", o.get("side").cloned().unwrap_or(Json::Null)),
            ("kind", o.get("kind").cloned().unwrap_or(Json::Null)),
            ("status", o.get("status").cloned().unwrap_or(Json::Null)),
            ("venue", venue_of(cfg, d, o)),
            ("base", or2(o, "filled_base", "base")),
            ("quote", or2(o, "filled_quote", "quote")),
            ("price", or2(o, "avg_price", "price")),
            ("ts", if ts.truthy() { ts } else { Json::Null }),
        ])
    };
    let mut filled: Vec<Json> = ordered
        .iter()
        .filter(|o| matches!(o.get("status"), Some(Json::Str(s)) if s == "filled"))
        .map(|o| row(o))
        .collect();
    if d.manual_fills.exists() {
        match manual_fills(&d.manual_fills) {
            Ok(extra) => {
                filled.extend(extra);
                let k = |r: &Json| {
                    sort_key(r.get("ts").filter(|t| t.truthy()).unwrap_or(&Json::Int(0)))
                };
                filled.sort_by(|a, b| k(b).total_cmp(&k(a)));
            }
            Err(e) => errors.push(format!("manual-fills: {e}")),
        }
    }
    let mut log: Vec<Json> = ordered
        .iter()
        .filter(|o| is_open(o.get("status")))
        .map(|o| row(o))
        .collect();
    log.extend(filled.into_iter().take(60));

    let daily = match state.get("_daily") {
        Some(j @ Json::Obj(_)) => j.clone(),
        _ => Json::obj(),
    };
    let last_run = d
        .run_log
        .as_deref()
        .and_then(mtime)
        .map(|t| Json::Str(rungbot_core::iso8601_micros(t)))
        .unwrap_or(Json::Null);
    let n_errors = errors.len();
    let health = obj(vec![
        ("mode", cfg.trade_mode.as_str().into()),
        (
            "live_enabled",
            if cfg.live_trading_enabled {
                "yes"
            } else {
                "no"
            }
            .into(),
        ),
        ("halt", Json::Bool(cfg.halt_file.exists())),
        ("last_run", last_run),
        ("daily", daily),
        (
            "errors",
            Json::Arr(errors.into_iter().map(Json::Str).collect()),
        ),
        (
            "deploy",
            read_json_or(
                &d.deploy_status_path(),
                obj(vec![
                    ("last_ok_ts", Json::Null),
                    ("consecutive_failures", Json::Null),
                ]),
            ),
        ),
        (
            "audit",
            read_json_or(
                &cfg.audit_state_path(),
                obj(vec![
                    ("ts", Json::Null),
                    ("ok", Json::Null),
                    ("findings", Json::Arr(Vec::new())),
                ]),
            ),
        ),
    ]);
    let n_coins = coins.len();
    let snapshot = obj(vec![
        (
            "generated",
            rungbot_core::iso8601_micros((io.clock)()).into(),
        ),
        ("health", health),
        (
            "portfolio",
            obj(vec![
                ("total_value", total_value.into()),
                ("stable_bag", stable.into()),
                ("stable_locked", stable_locked.into()),
                ("unrealized_pnl", unrealized.into()),
                ("realized_proceeds", realized.json()),
            ]),
        ),
        ("coins", Json::Arr(coins.into_iter().map(|c| c.1).collect())),
        ("order_log", Json::Arr(log)),
        ("funding", funding),
        ("regime", regime_block(cfg, io, now)),
        ("decisions", decisions_block),
        ("churn", churn_block),
    ]);
    let out = d.data_path();
    crate::store::write_atomic(&out, &snapshot.dumps(Some(2)))?;
    let _ = writeln!(
        io.out,
        "wrote {} | {n_coins} coins | value ${} | stable ${} (locked ${}) | errors {n_errors}",
        out.display(),
        fixed(total_value, 2),
        fixed(stable, 2),
        fixed(stable_locked, 2)
    );
    Ok(())
}

/// The filled rows of the manual-fills file, pre-shaped order-log rows.
fn manual_fills(path: &Path) -> Result<Vec<Json>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: Json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let items: Vec<Json> = match v {
        Json::Arr(a) => a,
        Json::Obj(o) => o.into_iter().map(|(k, _)| Json::Str(k)).collect(),
        Json::Str(s) => s.chars().map(|c| Json::Str(c.to_string())).collect(),
        other => return Err(format!("'{}' object is not iterable", type_name(&other))),
    };
    let mut out = Vec::new();
    for r in items {
        if !r.is_obj() {
            return Err(format!("'{}' object has no attribute 'get'", type_name(&r)));
        }
        if matches!(r.get("status"), Some(Json::Str(s)) if s == "filled") {
            out.push(r);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(splitlines("a\nb\r\nc\rd"), vec!["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\n"), vec!["a"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
    }
}
