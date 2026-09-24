//! `scenarios.json`: the book from `data.json` run through the scenario model
//! ([`rungbot_core::scenarios`]), with the price history from the candle cache and the
//! all-time highs refreshed from CoinGecko when it answers.

use std::collections::BTreeMap;

use rungbot_core::scenarios::{self as model, Book, BookCoin, Model, Series, Zone};
use rungbot_core::watch::json::Json;

use super::{iso_seconds, net, py_float, DashConfig, Io};
use crate::run::config::RunConfig;

/// `float(c[key])`, with the reference's error text.
fn field(c: &Json, key: &str) -> Result<f64, String> {
    py_float(c.get(key).ok_or_else(|| format!("KeyError: '{key}'"))?)
}

/// Today's book from `data.json`: every coin's holding, price and cost basis, the free
/// stable cash, and the resting deploy zones from the journal.
pub fn book(cfg: &RunConfig, d: &DashConfig) -> Result<Book, String> {
    let text = std::fs::read_to_string(d.data_path())
        .map_err(|e| format!("{}: {e}", d.data_path().display()))?;
    let snap: Json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut coins: Vec<BookCoin> = Vec::new();
    for c in snap.get("coins").map(Json::items).unwrap_or(&[]) {
        let sym = c.get("sym").ok_or("KeyError: 'sym'")?.py_str();
        let held = field(c, "held")?;
        let price = field(c, "price")?;
        let cost = match c.get("entry").filter(|e| e.truthy()) {
            Some(e) => py_float(e)?,
            None => price,
        };
        let coin = BookCoin {
            sym: sym.clone(),
            held,
            price,
            cost,
        };
        match coins.iter_mut().find(|x| x.sym == sym) {
            Some(slot) => *slot = coin,
            None => coins.push(coin),
        }
    }
    let port = snap.get("portfolio").cloned().unwrap_or_default();
    let num = |k: &str| match port.get(k) {
        Some(v) => py_float(v),
        None => Ok(0.0),
    };
    let x = num("stable_bag")? - num("stable_locked")?;
    let cash_free = if x > 0.0 { x } else { 0.0 };
    // Any unreadable journal row ends the zone list where it stands.
    let mut zones = Vec::new();
    if let Ok(t) = std::fs::read_to_string(cfg.journal_path()) {
        if let Ok(j) = serde_json::from_str::<Json>(&t) {
            let rows: Vec<&Json> = match &j {
                Json::Obj(o) => o.iter().map(|(_, v)| v).collect(),
                Json::Arr(a) => a.iter().collect(),
                _ => Vec::new(),
            };
            for o in rows {
                let is = |k: &str, vals: &[&str]| matches!(o.get(k), Some(Json::Str(s)) if vals.contains(&s.as_str()));
                if is("kind", &["deploy_buy"])
                    && is("status", &["open", "new", "NEW", "placed"])
                    && o.get("rung").is_some_and(Json::truthy)
                {
                    let z = (|| -> Result<Zone, String> {
                        Ok(Zone {
                            sym: o.get("sym").ok_or("KeyError")?.py_str(),
                            price: field(o, "price")?,
                            quote: field(o, "quote")?,
                        })
                    })();
                    match z {
                        Ok(z) => zones.push(z),
                        Err(_) => break,
                    }
                }
            }
        }
    }
    Ok(Book {
        coins,
        cash_free,
        zones,
    })
}

/// A coin's history files, first wins: the configured ones, else the candle cache's
/// usual names.
pub fn sources_for(d: &DashConfig, sym: &str) -> Vec<(String, String)> {
    match d.scenarios.sources.iter().find(|(s, _)| s == sym) {
        Some((_, v)) => v.clone(),
        None => vec![
            (format!("binance_{sym}USDT.json"), "ohlc".into()),
            (format!("gate_{sym}_USDT.json"), "ohlc".into()),
        ],
    }
}

pub fn load_series(d: &DashConfig, sym: &str) -> Result<Series, String> {
    let mut srcs = Vec::new();
    for (file, kind) in sources_for(d, sym) {
        let p = d.scenarios.replay_cache.join(&file);
        if !p.exists() {
            continue;
        }
        let t = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let v: Json = serde_json::from_str(&t).map_err(|e| format!("{}: {e}", p.display()))?;
        srcs.push((kind, v));
    }
    model::series_from_sources(&srcs)
}

/// The all-time highs, refreshed from CoinGecko where it answers; the configured
/// values stand otherwise.
pub fn refresh_ath(d: &DashConfig, io: &mut Io, ath: &mut [(String, f64, String)]) {
    let ids: Vec<&str> = ath
        .iter()
        .filter_map(|(s, _, _)| d.coingecko_id(s))
        .collect();
    if ids.is_empty() {
        return;
    }
    let url = format!(
        "https://api.coingecko.com/api/v3/coins/markets?vs_currency=usd&ids={}",
        ids.join(",")
    );
    let Ok(Json::Arr(rows)) =
        net::fetch(&io.http, &url, None, &[("User-Agent", "Mozilla/5.0")], 15)
    else {
        return;
    };
    for c in rows {
        let Some(Json::Str(sym)) = c.get("symbol") else {
            return;
        };
        let sym = sym.to_uppercase();
        let Some(slot) = ath.iter_mut().find(|(s, _, _)| *s == sym) else {
            continue;
        };
        let Some(a) = c.get("ath").filter(|a| a.truthy()) else {
            continue;
        };
        let Ok(a) = py_float(a) else {
            return;
        };
        let date = c
            .get("ath_date")
            .cloned()
            .unwrap_or(Json::Str(String::new()))
            .py_str();
        slot.1 = a;
        slot.2 = date.chars().take(10).collect();
    }
}

/// The venue fee for a coin, in percent: the override, else its route's venue.
pub fn fee_pct(cfg: &RunConfig, d: &DashConfig, sym: &str) -> f64 {
    if let Some(f) = d.scenarios.fee_pct {
        return f;
    }
    match cfg.route(sym).map(|r| r.exch.as_str()) {
        Some("revx") => cfg.fee_pct_revx,
        Some("gate") => cfg.fee_pct_gate,
        Some("binance") => cfg.fee_pct_binance,
        _ => 0.1,
    }
}

/// Build and write `scenarios.json`, printing the forecast, size, ATH and per-scenario
/// lines.
pub fn run(cfg: &RunConfig, d: &DashConfig, io: &mut Io) -> Result<(), String> {
    let b = book(cfg, d)?;
    let mut series = BTreeMap::new();
    for c in &b.coins {
        series.insert(c.sym.clone(), load_series(d, &c.sym)?);
    }
    let mut fees = BTreeMap::new();
    for s in b
        .coins
        .iter()
        .map(|c| &c.sym)
        .chain(b.zones.iter().map(|z| &z.sym))
    {
        fees.insert(s.clone(), fee_pct(cfg, d, s));
    }
    let mut ath = d.scenarios.ath.clone();
    refresh_ath(d, io, &mut ath);
    let alts = d.scenarios.alts.clone().unwrap_or_else(|| {
        cfg.watchlist
            .iter()
            .map(|(s, _)| s.clone())
            .filter(|s| s != model::MARKET)
            .collect()
    });
    let alloc = d.scenarios.alloc.clone().unwrap_or_else(|| {
        cfg.deploy_alloc
            .iter()
            .map(|(s, w)| (s.to_uppercase(), *w))
            .collect()
    });
    let m = Model {
        fee_default: d.scenarios.fee_pct.unwrap_or(0.1),
        fee_pct: fees,
        slip_pct: d.scenarios.slip_pct,
        dex_other_usd: d.scenarios.dex_usd,
        new_cash_usd: d.scenarios.new_cash_usd,
        paths: d.scenarios.paths,
        seed: d.scenarios.seed,
        alts,
        scenarios: d.scenarios.windows.clone(),
        ath,
        alloc,
        timing: d.scenarios.timing.clone(),
        policy: cfg.sell_policy_config(),
    };
    let out = model::run(&m, &b, &series, &iso_seconds((io.clock)()))?;
    for l in &out.head {
        let _ = writeln!(io.out, "{l}");
    }
    let body = out.json.dumps_compact();
    crate::store::write_atomic(&d.scenarios_path(), &body)?;
    let _ = writeln!(io.out, "{}", out.size_line(body.len()));
    for l in &out.tail {
        let _ = writeln!(io.out, "{l}");
    }
    Ok(())
}
