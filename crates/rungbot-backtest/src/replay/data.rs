//! Calendar arithmetic and the candle-cache formats the replays read.
//!
//! Every cache is public market data, one row per UTC day, keyed by `YYYY-MM-DD`:
//!
//! * **ohlc** — `[[date, open, high, low, close], ...]`, what the candle fetcher writes
//!   per venue (`binance_BTCUSDT.json`, `gate_FET_USDT.json`, ...).
//! * **px** — `[[date, price], ...]`, a price-only series (e.g. DefiLlama).
//! * **rows** — `{"rows": [{"date", "open", "high", "low", "close", ...}]}`, the long
//!   BTC daily history.
//! * **coinmetrics** — `[{"time": "2010-07-18T...", "PriceUSD": "0.0858"}, ...]`, BTC
//!   before the venues existed.
//! * **fng** — `[{"timestamp": "...", "value": "..."}]`, the Fear & Greed index.

use crate::py::Py;

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` -> days since epoch. Panics on a malformed date: the caches are ours.
pub fn day_num(s: &str) -> i64 {
    let p: Vec<i64> = s[..10]
        .split('-')
        .map(|x| x.parse().unwrap_or_else(|_| panic!("bad date {s:?}")))
        .collect();
    days_from_civil(p[0], p[1], p[2])
}

pub fn day_str(z: i64) -> String {
    let (y, m, d) = civil_from_days(z);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `date + k days`, as a string.
pub fn shift(s: &str, k: i64) -> String {
    day_str(day_num(s) + k)
}

/// ISO `(year, week)` of a day.
pub fn iso_week(z: i64) -> (i64, i64) {
    let wd = (z + 3).rem_euclid(7) + 1; // Monday = 1; 1970-01-01 was a Thursday
    let thursday = z - wd + 4;
    let (y, _, _) = civil_from_days(thursday);
    let jan1 = days_from_civil(y, 1, 1);
    (y, (thursday - jan1) / 7 + 1)
}

/// UTC day of an epoch in seconds.
pub fn day_of_epoch(secs: i64) -> String {
    day_str(secs.div_euclid(86_400))
}

/// One daily OHLC row.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

fn f(v: &Py) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

/// An `ohlc` cache.
pub fn ohlc(v: &Py) -> Vec<Bar> {
    v.as_list()
        .iter()
        .map(|r| {
            let r = r.as_list();
            Bar {
                date: r[0].to_py_string(),
                open: f(&r[1]),
                high: f(&r[2]),
                low: f(&r[3]),
                close: f(&r[4]),
            }
        })
        .collect()
}

/// A `px` cache: `(date, price)`.
pub fn px(v: &Py) -> Vec<(String, f64)> {
    v.as_list()
        .iter()
        .map(|r| {
            let r = r.as_list();
            (r[0].to_py_string(), f(&r[1]))
        })
        .collect()
}

/// A `rows` cache (the long BTC history).
pub fn rows(v: &Py) -> Vec<Bar> {
    v.get("rows")
        .map(|r| r.as_list())
        .unwrap_or(&[])
        .iter()
        .map(|r| Bar {
            date: r.get("date").map(|d| d.to_py_string()).unwrap_or_default(),
            open: r.get("open").map(f).unwrap_or(f64::NAN),
            high: r.get("high").map(f).unwrap_or(f64::NAN),
            low: r.get("low").map(f).unwrap_or(f64::NAN),
            close: r.get("close").map(f).unwrap_or(f64::NAN),
        })
        .collect()
}

/// Convert `ohlc` bars into the `rows` shape.
pub fn ohlc_to_rows(bars: &[Bar]) -> Py {
    let rows = bars
        .iter()
        .map(|b| {
            crate::pydict![
                ("date", b.date.as_str()),
                ("open", b.open),
                ("high", b.high),
                ("low", b.low),
                ("close", b.close)
            ]
        })
        .collect();
    crate::pydict![("rows", Py::List(rows))]
}

/// A `px` cache as bars with open = high = low = close.
pub fn px_bars(v: &Py) -> Vec<Bar> {
    px(v)
        .into_iter()
        .map(|(date, p)| Bar {
            date,
            open: p,
            high: p,
            low: p,
            close: p,
        })
        .collect()
}

/// A `coinmetrics` cache (or `{"data": [...]}`): `(date, price)` for every row that has
/// both. Rows may also be `[date, price]` pairs.
pub fn coinmetrics(v: &Py) -> Vec<(String, f64)> {
    let rows = match v.get("data") {
        Some(d) => d.as_list(),
        None => v.as_list(),
    };
    let mut out = Vec::new();
    for r in rows {
        match r {
            Py::Dict(_) => {
                let t = r
                    .get("time")
                    .filter(|x| x.truthy())
                    .or_else(|| r.get("date"));
                let p = r
                    .get("PriceUSD")
                    .filter(|x| x.truthy())
                    .or_else(|| r.get("price"));
                if let (Some(t), Some(p)) = (t, p) {
                    if t.truthy() && p.truthy() {
                        let t = t.to_py_string();
                        out.push((t.chars().take(10).collect(), f(p)));
                    }
                }
            }
            Py::List(x) | Py::Tuple(x) if x.len() >= 2 => {
                out.push((x[0].to_py_string().chars().take(10).collect(), f(&x[1])));
            }
            _ => {}
        }
    }
    out
}

/// A Fear & Greed cache: `(epoch_seconds, value)`.
pub fn fng(v: &Py) -> Vec<(i64, i64)> {
    v.as_list()
        .iter()
        .filter_map(|r| {
            let t = r.get("timestamp")?.as_f64()? as i64;
            let x = r.get("value")?.as_f64()? as i64;
            Some((t, x))
        })
        .collect()
}
