//! Fill odds of the resting buy rungs: how much of the stable cash is realistically in
//! play.
//!
//! For every open limit buy in the journal: its gap below today's spot, and how often a
//! daily low reached that gap within H days in the coin's own cached history. Two
//! samples per rung: every start day with a full window ahead ("all"), and start days in
//! the same state as today ("now": the coin's RUN gate as today and BTC above its 100-
//! and 200-day averages). Expected fill = remaining notional × like-now odds.
//!
//! Read-only: nothing is fetched, placed or cancelled. Overlapping windows make the odds
//! a frequency over history, not independent trials; `n` is the number of start days.
//!
//! One correction to the reference: a partly filled rung counts only its unfilled part
//! (the reference counted it at full size until it filled completely).

use std::collections::BTreeSet;
use std::path::Path;

use indexmap::IndexMap;
use rungbot_core::regime::{running_from_series, sma, RegimeConfig};
use rungbot_core::watch::pyfmt::{comma, sum as py_sum};
use serde::Serialize;
use serde_json::{json, Value};

use crate::journal::Journal;

/// Cache files per coin; the longest series wins.
pub const SOURCES: [&str; 2] = ["binance_{s}USDT.json", "gate_{s}_USDT.json"];

/// One day: (date, high, low, close).
pub type Day = (Value, f64, f64, f64);

fn fnum(v: Option<&Value>) -> Result<f64, String> {
    crate::pyfmt::float_or_zero(v).and_then(|x| {
        if v.is_some_and(|v| !v.is_null()) {
            Ok(x)
        } else {
            Err("missing column".into())
        }
    })
}

/// The longest cached series for `sym`, ascending, or empty.
pub fn load_series(cache: &Path, sym: &str) -> Result<Vec<Day>, String> {
    let mut best: Vec<Day> = Vec::new();
    for pat in SOURCES {
        let p = cache.join(pat.replace("{s}", sym));
        if !p.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let rows: Vec<Vec<Value>> =
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", p.display()))?;
        if rows.len() > best.len() {
            best = rows
                .iter()
                .map(|r| {
                    Ok((
                        r.first().cloned().unwrap_or(Value::Null),
                        fnum(r.get(2))?,
                        fnum(r.get(3))?,
                        fnum(r.get(4))?,
                    ))
                })
                .collect::<Result<_, String>>()?;
        }
    }
    Ok(best)
}

fn date_key(v: &Value) -> String {
    v.to_string()
}

/// Dates where BTC closed above both its 100- and 200-day averages.
pub fn btc_bull_days(btc: &[Day]) -> BTreeSet<String> {
    let closes: Vec<f64> = btc.iter().map(|r| r.3).collect();
    let mut out = BTreeSet::new();
    for i in 199..closes.len() {
        let px = closes[i];
        let a = sma(&closes[i - 99..=i]).unwrap_or(f64::INFINITY);
        let b = sma(&closes[i - 199..=i]).unwrap_or(f64::INFINITY);
        if px > a && px > b {
            out.insert(date_key(&btc[i].0));
        }
    }
    out
}

/// A filter on start days: `(index, closes)`.
pub type Keep<'a> = &'a dyn Fn(usize, &[f64]) -> bool;

/// `(hits, n)`: start days whose next `horizon` daily lows reached close × (1 + gap).
pub fn odds(series: &[Day], gap: f64, horizon: usize, keep: Option<Keep>) -> (usize, usize) {
    let closes: Vec<f64> = series.iter().map(|r| r.3).collect();
    let (mut hits, mut n) = (0, 0);
    let end = series.len().saturating_sub(horizon);
    for i in 30..end.max(30) {
        if let Some(k) = keep {
            if !k(i, &closes) {
                continue;
            }
        }
        let target = closes[i] * (1.0 + gap);
        n += 1;
        let low = series[i + 1..i + 1 + horizon]
            .iter()
            .map(|r| r.2)
            .fold(f64::INFINITY, f64::min);
        if low <= target {
            hits += 1;
        }
    }
    (hits, n)
}

/// An open buy rung: what is left of it, at its price.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenBuy {
    pub sym: String,
    pub rung: Option<i64>,
    pub price: f64,
    pub usd: f64,
    pub ts: Option<f64>,
}

/// Every open buy with a price and something left to fill, by coin then highest price.
pub fn open_buys(j: &Journal) -> Vec<OpenBuy> {
    let mut rows: Vec<OpenBuy> = j
        .open_orders(None)
        .into_iter()
        .filter(|o| o.side == "buy" && o.price.is_some_and(|p| p != 0.0))
        .filter_map(|o| {
            let price = o.price.unwrap_or(0.0);
            let left = o.base.unwrap_or(0.0) - o.booked_base();
            (left > 0.0).then(|| OpenBuy {
                sym: o.sym.clone(),
                rung: o.rung,
                price,
                usd: left * price,
                ts: o.ts,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a.sym.cmp(&b.sym).then(
            (-a.price)
                .partial_cmp(&-b.price)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    rows
}

/// One horizon's odds for a rung.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Odds {
    pub all: Option<f64>,
    pub all_n: usize,
    pub now: Option<f64>,
    pub now_n: usize,
}

/// One rung in the table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub sym: String,
    pub rung: Option<i64>,
    pub price: f64,
    pub usd: f64,
    pub ts: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spot: Option<f64>,
    /// Below spot as a fraction (negative); `None` without spot or history.
    pub gap: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<bool>,
    /// By horizon in days.
    pub odds: IndexMap<String, Odds>,
}

/// The whole table, in the reference's field order (`--json`, the dashboard).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FillOdds {
    pub horizons: Vec<usize>,
    pub rows: Vec<Row>,
    pub total_usd: f64,
    pub expected_fill_usd: f64,
    pub low_odds_usd: f64,
    pub low_odds: f64,
    pub market: Value,
    pub held_days: Value,
    pub confirmed: Value,
    pub cache_last: IndexMap<String, Value>,
}

/// The odds of every open buy rung, from the regime reading (`coins.{SYM}.px`,
/// `running`, `market`), the label history (`held_days`, `confirmed`) and the candle
/// cache.
pub fn compute(
    j: &Journal,
    regime: &Value,
    history: Option<&Value>,
    cache: &Path,
    horizons: &[usize],
    low_odds: f64,
    rcfg: RegimeConfig,
) -> Result<FillOdds, String> {
    let coins = regime.get("coins").and_then(Value::as_object);
    let spot = |s: &str| -> Option<f64> {
        coins
            .and_then(|c| c.get(s))
            .and_then(|c| c.get("px"))
            .and_then(Value::as_f64)
            .filter(|x| *x != 0.0)
    };
    let running = |s: &str| -> bool {
        coins
            .and_then(|c| c.get(s))
            .and_then(|c| c.get("running"))
            .is_some_and(|v| crate::pyfmt::truthy(Some(v)))
    };
    let btc = load_series(cache, "BTC")?;
    let bull = btc_bull_days(&btc);
    let mut series: IndexMap<String, Vec<Day>> = IndexMap::new();
    let mut rows: Vec<Row> = Vec::new();
    let hmax = horizons.iter().copied().max().unwrap_or(0);
    for r in open_buys(j) {
        if !series.contains_key(&r.sym) {
            let s = load_series(cache, &r.sym)?;
            series.insert(r.sym.clone(), s);
        }
        let s = &series[&r.sym];
        let mut row = Row {
            sym: r.sym.clone(),
            rung: r.rung,
            price: r.price,
            usd: r.usd,
            ts: r.ts,
            spot: None,
            gap: None,
            running: None,
            odds: IndexMap::new(),
        };
        let (Some(px), false) = (spot(&r.sym), s.is_empty()) else {
            rows.push(row);
            continue;
        };
        let gap = r.price / px - 1.0;
        let want = running(&r.sym);
        let like_now = |i: usize, closes: &[f64]| -> bool {
            bull.contains(&date_key(&s[i].0))
                && running_from_series(&closes[..=i], 1, rcfg).0 == want
        };
        for &h in horizons {
            let (a_hit, a_n) = odds(s, gap, h, None);
            let (l_hit, l_n) = odds(s, gap, h, Some(&like_now));
            row.odds.insert(
                h.to_string(),
                Odds {
                    all: (a_n > 0).then(|| a_hit as f64 / a_n as f64),
                    all_n: a_n,
                    now: (l_n > 0).then(|| l_hit as f64 / l_n as f64),
                    now_n: l_n,
                },
            );
        }
        row.spot = Some(px);
        row.gap = Some(gap);
        row.running = Some(want);
        rows.push(row);
    }
    let now_at = |r: &Row| {
        r.odds
            .get(&hmax.to_string())
            .and_then(|o| o.now)
            .unwrap_or(0.0)
    };
    let total = py_sum(rows.iter().map(|r| r.usd));
    let exp = py_sum(rows.iter().map(|r| r.usd * now_at(r)));
    let low = py_sum(rows.iter().filter(|r| now_at(r) < low_odds).map(|r| r.usd));
    let mut cache_last: IndexMap<String, Value> = IndexMap::new();
    for (k, v) in &series {
        if let Some(last) = v.last() {
            cache_last.insert(k.clone(), last.0.clone());
        }
    }
    let hist = history.cloned().unwrap_or_else(|| json!({}));
    Ok(FillOdds {
        horizons: horizons.to_vec(),
        rows,
        total_usd: total,
        expected_fill_usd: exp,
        low_odds_usd: low,
        low_odds,
        market: regime.get("market").cloned().unwrap_or(Value::Null),
        held_days: hist.get("held_days").cloned().unwrap_or(Value::Null),
        confirmed: hist.get("confirmed").cloned().unwrap_or(Value::Null),
        cache_last,
    })
}

fn pct(x: Option<f64>) -> String {
    match x {
        None => "  -- ".into(),
        Some(v) => format!("{:>4}%", crate::pyfmt::fixed(v * 100.0, 0)),
    }
}

fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        v => crate::pyfmt::value_str(v),
    }
}

/// The table `rungbot-exec fillodds` prints.
pub fn render(res: &FillOdds) -> String {
    let hs = &res.horizons;
    let stage = if crate::pyfmt::truthy(Some(&res.confirmed)) {
        "confirmed"
    } else {
        "provisional"
    };
    let mut out = vec![
        format!(
            "Fill odds of resting buy rungs · market {} held {}d ({stage})",
            py_str(&res.market),
            py_str(&res.held_days)
        ),
        "odds = share of history where the daily low reached the rung within H days; 'now' = coin's RUN gate as today + BTC above 100d/200d SMA".into(),
        String::new(),
    ];
    let mut head = format!("{:5} {:>4} {:>7} {:>6}", "coin", "rung", "$ left", "gap");
    for h in hs {
        head.push_str(&format!("  {h}d all {h}d now"));
    }
    let head = head + "   n(now)";
    out.push(head.clone());
    out.push("-".repeat(head.chars().count()));
    let hmax = hs.iter().copied().max().unwrap_or(0);
    for r in &res.rows {
        let rung = r
            .rung
            .filter(|x| *x != 0)
            .map(|x| x.to_string())
            .unwrap_or_default();
        let mut line = format!(
            "{:5} {:>4} {:>7} ",
            r.sym,
            rung,
            crate::pyfmt::fixed(r.usd, 0)
        );
        let Some(gap) = r.gap else {
            out.push(line + "   no spot or history");
            continue;
        };
        line.push_str(&format!("{:>5}%", crate::pyfmt::fixed(gap * 100.0, 1)));
        for h in hs {
            let o = &r.odds[&h.to_string()];
            line.push_str(&format!("  {:>6} {:>6}", pct(o.all), pct(o.now)));
        }
        let n_now = r.odds.get(&hmax.to_string()).map_or(0, |o| o.now_n);
        out.push(line + &format!("   {n_now:>5}"));
    }
    let mut last: Vec<(&String, String)> =
        res.cache_last.iter().map(|(k, v)| (k, py_str(v))).collect();
    last.sort();
    out.push(String::new());
    out.push(format!(
        "resting ${} · expected to fill within {hmax}d (like now) ${} · under {}% odds ${}",
        comma(res.total_usd, 0),
        comma(res.expected_fill_usd, 0),
        crate::pyfmt::fixed(res.low_odds * 100.0, 0),
        comma(res.low_odds_usd, 0)
    ));
    out.push(format!(
        "history through {}",
        last.iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.join("\n")
}
