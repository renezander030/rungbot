//! Ladder churn from the order journal: how often the resting zones are rolled (every
//! rung cancelled and re-placed), how many rungs that costs per fill, and how long a rung
//! lives before it is replaced. Read-only.
//!
//! A roll event is a cluster of our own roll cancels on one venue whose replacements
//! were placed in the same 15-minute window; our cancels carry no cancel timestamp, so
//! the event time is the replacement's placement time.

use std::collections::BTreeMap;

use rungbot_core::watch::pyfmt::round as py_round;
use serde::Serialize;

use crate::journal::Order;

/// The note a roll cancel carries.
pub const ROLL_NOTE: &str = "rolled into new tranche";
/// The width of one roll event.
pub const BUCKET_S: f64 = 900.0;

/// One roll event: when (the bucket start), where, how many rungs, and each rolled
/// rung's lifetime in hours.
#[derive(Debug, Clone, PartialEq)]
pub struct RollEvent {
    pub ts: f64,
    pub exch: String,
    pub n: i64,
    pub lifetimes_h: Vec<f64>,
}

fn f(x: Option<f64>) -> f64 {
    x.unwrap_or(0.0)
}

fn is_deploy_rung(o: &Order) -> bool {
    o.kind == "deploy_buy" && o.rung.is_some_and(|r| r != 0)
}

/// Every roll event in `rows`, oldest first.
pub fn roll_events(rows: &[&Order]) -> Vec<RollEvent> {
    let dep: Vec<&Order> = rows
        .iter()
        .copied()
        .filter(|o| is_deploy_rung(o) && f(o.ts) != 0.0)
        .collect();
    let mut by_key: BTreeMap<(&str, &str, i64), Vec<&Order>> = BTreeMap::new();
    for o in &dep {
        by_key
            .entry((o.exch.as_str(), o.sym.as_str(), o.rung.unwrap_or(0)))
            .or_default()
            .push(o);
    }
    for v in by_key.values_mut() {
        v.sort_by(|a, b| {
            f(a.ts)
                .partial_cmp(&f(b.ts))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let mut ev: BTreeMap<(String, i64), (i64, Vec<f64>)> = BTreeMap::new();
    for o in &dep {
        if !o.status.starts_with("cancel") || !o.note.as_deref().unwrap_or("").contains(ROLL_NOTE) {
            continue;
        }
        let key = (o.exch.as_str(), o.sym.as_str(), o.rung.unwrap_or(0));
        let Some(rep) = by_key[&key].iter().find(|p| f(p.ts) > f(o.ts)) else {
            continue;
        };
        let rep_ts = f(rep.ts);
        let e = ev
            .entry((o.exch.clone(), (rep_ts / BUCKET_S).floor() as i64))
            .or_default();
        e.0 += 1;
        e.1.push((rep_ts - f(o.ts)) / 3600.0);
    }
    let mut out: Vec<RollEvent> = ev
        .into_iter()
        .map(|((exch, b), (n, life))| RollEvent {
            ts: b as f64 * BUCKET_S,
            exch,
            n,
            lifetimes_h: life,
        })
        .collect();
    out.sort_by(|a, b| {
        a.ts.partial_cmp(&b.ts)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.exch.cmp(&b.exch))
            .then_with(|| a.n.cmp(&b.n))
            .then_with(|| {
                a.lifetimes_h
                    .partial_cmp(&b.lifetimes_h)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    out
}

/// Churn over the last `days` (and 24 hours), flat, in the dashboard's field order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Churn {
    pub window_days: i64,
    pub roll_events: usize,
    pub roll_events_24h: usize,
    pub rungs_rolled: i64,
    pub rungs_placed: usize,
    pub fills: usize,
    pub cancels_per_fill: Option<f64>,
    pub rung_lifetime_h_median: Option<f64>,
    pub rung_lifetime_h_p25: Option<f64>,
    pub last_roll_ts: Option<f64>,
    pub last_roll_exch: Option<String>,
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// The churn metrics at `now` over `days`.
pub fn metrics(rows: &[&Order], now: f64, days: i64) -> Churn {
    let since = now - days as f64 * 86_400.0;
    let since_24h = now - 86_400.0;
    let events = roll_events(rows);
    let recent: Vec<&RollEvent> = events.iter().filter(|e| e.ts >= since).collect();
    let lifetimes: Vec<f64> = recent
        .iter()
        .flat_map(|e| e.lifetimes_h.iter().copied())
        .collect();
    let dep: Vec<&Order> = rows.iter().copied().filter(|o| is_deploy_rung(o)).collect();
    let placed = dep.iter().filter(|o| f(o.ts) >= since).count();
    let fills = dep
        .iter()
        .filter(|o| {
            let t = [o.filled_ts, o.status_ts, o.ts]
                .into_iter()
                .find(|x| x.is_some_and(|v| v != 0.0))
                .flatten();
            o.status == "filled" && f(t) >= since
        })
        .count();
    let cancelled: i64 = recent.iter().map(|e| e.n).sum();
    let mut sorted = lifetimes.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Churn {
        window_days: days,
        roll_events: recent.len(),
        roll_events_24h: events.iter().filter(|e| e.ts >= since_24h).count(),
        rungs_rolled: cancelled,
        rungs_placed: placed,
        fills,
        cancels_per_fill: (fills > 0).then(|| py_round(cancelled as f64 / fills as f64, 1)),
        rung_lifetime_h_median: (!lifetimes.is_empty()).then(|| py_round(median(&lifetimes), 1)),
        rung_lifetime_h_p25: (!lifetimes.is_empty()).then(|| py_round(sorted[sorted.len() / 4], 1)),
        last_roll_ts: events.last().map(|e| e.ts),
        last_roll_exch: events.last().map(|e| e.exch.clone()),
    }
}

/// The log line: `CHURN 7d: 3 rolls (36 rungs) / 2 fills, 18.0 cancels per fill, rung
/// life median 6.5h; 1 rolls in 24h`.
pub fn summary(m: &Churn) -> String {
    let cpf = m
        .cancels_per_fill
        .map(|x| format!(", {} cancels per fill", crate::pyfmt::float_repr(x)))
        .unwrap_or_default();
    let life = m
        .rung_lifetime_h_median
        .map(|x| format!(", rung life median {}h", crate::pyfmt::float_repr(x)))
        .unwrap_or_default();
    format!(
        "CHURN {}d: {} rolls ({} rungs) / {} fills{cpf}{life}; {} rolls in 24h",
        m.window_days, m.roll_events, m.rungs_rolled, m.fills, m.roll_events_24h
    )
}
