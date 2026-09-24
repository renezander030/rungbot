//! Current market microstate per coin, read against the resting orders.
//!
//! From the last 30 completed daily candles plus today's partial: 7- and 30-day returns,
//! the 30-day range and how far spot sits from it, volume trend, realised volatility,
//! the worst 1- and 3-day moves, the 30-day SMA, strength against BTC — and for every
//! resting buy rung, how far below spot it sits in percent and in daily sigmas, and how
//! many of the last 30 days had an intraday low that deep.
//!
//! The candle pulls and the derivatives read (funding, open interest, Fear & Greed) are
//! network calls and live in the CLI; this is the arithmetic.

use crate::py::{fs, pstdev, round, round_half_even, sum, Py};
use crate::pydict;
use crate::replay::data::day_of_epoch;

/// One daily candle with its quote volume.
#[derive(Debug, Clone, PartialEq)]
pub struct Candle {
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub qv: f64,
}

/// A coin's resting orders and cost basis.
///
/// Numbers are kept as written (an int stays an int in the JSON document), which is why
/// they are [`Py`] values rather than floats.
#[derive(Debug, Clone)]
pub struct Orders {
    /// Buy rungs: `(price, quote amount)`.
    pub rungs: Vec<(Py, Py)>,
    /// Resting limit sells: `(price, quantity)`.
    pub sells: Vec<(Py, Py)>,
    pub cost: Py,
}

fn num(v: &Py) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn tuple_min(v: &[(f64, i64)]) -> (f64, i64) {
    let mut b = v[0];
    for x in &v[1..] {
        if x.0 < b.0 || (x.0 == b.0 && x.1 < b.1) {
            b = *x;
        }
    }
    b
}

fn tuple_max(v: &[(f64, i64)]) -> (f64, i64) {
    let mut b = v[0];
    for x in &v[1..] {
        if x.0 > b.0 || (x.0 == b.0 && x.1 > b.1) {
            b = *x;
        }
    }
    b
}

/// `(t -> close)` for BTC's candles plus its spot, for the strength-vs-BTC read.
pub struct BtcRef<'a> {
    pub closes: &'a [(i64, f64)],
    pub spot: f64,
}

/// One coin's microstate document.
pub fn metrics(sym: &str, rows: &[Candle], spot: f64, btc: Option<&BtcRef>, ord: &Orders) -> Py {
    let rows = &rows[rows.len().saturating_sub(31)..];
    let done = &rows[..rows.len() - 1];
    let closes: Vec<f64> = done.iter().map(|r| r.c).collect();
    let (c7, c30) = (closes[closes.len() - 7], closes[closes.len() - 30]);
    let (lo, lo_t) = tuple_min(&rows.iter().map(|r| (r.l, r.t)).collect::<Vec<_>>());
    let (hi, hi_t) = tuple_max(&rows.iter().map(|r| (r.h, r.t)).collect::<Vec<_>>());
    let today = rows[rows.len() - 1].t;
    let v7 = sum(done[done.len() - 7..].iter().map(|r| r.qv)) / 7.0;
    let v30 = sum(done.iter().map(|r| r.qv)) / 30.0;
    let lr: Vec<f64> = (1..closes.len())
        .map(|i| (closes[i] / closes[i - 1]).ln())
        .collect();
    let vol = pstdev(&lr) * 100.0;
    let worst1 = lr.iter().cloned().fold(f64::INFINITY, f64::min) * 100.0;
    let worst3 = (0..lr.len() - 2)
        .map(|i| sum(lr[i..i + 3].iter().copied()))
        .fold(f64::INFINITY, f64::min)
        * 100.0;
    let sma30 = sum(closes.iter().copied()) / 30.0;
    let t30 = done[done.len() - 30].t;
    let ratio = btc.and_then(|b| {
        let at = b.closes.iter().find(|(t, _)| *t == t30).map(|x| x.1)?;
        if b.spot == 0.0 {
            return None;
        }
        Some(((spot / b.spot) / (c30 / at) - 1.0) * 100.0)
    });
    let rungs: Vec<Py> = ord
        .rungs
        .iter()
        .enumerate()
        .map(|(i, (px_v, usd))| {
            let px = num(px_v);
            let dist = (1.0 - px / spot) * 100.0;
            let red = (1..rows.len())
                .filter(|j| (1.0 - rows[*j].l / rows[j - 1].c) * 100.0 >= dist)
                .count();
            pydict![
                ("rung", i + 1),
                ("price", px_v.clone()),
                ("usd", usd.clone()),
                ("dist_pct", round(dist, 2)),
                (
                    "sigma",
                    if vol != 0.0 {
                        Py::Float(round(dist / vol, 2))
                    } else {
                        Py::None
                    }
                ),
                ("days_in_30_with_low_that_deep", red),
                ("below_30d_low", px < lo),
                ("vs_30d_sma_pct", round((px / sma30 - 1.0) * 100.0, 2))
            ]
        })
        .collect();
    let sells: Vec<Py> = ord
        .sells
        .iter()
        .map(|(px, q)| {
            pydict![
                ("price", px.clone()),
                ("qty", q.clone()),
                ("above_spot_pct", round((num(px) / spot - 1.0) * 100.0, 2))
            ]
        })
        .collect();
    pydict![
        ("sym", sym),
        ("spot", spot),
        ("spot_day", day_of_epoch(today)),
        ("ret7_pct", round((spot / c7 - 1.0) * 100.0, 2)),
        ("ret30_pct", round((spot / c30 - 1.0) * 100.0, 2)),
        ("low30", lo),
        ("low30_date", day_of_epoch(lo_t)),
        ("high30", hi),
        ("high30_date", day_of_epoch(hi_t)),
        ("days_since_high30", (today - hi_t).div_euclid(86_400)),
        ("days_since_low30", (today - lo_t).div_euclid(86_400)),
        ("off_high30_pct", round((spot / hi - 1.0) * 100.0, 2)),
        ("above_low30_pct", round((spot / lo - 1.0) * 100.0, 2)),
        ("vol7_avg_usd", round_half_even(v7)),
        ("vol30_avg_usd", round_half_even(v30)),
        (
            "vol7_vs_vol30",
            if v30 != 0.0 {
                Py::Float(round(v7 / v30, 2))
            } else {
                Py::None
            }
        ),
        ("realized_vol30_daily_pct", round(vol, 2)),
        ("worst_1d_pct", round(worst1, 2)),
        ("worst_3d_pct", round(worst3, 2)),
        ("sma30", sma30),
        ("spot_vs_sma30_pct", round((spot / sma30 - 1.0) * 100.0, 2)),
        ("vs_btc_30d_pct", ratio.map(|r| round(r, 2))),
        ("cost", ord.cost.clone()),
        (
            "pnl_vs_cost_pct",
            round((spot / num(&ord.cost) - 1.0) * 100.0, 2)
        ),
        ("rungs", Py::List(rungs)),
        ("sells", Py::List(sells))
    ]
}

/// The compact table: one line per coin, then every rung and resting sell, then the
/// derivatives read.
pub fn table(res: &Py) -> String {
    let mut o = format!(
        "pulled {}\n",
        res.get("pulled_utc")
            .map(|v| v.to_py_string())
            .unwrap_or_default()
    );
    o.push_str(&format!(
        "{} {} {} {} {} {} {} {} {} {} {} {} {} {} {}\n",
        fs("sym", "5s"),
        fs("spot", ">10s"),
        fs("7d%", ">6s"),
        fs("30d%", ">6s"),
        fs("offHi%", ">6s"),
        fs("dHi", ">3s"),
        fs("lo30", ">10s"),
        fs("lo_date", ">10s"),
        fs("v7/v30", ">6s"),
        fs("vol%", ">5s"),
        fs("w1d%", ">6s"),
        fs("w3d%", ">6s"),
        fs("vsBTC30%", ">8s"),
        fs("vsSMA30%", ">8s"),
        fs("pnl%", ">6s")
    ));
    let coins = res.get("coins").cloned().unwrap_or(Py::None);
    let g = |m: &Py, k: &str| m.get(k).cloned().unwrap_or(Py::None);
    for (sym, m) in coins.items() {
        let sym = sym.to_py_string();
        if let Some(e) = m.get("error") {
            o.push_str(&format!("{} ERROR {}\n", fs(&sym, "5s"), e.to_py_string()));
            continue;
        }
        o.push_str(&format!(
            "{} {} {} {} {} {} {} {} {} {} {} {} {} {} {}\n",
            fs(&sym, "5s"),
            g(m, "spot").fmt(">10.6g"),
            g(m, "ret7_pct").fmt(">6.1f"),
            g(m, "ret30_pct").fmt(">6.1f"),
            g(m, "off_high30_pct").fmt(">6.1f"),
            g(m, "days_since_high30").fmt(">3d"),
            g(m, "low30").fmt(">10.6g"),
            g(m, "low30_date").fmt(">10s"),
            g(m, "vol7_vs_vol30").fmt(">6.2f"),
            g(m, "realized_vol30_daily_pct").fmt(">5.2f"),
            g(m, "worst_1d_pct").fmt(">6.1f"),
            g(m, "worst_3d_pct").fmt(">6.1f"),
            fs(&g(m, "vs_btc_30d_pct").to_py_string(), ">8s"),
            g(m, "spot_vs_sma30_pct").fmt(">8.1f"),
            g(m, "pnl_vs_cost_pct").fmt(">6.1f")
        ));
    }
    o.push_str("rungs: sym r# price dist% sigma redDays30 belowLo30 vsSMA30%\n");
    for (sym, m) in coins.items() {
        let sym = sym.to_py_string();
        if m.get("error").is_some() {
            continue;
        }
        for r in g(m, "rungs").as_list() {
            o.push_str(&format!(
                "  {} r{} {} {} {} {} {} {}\n",
                fs(&sym, "5s"),
                g(r, "rung").to_py_string(),
                g(r, "price").fmt("<10.6g"),
                g(r, "dist_pct").fmt(">6.2f"),
                g(r, "sigma").fmt(">5.2f"),
                g(r, "days_in_30_with_low_that_deep").fmt(">2d"),
                fs(&g(r, "below_30d_low").to_py_string(), ">5s"),
                g(r, "vs_30d_sma_pct").fmt(">7.2f")
            ));
        }
        for s in g(m, "sells").as_list() {
            o.push_str(&format!(
                "  {} SELL {} +{}% above spot, qty {}\n",
                fs(&sym, "5s"),
                g(s, "price").fmt("<10.6g"),
                g(s, "above_spot_pct").fmt(".1f"),
                g(s, "qty").to_py_string()
            ));
        }
    }
    for (k, v) in res.get("btc_derivs").map(|d| d.items()).unwrap_or(&[]) {
        o.push_str(&format!("  {}: {}\n", k.to_py_string(), v.to_py_string()));
    }
    o
}

/// Where the microstate read gets its data. Network calls in the CLI, fixtures in tests.
pub trait Feed {
    /// Daily candles (with today's partial last), oldest first.
    fn daily(&self, venue: &str, pair: &str) -> Result<Vec<Candle>, String>;
    fn spot(&self, venue: &str, pair: &str) -> Result<f64, String>;
    /// BTC funding, open interest and Fear & Greed; failures are recorded inside.
    fn derivs(&self) -> Py;
}

/// One coin to read: symbol, venue (`binance` or `gate`, with a KuCoin fallback for
/// Gate), the venue's pair, and its resting orders.
pub struct Watch {
    pub sym: String,
    pub venue: String,
    pub pair: String,
    pub orders: Orders,
}

/// The full read: BTC first (the reference for strength), then every other coin.
/// Returns the table, the `microstate.json` document, and fallback notes for stderr.
pub fn run(
    coins: &[Watch],
    pulled_utc: &str,
    feed: &dyn Feed,
) -> Result<(String, Py, String), String> {
    let mut res = pydict![
        ("pulled_utc", pulled_utc),
        ("coins", Py::dict()),
        ("btc_derivs", Py::dict())
    ];
    let btc_rows = feed.daily("binance", "BTCUSDT")?;
    let btc_spot = feed.spot("binance", "BTCUSDT")?;
    let btc_closes: Vec<(i64, f64)> = btc_rows.iter().map(|r| (r.t, r.c)).collect();
    let btc_orders = coins
        .iter()
        .find(|c| c.sym == "BTC")
        .map(|c| c.orders.clone())
        .ok_or("the microstate read needs BTC in the watchlist")?;
    let mut out = Py::dict();
    out.set(
        "BTC",
        metrics("BTC", &btc_rows, btc_spot, None, &btc_orders),
    );
    let btc_ref = BtcRef {
        closes: &btc_closes,
        spot: btc_spot,
    };
    let mut notes = String::new();
    for c in coins {
        if c.sym == "BTC" {
            continue;
        }
        let mut venue = c.venue.clone();
        let fetched = if venue == "binance" {
            feed.daily(&venue, &c.pair)
                .and_then(|rows| feed.spot(&venue, &c.pair).map(|s| (rows, s)))
        } else {
            match feed
                .daily(&venue, &c.pair)
                .and_then(|rows| feed.spot(&venue, &c.pair).map(|s| (rows, s)))
            {
                Ok(x) => Ok(x),
                Err(e) => {
                    notes.push_str(&format!("{}: gate failed ({e}), trying kucoin\n", c.sym));
                    venue = "kucoin".into();
                    feed.daily("kucoin", &c.pair.replace('_', "-"))
                        .and_then(|rows| {
                            let spot = rows.last().map(|r| r.c).ok_or("no candles")?;
                            Ok((rows, spot))
                        })
                }
            }
        };
        match fetched {
            Ok((rows, spot)) => {
                let mut m = metrics(&c.sym, &rows, spot, Some(&btc_ref), &c.orders);
                m.set("source", format!("{venue}:{}", c.pair));
                out.set(c.sym.as_str(), m);
            }
            Err(e) => out.set(
                c.sym.as_str(),
                pydict![
                    ("sym", c.sym.as_str()),
                    ("error", e),
                    ("source", format!("{venue}:{}", c.pair))
                ],
            ),
        }
    }
    if let Py::Dict(items) = &mut out {
        if let Some((_, btc)) = items.iter_mut().find(|(k, _)| *k == Py::from("BTC")) {
            btc.set("source", "binance:BTCUSDT");
        }
    }
    res.set("coins", out);
    res.set("btc_derivs", feed.derivs());
    Ok((table(&res), res, notes))
}
