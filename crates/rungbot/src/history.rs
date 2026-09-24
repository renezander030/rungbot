//! Price history for the backtests and replays. Public endpoints, no authentication.
//!
//! * CoinGecko `market_chart`: hourly points for windows up to 90 days, daily beyond —
//!   the window backtest's history, with its timestamps.
//! * Daily OHLC per venue for the replays: Binance (paged forward from 2017-08),
//!   Gate (paged back through `to`), KuCoin (paged back through `endAt`).
//! * The microstate read: recent daily candles with quote volume, spot, and BTC
//!   funding / open interest / Fear & Greed.
//!
//! Every call goes through [`crate::tickers::get_text`], so `RUNGBOT_OFFLINE=1` refuses
//! all of them.

use std::time::Duration;

use rungbot_backtest::py::{self, loads, Py};
use rungbot_backtest::replay::data::day_of_epoch;
use rungbot_backtest::replay::microstate::{Candle, Feed};
use rungbot_backtest::History;

use crate::tickers::{get_text, TickerError};

fn sleep(secs: f64) {
    if std::env::var("RUNGBOT_OFFLINE").as_deref() != Ok("1") {
        std::thread::sleep(Duration::from_secs_f64(secs));
    }
}

fn get_py(url: &str) -> Result<Py, String> {
    let (status, body) = get_text(url).map_err(|e| e.to_string())?;
    if status != 200 {
        let head: String = body.chars().take(120).collect();
        return Err(format!("{url} -> {status} {head}"));
    }
    loads(&body).map_err(|e| format!("{url}: bad JSON: {e}"))
}

/// `[(epoch_seconds, price)]` for the last `days` from CoinGecko, retrying a rate limit.
pub fn coingecko_chart(id: &str, days: i64) -> Result<Vec<(i64, f64)>, String> {
    let url = format!(
        "https://api.coingecko.com/api/v3/coins/{id}/market_chart?vs_currency=usd&days={days}"
    );
    let tries = 5;
    for k in 0..tries {
        let (status, body) = get_text(&url).map_err(|e| e.to_string())?;
        if status == 429 && k < tries - 1 {
            sleep(20.0 * (k + 1) as f64);
            continue;
        }
        if status != 200 {
            return Err(format!("HTTP Error {status}"));
        }
        let v = loads(&body).map_err(|e| format!("bad JSON: {e}"))?;
        return Ok(v
            .get("prices")
            .map(|p| p.as_list())
            .unwrap_or(&[])
            .iter()
            .filter_map(|r| {
                let r = r.as_list();
                Some(((r.first()?.as_f64()? / 1000.0) as i64, r.get(1)?.as_f64()?))
            })
            .collect());
    }
    Err("rate limited".into())
}

/// Fetch every coin's window history in order; `Err` is the line the report prints.
pub fn window_history(ids: &[(String, String)], days: i64) -> Result<History, String> {
    let mut out = Vec::new();
    for (sym, id) in ids {
        match coingecko_chart(id, days) {
            Ok(pts) => out.push((sym.clone(), pts)),
            Err(e) => return Err(format!("history {sym} failed: {e}")),
        }
        sleep(5.0);
    }
    Ok(History(out))
}

// ------------------------------------------------------------------ replay candles

/// A GET that returns `(status, body)` and retries transport errors, like the reference.
fn get_retry(url: &str, tries: usize) -> Option<(i32, Py)> {
    for i in 0..tries {
        match get_text(url) {
            Ok((status, body)) if status == 400 || status == 404 => {
                return Some((status, Py::Str(body.chars().take(200).collect())));
            }
            Ok((200, body)) => match loads(&body) {
                Ok(v) => return Some((200, v)),
                Err(_) => sleep(1.5 * (i + 1) as f64),
            },
            Ok(_) => sleep(1.5 * (i + 1) as f64),
            Err(TickerError::Offline(_)) => return None,
            Err(_) => sleep(1.5 * (i + 1) as f64),
        }
    }
    None
}

fn row(date: String, o: &Py, h: &Py, l: &Py, c: &Py) -> Py {
    let f = |v: &Py| Py::Float(v.as_f64().unwrap_or(f64::NAN));
    Py::List(vec![Py::Str(date), f(o), f(h), f(l), f(c)])
}

fn fail(what: &str, code: Option<i32>, body: Option<&Py>) -> String {
    let code = code.map(|c| c.to_string()).unwrap_or_else(|| "None".into());
    let body: String = body
        .map(|b| b.to_py_string())
        .unwrap_or_else(|| "None".into())
        .chars()
        .take(120)
        .collect();
    format!("{what} -> {code} {body}")
}

fn sorted(out: std::collections::BTreeMap<String, Py>) -> Vec<Py> {
    out.into_values().collect()
}

/// Binance daily OHLC from `start` onwards, paged forward.
pub fn binance_daily_ohlc(symbol: &str, start_ms: i64) -> Result<Vec<Py>, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut st = start_ms;
    loop {
        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol={symbol}&interval=1d&limit=1000&startTime={st}"
        );
        let got = get_retry(&url, 4);
        let body = match &got {
            Some((200, b)) if matches!(b, Py::List(_)) => b,
            _ => {
                return Err(fail(
                    &format!("binance {symbol}"),
                    got.as_ref().map(|g| g.0),
                    got.as_ref().map(|g| &g.1),
                ))
            }
        };
        let ks = body.as_list();
        if ks.is_empty() {
            break;
        }
        for k in ks {
            let k = k.as_list();
            let d = day_of_epoch(k[0].as_f64().unwrap_or(0.0) as i64 / 1000);
            out.insert(d.clone(), row(d, &k[1], &k[2], &k[3], &k[4]));
        }
        if ks.len() < 1000 {
            break;
        }
        st = ks[ks.len() - 1].as_list()[0].as_f64().unwrap_or(0.0) as i64 + 86_400_000;
        sleep(0.25);
    }
    Ok(sorted(out))
}

/// Gate daily OHLC, paged back from `now`.
pub fn gate_daily_ohlc(pair: &str, now: i64) -> Result<Vec<Py>, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut to = now;
    for _ in 0..12 {
        let url = format!(
            "https://api.gateio.ws/api/v4/spot/candlesticks?currency_pair={pair}&interval=1d&limit=1000&to={to}"
        );
        let got = get_retry(&url, 4);
        let body = match &got {
            Some((200, b)) if matches!(b, Py::List(_)) => b,
            _ => {
                if !out.is_empty() {
                    break;
                }
                return Err(fail(
                    &format!("gate {pair}"),
                    got.as_ref().map(|g| g.0),
                    got.as_ref().map(|g| &g.1),
                ));
            }
        };
        let ks = body.as_list();
        if ks.is_empty() {
            break;
        }
        // [ts, quote_vol, close, high, low, open, base_vol, finished]
        for k in ks {
            let k = k.as_list();
            let d = day_of_epoch(k[0].as_f64().unwrap_or(0.0) as i64);
            out.insert(d.clone(), row(d, &k[5], &k[3], &k[4], &k[2]));
        }
        let first_ts = ks[0].as_list()[0].as_f64().unwrap_or(0.0) as i64;
        if ks.len() < 2 {
            break;
        }
        to = first_ts - 86_400;
        sleep(0.3);
    }
    Ok(sorted(out))
}

/// KuCoin daily OHLC, paged back from `now`.
pub fn kucoin_daily_ohlc(symbol: &str, now: i64) -> Result<Vec<Py>, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut end = now;
    for _ in 0..12 {
        let url = format!(
            "https://api.kucoin.com/api/v1/market/candles?type=1day&symbol={symbol}&startAt=0&endAt={end}"
        );
        let got = get_retry(&url, 4);
        let ok = matches!(&got, Some((200, b)) if b.get("code").and_then(|c| c.as_str()) == Some("200000"));
        if !ok {
            if !out.is_empty() {
                break;
            }
            return Err(fail(
                &format!("kucoin {symbol}"),
                got.as_ref().map(|g| g.0),
                got.as_ref().map(|g| &g.1),
            ));
        }
        let body = &got.as_ref().expect("checked").1;
        let data = body.get("data").map(|d| d.as_list()).unwrap_or(&[]);
        if data.is_empty() {
            break;
        }
        // [time, open, close, high, low, volume, turnover], newest first
        for k in data {
            let k = k.as_list();
            let d = day_of_epoch(k[0].as_f64().unwrap_or(0.0) as i64);
            out.insert(d.clone(), row(d, &k[1], &k[3], &k[4], &k[2]));
        }
        let oldest = data
            .iter()
            .filter_map(|k| k.as_list().first().and_then(|t| t.as_f64()))
            .fold(f64::INFINITY, f64::min) as i64;
        if data.len() < 1500 {
            break;
        }
        end = oldest - 1;
        sleep(0.3);
    }
    Ok(sorted(out))
}

// ------------------------------------------------------------------ microstate feed

/// The live microstate feed: public candles, spot and derivatives.
pub struct LiveFeed;

fn candle(t: &Py, o: &Py, h: &Py, l: &Py, c: &Py, qv: &Py) -> Candle {
    let f = |v: &Py| v.as_f64().unwrap_or(f64::NAN);
    Candle {
        t: f(t) as i64,
        o: f(o),
        h: f(h),
        l: f(l),
        c: f(c),
        qv: f(qv),
    }
}

const MICRO_N: usize = 45;

impl Feed for LiveFeed {
    fn daily(&self, venue: &str, pair: &str) -> Result<Vec<Candle>, String> {
        match venue {
            "binance" => {
                let d = get_py(&format!(
                    "https://api.binance.com/api/v3/klines?symbol={pair}&interval=1d&limit={MICRO_N}"
                ))?;
                Ok(d.as_list()
                    .iter()
                    .map(|k| {
                        let k = k.as_list();
                        let mut c = candle(&k[0], &k[1], &k[2], &k[3], &k[4], &k[7]);
                        c.t /= 1000;
                        c
                    })
                    .collect())
            }
            "gate" => {
                let d = get_py(&format!(
                    "https://api.gateio.ws/api/v4/spot/candlesticks?currency_pair={pair}&interval=1d&limit={MICRO_N}"
                ))?;
                let mut rows: Vec<Candle> = d
                    .as_list()
                    .iter()
                    .map(|k| {
                        let k = k.as_list();
                        candle(&k[0], &k[5], &k[3], &k[4], &k[2], &k[1])
                    })
                    .collect();
                rows.sort_by_key(|r| r.t);
                Ok(rows)
            }
            "kucoin" => {
                let d = get_py(&format!(
                    "https://api.kucoin.com/api/v1/market/candles?type=1day&symbol={pair}"
                ))?;
                let mut rows: Vec<Candle> = d
                    .get("data")
                    .map(|x| x.as_list())
                    .unwrap_or(&[])
                    .iter()
                    .map(|k| {
                        let k = k.as_list();
                        candle(&k[0], &k[1], &k[3], &k[4], &k[2], &k[6])
                    })
                    .collect();
                rows.sort_by_key(|r| r.t);
                let n = rows.len();
                Ok(rows.split_off(n.saturating_sub(MICRO_N)))
            }
            other => Err(format!("no daily candles from {other}")),
        }
    }

    fn spot(&self, venue: &str, pair: &str) -> Result<f64, String> {
        match venue {
            "binance" => get_py(&format!(
                "https://api.binance.com/api/v3/ticker/price?symbol={pair}"
            ))?
            .get("price")
            .and_then(|p| p.as_f64())
            .ok_or_else(|| format!("binance {pair}: no price")),
            "gate" => get_py(&format!(
                "https://api.gateio.ws/api/v4/spot/tickers?currency_pair={pair}"
            ))?
            .as_list()
            .first()
            .and_then(|t| t.get("last"))
            .and_then(|p| p.as_f64())
            .ok_or_else(|| format!("gate {pair}: no last price")),
            other => Err(format!("no spot from {other}")),
        }
    }

    fn derivs(&self) -> Py {
        let mut out = Py::dict();
        let fl = |v: Option<&Py>| v.and_then(|x| x.as_f64()).unwrap_or(f64::NAN);
        match get_py("https://fapi.binance.com/fapi/v1/premiumIndex?symbol=BTCUSDT") {
            Ok(p) => {
                out.set("funding_now_pct_8h", fl(p.get("lastFundingRate")) * 100.0);
                out.set("mark", fl(p.get("markPrice")));
                out.set(
                    "next_funding_utc",
                    day_of_epoch(fl(p.get("nextFundingTime")) as i64 / 1000),
                );
            }
            Err(e) => out.set("funding_now_err", e),
        }
        match get_py("https://fapi.binance.com/fapi/v1/fundingRate?symbol=BTCUSDT&limit=90") {
            Ok(fr) => {
                let rates: Vec<f64> = fr
                    .as_list()
                    .iter()
                    .map(|x| fl(x.get("fundingRate")))
                    .collect();
                let n = rates.len() as f64;
                out.set(
                    "funding_30d_avg_pct_8h",
                    py::sum(rates.iter().copied()) / n * 100.0,
                );
                out.set(
                    "funding_30d_neg_count",
                    rates.iter().filter(|r| **r < 0.0).count(),
                );
                out.set("funding_30d_n", rates.len());
                let tail = &rates[rates.len().saturating_sub(21)..];
                out.set(
                    "funding_last7_avg_pct_8h",
                    py::sum(tail.iter().copied()) / 21.0 * 100.0,
                );
                out.set(
                    "funding_annualized_pct",
                    py::sum(rates.iter().copied()) / n * 3.0 * 365.0 * 100.0,
                );
            }
            Err(e) => out.set("funding_hist_err", e),
        }
        match get_py(
            "https://fapi.binance.com/futures/data/openInterestHist?symbol=BTCUSDT&period=1d&limit=30",
        ) {
            Ok(oi) => {
                let s: Vec<(i64, f64, f64)> = oi
                    .as_list()
                    .iter()
                    .map(|x| {
                        (
                            fl(x.get("timestamp")) as i64 / 1000,
                            fl(x.get("sumOpenInterest")),
                            fl(x.get("sumOpenInterestValue")),
                        )
                    })
                    .collect();
                if let (Some(first), Some(last)) = (s.first(), s.last()) {
                    out.set("oi_btc_first", first.1);
                    out.set("oi_btc_first_day", day_of_epoch(first.0));
                    out.set("oi_btc_last", last.1);
                    out.set("oi_btc_last_day", day_of_epoch(last.0));
                    out.set("oi_btc_30d_change_pct", (last.1 / first.1 - 1.0) * 100.0);
                    let mx = s.iter().fold(s[0], |m, x| if x.1 > m.1 { *x } else { m });
                    out.set("oi_btc_30d_max", mx.1);
                    out.set("oi_btc_30d_max_day", day_of_epoch(mx.0));
                    out.set("oi_last_vs_max_pct", last.1 / mx.1 * 100.0);
                    out.set("oi_usd_last_bn", last.2 / 1e9);
                    out.set("oi_usd_30d_change_pct", (last.2 / first.2 - 1.0) * 100.0);
                } else {
                    out.set("oi_err", "list index out of range");
                }
            }
            Err(e) => out.set("oi_err", e),
        }
        match get_py("https://api.alternative.me/fng/?limit=7&format=json") {
            Ok(fg) => {
                let rows = fg
                    .get("data")
                    .map(|d| d.as_list())
                    .unwrap_or(&[])
                    .iter()
                    .map(|x| {
                        Py::Tuple(vec![
                            Py::Str(day_of_epoch(fl(x.get("timestamp")) as i64)),
                            Py::Int(fl(x.get("value")) as i64),
                            x.get("value_classification").cloned().unwrap_or(Py::None),
                        ])
                    })
                    .collect();
                out.set("fng", Py::List(rows));
            }
            Err(e) => out.set("fng_err", e),
        }
        out
    }
}

/// The Fear & Greed history, newest first as the API lists it.
pub fn fng_history() -> Result<Py, String> {
    let v = get_py("https://api.alternative.me/fng/?limit=0&format=json")?;
    Ok(v.get("data").cloned().unwrap_or(Py::List(Vec::new())))
}

/// Coin Metrics community daily `PriceUSD` for BTC, all pages.
pub fn coinmetrics_btc() -> Result<Py, String> {
    let mut rows = Vec::new();
    let mut url = "https://community-api.coinmetrics.io/v4/timeseries/asset-metrics?assets=btc&metrics=PriceUSD&frequency=1d&page_size=10000".to_string();
    loop {
        let v = get_py(&url)?;
        rows.extend(
            v.get("data")
                .map(|d| d.as_list().to_vec())
                .unwrap_or_default(),
        );
        match v.get("next_page_url").and_then(|u| u.as_str()) {
            Some(next) if !next.is_empty() => url = next.to_string(),
            _ => break,
        }
    }
    Ok(Py::List(rows))
}
