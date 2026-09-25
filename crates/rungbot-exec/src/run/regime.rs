//! The market regime a run steers by, cached on disk, and the sell-policy mode it
//! implies.
//!
//! * [`get_regime`]: the label (`bull`/`chop`/`bear`/`unknown`), BTC against its long
//!   averages, breadth, and each coin's RUN gate. Cached in `regime-state.json` for
//!   `regime_ttl_h` hours; computed from public daily closes otherwise.
//! * [`label_history`]: the label replayed day by day, so "has this label held long
//!   enough to count" has an answer. Cached in `regime-history.json` for
//!   `regime_hist_ttl_h` hours; a failed rebuild serves the previous history as stale.
//! * [`PolicyMode`]: whether the bull sell policy governs sells this run, and which
//!   coins an external signal arms.
//!
//! The arithmetic is [`rungbot_core::regime`]; this module owns the fetching, the
//! caches and their file shapes.

use std::path::Path;

use rungbot_core::regime::{market_label, running_from_series, sma, RegimeConfig};
use serde_json::{json, Map, Value};

use super::config::RunConfig;
use super::market::Market;
use crate::pyfmt;

/// Daily candles the live reading needs: enough for the 200-day average.
pub const KLINE_DAYS: usize = 220;

/// The only coin an external arm (froth read, manual file) applies to by default.
pub const ARM_SYM: &str = "BTC";

/// A froth read older than this does not arm.
pub const ARM_STALE_S: f64 = 3.0 * 86_400.0;

fn round_to(x: f64, digits: usize) -> f64 {
    format!("{x:.digits$}").parse().unwrap_or(x)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_atomic(path: &Path, body: &str, what: &str) {
    if let Err(e) = crate::store::write_atomic(path, body) {
        eprintln!("WARN: could not {what}: {e}");
    }
}

/// Daily closes for a routed pair. Revolut X pairs read a deeper venue's candles, per
/// `regime_kline_source`.
pub fn closes_for_route(
    cfg: &RunConfig,
    m: &dyn Market,
    exch: &str,
    pair: &str,
    days: usize,
) -> Result<Vec<f64>, String> {
    let (exch, pair) = if exch == "revx" {
        match cfg.regime_kline_source.get(pair) {
            Some((e, p)) => (e.as_str(), p.as_str()),
            None => return Err(format!("no kline source for revx pair {pair}")),
        }
    } else {
        (exch, pair)
    };
    m.closes(exch, pair, days)
}

fn regime_cfg(cfg: &RunConfig) -> RegimeConfig {
    RegimeConfig {
        run_min_signals: cfg.run_min_signals,
        run_ret30_min: cfg.run_ret30_min,
    }
}

/// Read the market now: every routed coin's RUN gate, breadth, and the label.
pub fn compute(cfg: &RunConfig, m: &dyn Market, now: f64) -> Value {
    let mut coins = Map::new();
    let mut above30 = 0usize;
    for (sym, r) in &cfg.routing {
        match closes_for_route(cfg, m, &r.exch, &r.pair, KLINE_DAYS) {
            Ok(closes) if !closes.is_empty() => {
                let (running, sig) = running_from_series(&closes, 1, regime_cfg(cfg));
                let signals = if sig.insufficient_history {
                    json!({"insufficient_history": true})
                } else {
                    json!({"above_sma30": sig.above_sma30, "ret30_strong": sig.ret30_strong,
                           "fresh_30d_high": sig.fresh_30d_high, "higher_lows": sig.higher_lows})
                };
                let last = closes[closes.len() - 1];
                let sma30 = (closes.len() >= 30)
                    .then(|| sma(&closes[closes.len() - 30..]).map(|v| round_to(v, 8)))
                    .flatten();
                if closes.len() >= 30 && sma(&closes[closes.len() - 30..]).is_some_and(|s| last > s)
                {
                    above30 += 1;
                }
                coins.insert(
                    sym.clone(),
                    json!({"running": running, "signals": signals, "px": last, "sma30": sma30}),
                );
            }
            Ok(_) => {
                coins.insert(
                    sym.clone(),
                    json!({"running": false, "error": "list index out of range"}),
                );
            }
            Err(e) => {
                coins.insert(sym.clone(), json!({"running": false, "error": e}));
            }
        }
    }
    let n = coins.len();
    let (label, btc) = match m.closes("binance", &cfg.regime_market_symbol, KLINE_DAYS) {
        Ok(b) if !b.is_empty() => {
            let k = b.len();
            (
                market_label(&b, above30, n).as_str().to_string(),
                json!({"px": b[k - 1],
                       "sma100": round_to(sma(&b[k.saturating_sub(100)..]).unwrap_or(0.0), 2),
                       "sma200": round_to(sma(&b[k.saturating_sub(200)..]).unwrap_or(0.0), 2)}),
            )
        }
        Ok(_) => (
            "unknown".into(),
            json!({"error": "list index out of range"}),
        ),
        Err(e) => ("unknown".into(), json!({ "error": e })),
    };
    json!({
        "epoch": now.floor() as i64,
        "market": label,
        "btc": btc,
        "breadth_above_sma30": format!("{above30}/{n}"),
        "coins": Value::Object(coins),
    })
}

/// The cached regime, or a fresh reading written to the cache. Never fails: a reading
/// whose feeds all failed says `unknown`.
pub fn get_regime(cfg: &RunConfig, m: &dyn Market, now: f64) -> Value {
    let path = cfg.regime_path();
    if let Some(cached) = read_json(&path) {
        let epoch = cached.get("epoch").and_then(Value::as_f64).unwrap_or(0.0);
        if now - epoch < cfg.regime_ttl_h * 3600.0 {
            return cached;
        }
    }
    let reg = compute(cfg, m, now);
    write_atomic(&path, &pyfmt::dumps(&reg, Some(2)), "cache regime state");
    reg
}

/// The label replayed over the last `regime_hist_days`, with how long it has held.
pub fn label_history(cfg: &RunConfig, m: &dyn Market, now: f64) -> Value {
    let path = cfg.regime_history_path();
    if let Some(cached) = read_json(&path) {
        let epoch = cached.get("epoch").and_then(Value::as_f64).unwrap_or(0.0);
        if now - epoch < cfg.regime_hist_ttl_h * 3600.0 {
            return cached;
        }
    }
    let days = cfg.regime_hist_days;
    let mut series: Vec<Vec<f64>> = Vec::new();
    for (_, r) in &cfg.routing {
        if let Ok(c) = closes_for_route(cfg, m, &r.exch, &r.pair, days) {
            series.push(c);
        }
    }
    let n = series.iter().map(Vec::len).min().unwrap_or(0);
    let series: Vec<&[f64]> = series.iter().map(|v| &v[v.len() - n..]).collect();
    let btc_all: Vec<f64> = if n > 0 {
        match m.closes("binance", &cfg.regime_market_symbol, days) {
            Ok(b) => b[b.len().saturating_sub(n)..].to_vec(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let mut labels: Vec<String> = Vec::new();
    if n > 200 && btc_all.len() == n {
        for i in 200..=n {
            let breadth = series
                .iter()
                .filter(|c| i >= 30 && sma(&c[i - 30..i]).is_some_and(|s| c[i - 1] > s))
                .count();
            labels.push(
                market_label(&btc_all[..i], breadth, series.len())
                    .as_str()
                    .to_string(),
            );
        }
    }
    let prior = read_json(&path);
    if labels.is_empty() {
        // Fetch failed or too thin: serve the previous history stale rather than pin an
        // `unknown` label (the sell policy would fail over to the ladder).
        if let Some(Value::Object(mut p)) = prior.clone() {
            if p.get("labels")
                .and_then(Value::as_array)
                .is_some_and(|l| !l.is_empty())
            {
                p.insert("stale".into(), json!(true));
                return Value::Object(p);
            }
        }
    }
    let last = labels.last().cloned();
    let held = match &last {
        Some(l) => labels.iter().rev().take_while(|x| *x == l).count(),
        None => 0,
    };
    let prev = last
        .as_ref()
        .and_then(|l| labels.iter().rev().find(|x| *x != l).cloned());
    let notified = prior
        .as_ref()
        .and_then(|p| p.get("notified").cloned())
        .unwrap_or_else(|| json!({}));
    let out = json!({
        "epoch": now.floor() as i64,
        "labels": labels,
        "label": last.unwrap_or_else(|| "unknown".into()),
        "held_days": held,
        "prev_label": prev,
        "confirmed": held as i64 >= cfg.regime_confirm_days,
        "confirm_days": cfg.regime_confirm_days,
        "days_covered": labels.len(),
        "notified": notified,
    });
    write_atomic(&path, &pyfmt::dumps(&out, None), "write regime history");
    out
}

/// The sell-policy mode for one run.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyMode {
    /// `auto`, `ladder` or `bull`.
    pub mode: String,
    /// The history's label, `""` when it could not be read.
    pub label: String,
    pub confirmed: bool,
    pub prev_label: Option<String>,
    /// The mode the last run persisted: kept on an unknown label.
    pub fallback: Option<bool>,
}

impl PolicyMode {
    pub fn new(mode: &str, history: Option<&Value>) -> PolicyMode {
        let h = history.cloned().unwrap_or(Value::Null);
        PolicyMode {
            mode: mode.to_string(),
            label: h
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            confirmed: pyfmt::truthy(h.get("confirmed")),
            prev_label: h
                .get("prev_label")
                .and_then(Value::as_str)
                .map(str::to_string),
            fallback: None,
        }
    }

    /// Does the bull policy govern sells right now?
    pub fn active(&self) -> bool {
        let fb = self.fallback.unwrap_or(false);
        match self.mode.as_str() {
            "ladder" => false,
            "bull" => true,
            _ => match self.label.as_str() {
                "" | "unknown" => fb,
                "bull" => self.confirmed || fb,
                // Hysteresis on the way out: stay until the flip is confirmed.
                _ => fb && self.prev_label.as_deref() == Some("bull") && !self.confirmed,
            },
        }
    }
}

/// The external arm for BTC from the froth read: `(armed, the read)`. A read older than
/// three days does not arm; the latch (`armed_until`) keeps it armed after the flag drops.
pub fn btc_armed(froth_path: &Path, now: f64) -> (bool, Map<String, Value>) {
    let Some(st) = read_json(froth_path) else {
        return (false, Map::new());
    };
    let arm = match st.get("btc_arm") {
        Some(Value::Object(o)) if !o.is_empty() => o.clone(),
        _ => Map::new(),
    };
    let num = |k: &str| -> Option<f64> {
        match arm.get(k) {
            None | Some(Value::Null) => Some(0.0),
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::Bool(b)) => Some(*b as i64 as f64),
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        }
    };
    let Some(ts) = num("ts") else {
        return (false, Map::new());
    };
    if now - ts > ARM_STALE_S {
        return (false, arm);
    }
    let until = match arm.get("armed_until") {
        Some(v) if pyfmt::truthy(Some(v)) => num("armed_until"),
        _ => Some(0.0),
    };
    let Some(until) = until else {
        return (false, Map::new());
    };
    (pyfmt::truthy(arm.get("armed")) || now < until, arm)
}

/// Is `sym`'s exit armed this run? The manual file arms the arm symbol (every coin with
/// `sell_arm_all`); the froth read arms the arm symbol.
pub fn armed_for(cfg: &RunConfig, sym: &str, now: f64) -> bool {
    if cfg.sell_arm_file.exists() && (cfg.sell_arm_all || sym == ARM_SYM) {
        return true;
    }
    sym == ARM_SYM && btc_armed(&cfg.froth_path(), now).0
}
