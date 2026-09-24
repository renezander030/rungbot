//! Do BTC bull confirmations transfer to the alts?
//!
//! The label rule is replayed day by day on BTC; an event is the first day a bull streak
//! reaches 14 consecutive labels. Breadth needs a watchlist that did not exist in 2017,
//! so two BTC-only proxies: **A** drops breadth (bull = above both long SMAs, bear =
//! below the 200-day); **B** makes BTC its own watchlist (bull also needs close > SMA30,
//! bear also needs close <= SMA30).
//!
//! For every coin and event: the 90-day return and drawdown from the event close, which
//! dip depths would have filled, what a laddered entry returned for several market-buy
//! shares, and whether the per-coin RUN gate read at day 17 (or day 3) told the winners
//! from the losers. Plus each coin's current gate and its recent rung-1 fill odds.
//!
//! Also here: the depth/weight profile sweep over those events ([`profile_sweep`]) and
//! the current-event read against the live resting rungs ([`current`]).

use std::collections::BTreeMap;

use rungbot_core::regime::{running_from_series, RegimeConfig};

use crate::py::{at, ff, fi, fs, mean, median, repr, round, round_half_even, slice, sum, Py};
use crate::pydict;
use crate::replay::data::{shift, Bar};

pub const CONFIRM_DAYS: usize = 14;
pub const DEPTHS: [i64; 10] = [3, 5, 8, 10, 12, 15, 18, 20, 25, 30];
pub const SHARES: [f64; 5] = [0.0, 0.25, 0.30, 0.50, 1.0];
pub const HORIZON: i64 = 90;

/// `str(share)` as the reference keyed it: the first share was the int `0`.
fn share_key(s: f64) -> String {
    if s == 0.0 {
        "0".into()
    } else {
        repr(s)
    }
}

/// One coin in the study: where its candles live and its deployment zone.
#[derive(Debug, Clone)]
pub struct AltCoin {
    pub sym: String,
    /// `source:symbol`, e.g. `binance:BTCUSDT`, the cache file `binance_BTCUSDT.json`.
    pub source: String,
    pub depths: [f64; 3],
    pub weights: [f64; 3],
    /// The live resting rung-1 price and the spot it was set against, if known.
    pub rung1: Option<(f64, f64)>,
}

pub fn sma(v: &[f64]) -> f64 {
    sum(v.iter().copied()) / v.len() as f64
}

/// `(run, n_signals_true, names_true)` the way the reference counted a gate read.
fn gate(closes: &[f64]) -> (bool, i64, Vec<&'static str>) {
    let (run, s) = running_from_series(closes, 1, RegimeConfig::default());
    if s.insufficient_history {
        return (false, 1, vec!["insufficient_history"]);
    }
    let mut names = Vec::new();
    if s.above_sma30 {
        names.push("above_sma30");
    }
    if s.ret30_strong {
        names.push("ret30_strong");
    }
    if s.fresh_30d_high {
        names.push("fresh_30d_high");
    }
    if s.higher_lows {
        names.push("higher_lows");
    }
    (run, names.len() as i64, names)
}

pub fn btc_labels(closes: &[f64], variant: &str) -> Vec<Option<&'static str>> {
    let mut labs = vec![None; closes.len()];
    for i in 199..closes.len() {
        let px = closes[i];
        let s100 = sma(&closes[i - 99..=i]);
        let s200 = sma(&closes[i - 199..=i]);
        labs[i] = Some(if variant == "A" {
            if px > s100 && px > s200 {
                "bull"
            } else if px < s200 {
                "bear"
            } else {
                "chop"
            }
        } else {
            let s30 = sma(&closes[i - 29..=i]);
            if px > s100 && px > s200 && px > s30 {
                "bull"
            } else if px < s200 && px <= s30 {
                "bear"
            } else {
                "chop"
            }
        });
    }
    labs
}

pub struct Event {
    pub date: String,
    pub i: usize,
    pub flip: String,
    pub prev: Option<&'static str>,
}

pub fn confirmations(dates: &[String], labs: &[Option<&'static str>]) -> Vec<Event> {
    let mut ev = Vec::new();
    let mut streak = 0;
    for (i, l) in labs.iter().enumerate() {
        if *l == Some("bull") {
            streak += 1;
            if streak == CONFIRM_DAYS {
                ev.push(Event {
                    date: dates[i].clone(),
                    i,
                    flip: dates[i + 1 - CONFIRM_DAYS].clone(),
                    prev: labs[at(labs.len(), i as i64 - CONFIRM_DAYS as i64)],
                });
            }
        } else {
            streak = 0;
        }
    }
    ev
}

struct Map<'a>(BTreeMap<&'a str, &'a Bar>);

impl<'a> Map<'a> {
    fn new(rows: &'a [Bar]) -> Self {
        Map(rows.iter().map(|r| (r.date.as_str(), r)).collect())
    }
    fn get(&self, d: &str) -> Option<&'a Bar> {
        self.0.get(d).copied()
    }
    fn close_on_or_before(&self, d: &str, maxback: i64) -> Option<f64> {
        (0..=maxback).find_map(|k| self.get(&shift(d, -k)).map(|r| r.close))
    }
    /// Rows on calendar days d+a..=d+b.
    fn window(&self, d: &str, a: i64, b: i64) -> Vec<&'a Bar> {
        (a..=b).filter_map(|k| self.get(&shift(d, k))).collect()
    }
}

fn ladder_ret(
    entry: f64,
    low_min: f64,
    close_end: f64,
    depths: &[f64; 3],
    weights: &[f64; 3],
    m: f64,
) -> f64 {
    let (mut cash, mut coins) = (1.0 - m, m / entry);
    for (dpt, w) in depths.iter().zip(weights) {
        let alloc = (1.0 - m) * w / 100.0;
        let px = entry * (1.0 - dpt / 100.0);
        if low_min <= px {
            coins += alloc / px;
            cash -= alloc;
        }
    }
    (coins * close_end + cash - 1.0) * 100.0
}

fn med(xs: &[f64]) -> Py {
    if xs.is_empty() {
        Py::None
    } else {
        Py::Float(round(median(xs), 1))
    }
}

fn minf(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::INFINITY, f64::min)
}
fn maxf(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
}

/// One variant's study over all coins. `load(source)` returns a coin's candles.
pub fn analyze(
    variant: &str,
    btc_rows: &[Bar],
    coins: &[AltCoin],
    load: &dyn Fn(&str) -> Vec<Bar>,
) -> Py {
    let dates: Vec<String> = btc_rows.iter().map(|r| r.date.clone()).collect();
    let closes: Vec<f64> = btc_rows.iter().map(|r| r.close).collect();
    let labs = btc_labels(&closes, variant);
    let evs = confirmations(&dates, &labs);
    let btc_m = Map::new(btc_rows);
    let mut out = pydict![
        ("variant", variant),
        (
            "events",
            Py::List(
                evs.iter()
                    .map(|e| pydict![
                        ("date", e.date.as_str()),
                        ("i", e.i),
                        ("flip", e.flip.as_str()),
                        ("prev", e.prev)
                    ])
                    .collect()
            )
        )
    ];
    let mut coins_out = Py::dict();
    for coin in coins {
        let rows = load(&coin.source);
        let m = Map::new(&rows);
        let first = rows[0].date.clone();
        let mut res = Vec::new();
        for e in &evs {
            let d = e.date.as_str();
            let Some(r0) = m.get(d) else { continue };
            let fwd = m.window(d, 1, HORIZON);
            if (fwd.len() as i64) < HORIZON - 10 {
                continue;
            }
            let entry = r0.close;
            let low_min = minf(&fwd.iter().map(|r| r.low).collect::<Vec<_>>());
            let d90 = shift(d, HORIZON);
            let c90 = m
                .close_on_or_before(&d90, 5)
                .expect("a close within 5 days of D+90");
            let b0 = btc_m.get(d).expect("BTC has the event day").close;
            let b90 = btc_m
                .close_on_or_before(&d90, 5)
                .expect("BTC close at D+90");
            let bfwd = btc_m.window(d, 1, HORIZON);
            let blow = minf(&bfwd.iter().map(|r| r.low).collect::<Vec<_>>());
            let mut fills = Py::dict();
            for dp in DEPTHS {
                fills.set(dp, low_min <= entry * (1.0 - dp as f64 / 100.0));
            }
            let mut ladder = Py::dict();
            for s in SHARES {
                ladder.set(
                    share_key(s),
                    ladder_ret(entry, low_min, c90, &coin.depths, &coin.weights, s),
                );
            }
            let mut rec = pydict![
                ("date", d),
                ("prev", e.prev),
                ("entry", entry),
                ("ret90", (c90 / entry - 1.0) * 100.0),
                ("btc_ret90", (b90 / b0 - 1.0) * 100.0),
                ("maxdd90", (low_min / entry - 1.0) * 100.0),
                ("btc_maxdd90", (blow / b0 - 1.0) * 100.0),
                ("fills", fills),
                ("ladder", ladder)
            ];
            for (off, key) in [(17i64, "g17"), (3, "g3")] {
                let dk = shift(d, off);
                let hist: Vec<f64> = rows
                    .iter()
                    .filter(|r| r.date.as_str() <= dk.as_str())
                    .map(|r| r.close)
                    .collect();
                let ck = m.close_on_or_before(&dk, 5).filter(|v| *v != 0.0);
                let bk = btc_m.close_on_or_before(&dk, 5).filter(|v| *v != 0.0);
                match (hist.len() >= 31, ck, bk) {
                    (true, Some(ck), Some(bk)) => {
                        let (run, n, names) = gate(&hist);
                        rec.set(
                            key,
                            pydict![
                                ("run", run),
                                ("n", n),
                                (
                                    "sig",
                                    Py::List(names.iter().map(|x| Py::from(*x)).collect())
                                ),
                                ("ret_to90", (c90 / ck - 1.0) * 100.0),
                                ("btc_ret_to90", (b90 / bk - 1.0) * 100.0),
                                ("ret_D_to_k", (ck / entry - 1.0) * 100.0)
                            ],
                        );
                    }
                    _ => rec.set(key, Py::None),
                }
            }
            res.push(rec);
        }
        // current gate on this source
        let cl: Vec<f64> = rows.iter().map(|r| r.close).collect();
        let (run_now, n_now, sig_now) = gate(&cl);
        // rung-1 fill odds from recent realised moves
        let len = rows.len();
        let mut vol = Py::dict();
        for x in [2i64, 3, 5, 8, 10] {
            let mut vx = Py::dict();
            let xf = x as f64;
            for n in [30i64, 60, 90] {
                let seg = &rows[slice(len, -n, len as i64)];
                let prevc: Vec<f64> = (0..n).map(|j| rows[at(len, -n - 1 + j)].close).collect();
                let cnt = seg
                    .iter()
                    .enumerate()
                    .filter(|(j, r)| r.low <= prevc[*j] * (1.0 - xf / 100.0))
                    .count();
                vx.set(format!("days_low_le_{n}"), cnt);
            }
            for n in [90i64, 180, 365] {
                let mut hits = 0;
                for t in (len as i64 - n - 14)..(len as i64 - 14) {
                    let lm = minf(
                        &rows[slice(len, t + 1, t + 15)]
                            .iter()
                            .map(|r| r.low)
                            .collect::<Vec<_>>(),
                    );
                    if lm <= rows[at(len, t)].close * (1.0 - xf / 100.0) {
                        hits += 1;
                    }
                }
                vx.set(format!("p14_{n}"), round(hits as f64 / n as f64 * 100.0, 1));
            }
            vol.set(x, vx);
        }
        let rung1 = coin
            .rung1
            .map(|(r1, spot)| round((r1 / spot - 1.0) * 100.0, 2));
        let (src, ssym) = coin.source.split_once(':').unwrap_or((&coin.source, ""));
        coins_out.set(
            coin.sym.as_str(),
            pydict![
                ("source", format!("{src}:{ssym}")),
                ("first_date", first.as_str()),
                ("last_close", *cl.last().expect("rows")),
                (
                    "gate_now",
                    pydict![
                        ("run", run_now),
                        ("n", n_now),
                        (
                            "sig",
                            Py::List(sig_now.iter().map(|x| Py::from(*x)).collect())
                        )
                    ]
                ),
                ("rung1_live_dist_pct", rung1),
                ("vol", vol),
                ("events", Py::List(res))
            ],
        );
    }
    out.set("coins", coins_out);
    out
}

fn g(p: &Py, k: &str) -> f64 {
    p.get(k).and_then(|v| v.as_f64()).unwrap_or(f64::NAN)
}

fn prev2(p: &Py) -> String {
    p.get("prev")
        .map(|v| v.to_py_string())
        .unwrap_or_default()
        .chars()
        .take(2)
        .collect()
}

/// The console summary of one variant.
pub fn summarize(out: &Py, coins: &[AltCoin]) -> String {
    let mut o = String::new();
    let v = out.get("variant").unwrap().to_py_string();
    let evs = out.get("events").unwrap().as_list();
    let prev_bear = evs
        .iter()
        .filter(|e| e.get("prev") == Some(&Py::from("bear")))
        .count();
    o.push_str(&format!(
        "\n##### VARIANT {v}: {} BTC bull confirmations ({prev_bear} with prev=bear)\n",
        evs.len()
    ));
    o.push_str(&format!(
        "  {}\n",
        evs.iter()
            .map(|e| format!(
                "{}({},flip {})",
                e.get("date").unwrap().to_py_string(),
                prev2(e),
                &e.get("flip").unwrap().to_py_string()[5..]
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for (sym, c) in out.get("coins").unwrap().items() {
        let sym = sym.to_py_string();
        let r_ = c.get("events").unwrap().as_list();
        let n = r_.len();
        let gn = c.get("gate_now").unwrap();
        o.push_str(&format!(
            "\n=== {sym} src={} first={} events_with_90d={n} gate_now={}/4 {} rung1_live={}%\n",
            c.get("source").unwrap().to_py_string(),
            c.get("first_date").unwrap().to_py_string(),
            gn.get("n").unwrap().repr(),
            gn.get("sig").unwrap().repr(),
            c.get("rung1_live_dist_pct").unwrap().to_py_string()
        ));
        if n == 0 {
            continue;
        }
        let rets: Vec<f64> = r_.iter().map(|r| g(r, "ret90")).collect();
        let rel: Vec<f64> = r_
            .iter()
            .map(|r| g(r, "ret90") - g(r, "btc_ret90"))
            .collect();
        let btc90: Vec<f64> = r_.iter().map(|r| g(r, "btc_ret90")).collect();
        let dd: Vec<f64> = r_.iter().map(|r| g(r, "maxdd90")).collect();
        o.push_str(&format!(
            "  ret90 med/worst/best = {}/{}/{}  btc90 med={}  rel med={} beatBTC={}/{n}  maxdd90 med={} worst={}\n",
            med(&rets).repr(),
            repr(round(minf(&rets), 1)),
            repr(round(maxf(&rets), 1)),
            med(&btc90).repr(),
            med(&rel).repr(),
            rel.iter().filter(|x| **x > 0.0).count(),
            med(&dd).repr(),
            repr(round(minf(&dd), 1))
        ));
        o.push_str(&format!(
            "  fill% within 90d: {}\n",
            DEPTHS
                .iter()
                .map(|d| {
                    let hits = r_
                        .iter()
                        .filter(|r| {
                            r.get("fills")
                                .and_then(|f| {
                                    f.items()
                                        .iter()
                                        .find(|(k, _)| *k == Py::Int(*d))
                                        .map(|x| x.1.truthy())
                                })
                                .unwrap_or(false)
                        })
                        .count();
                    format!("-{d}:{}", round_half_even(100.0 * hits as f64 / n as f64))
                })
                .collect::<Vec<_>>()
                .join(" ")
        ));
        let lad = |r: &Py, s: &str| r.get("ladder").map(|l| g(l, s)).unwrap_or(f64::NAN);
        o.push_str(&format!(
            "  ladder ret90 by market share (median | worst | mean): {}\n",
            SHARES
                .iter()
                .map(|s| {
                    let k = share_key(*s);
                    let xs: Vec<f64> = r_.iter().map(|r| lad(r, &k)).collect();
                    let label = if *s == 0.0 { "0".to_string() } else { repr(*s) };
                    format!(
                        "m{label}: {}/{}/{}",
                        med(&xs).repr(),
                        repr(round(minf(&xs), 1)),
                        repr(round(mean(&xs), 1))
                    )
                })
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        let first_depth = coins
            .iter()
            .find(|c| c.sym == sym)
            .map(|c| c.depths[0])
            .unwrap_or(f64::NAN);
        o.push_str(&format!(
            "  ladder beats market(m=1) in {}/{n} events; no rung filled in {}/{n}\n",
            r_.iter().filter(|r| lad(r, "0") > lad(r, "1.0")).count(),
            r_.iter().filter(|r| g(r, "maxdd90") > -first_depth).count()
        ));
        for (key, lab) in [
            ("g17", "gate@D+17 -> ret to D+90 (73d)"),
            ("g3", "gate@D+3=flip+17 (today analog) -> ret to D+90 (87d)"),
        ] {
            let gs: Vec<&Py> = r_
                .iter()
                .filter(|r| r.get(key).is_some_and(|x| x.truthy()))
                .collect();
            let gn = |r: &&Py| g(r.get(key).unwrap(), "n");
            let groups: [(&str, Vec<&Py>); 3] = [
                (
                    "RUN(>=3)",
                    gs.iter().copied().filter(|r| gn(r) >= 3.0).collect(),
                ),
                ("2/4", gs.iter().copied().filter(|r| gn(r) == 2.0).collect()),
                (
                    "0-1/4",
                    gs.iter().copied().filter(|r| gn(r) <= 1.0).collect(),
                ),
            ];
            let parts: Vec<String> = groups
                .iter()
                .map(|(gname, l)| {
                    if l.is_empty() {
                        return format!("{gname}: n=0");
                    }
                    let a: Vec<f64> = l
                        .iter()
                        .map(|x| g(x.get(key).unwrap(), "ret_to90"))
                        .collect();
                    let b: Vec<f64> = l
                        .iter()
                        .map(|x| g(x.get(key).unwrap(), "btc_ret_to90"))
                        .collect();
                    let rr: Vec<f64> = a.iter().zip(&b).map(|(x, y)| x - y).collect();
                    format!(
                        "{gname}: n={} alt med={} (min {}) btc med={} rel med={} beat={}/{}",
                        l.len(),
                        med(&a).repr(),
                        repr(round(minf(&a), 1)),
                        med(&b).repr(),
                        med(&rr).repr(),
                        rr.iter().filter(|x| **x > 0.0).count(),
                        l.len()
                    )
                })
                .collect();
            o.push_str(&format!("  {lab}: {}\n", parts.join(" || ")));
        }
        let gcell = |r: &Py, k: &str| match r.get(k) {
            Some(x) if x.truthy() => x.get("n").unwrap().repr(),
            _ => "-".into(),
        };
        o.push_str(&format!(
            "  per-event: {}\n",
            r_.iter()
                .map(|r| format!(
                    "{}[{}] r90={} btc={} dd={} g17={} g3={}",
                    r.get("date").unwrap().to_py_string(),
                    prev2(r),
                    round_half_even(g(r, "ret90")),
                    round_half_even(g(r, "btc_ret90")),
                    round_half_even(g(r, "maxdd90")),
                    gcell(r, "g17"),
                    gcell(r, "g3")
                ))
                .collect::<Vec<_>>()
                .join("; ")
        ));
        let v5 = c.get("vol").unwrap();
        let vx = |x: i64, k: &str| {
            v5.items()
                .iter()
                .find(|(kk, _)| *kk == Py::Int(x))
                .and_then(|(_, v)| v.get(k).cloned())
                .unwrap_or(Py::None)
                .repr()
        };
        o.push_str(&format!(
            "  rung1 odds: {}\n",
            [2i64, 3, 5, 8, 10]
                .iter()
                .map(|x| format!(
                    "-{x}%: days(low<=prevC*(1-x)) 30/60/90d={}/{}/{} P(fill<=14d) over last 90/180/365 starts={}/{}/{}%",
                    vx(*x, "days_low_le_30"),
                    vx(*x, "days_low_le_60"),
                    vx(*x, "days_low_le_90"),
                    vx(*x, "p14_90"),
                    vx(*x, "p14_180"),
                    vx(*x, "p14_365")
                ))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    o
}

/// Both variants: the console report and the `results.json` document.
pub fn run(btc_rows: &[Bar], coins: &[AltCoin], load: &dyn Fn(&str) -> Vec<Bar>) -> (String, Py) {
    let mut o = String::new();
    let mut results = Py::dict();
    for v in ["A", "B"] {
        let out = analyze(v, btc_rows, coins, load);
        o.push_str(&summarize(&out, coins));
        results.set(v, out);
    }
    (o, results)
}

// ------------------------------------------------------------------ profile sweep

/// A depth/weight profile for the sweep.
pub type Profile = (String, [f64; 3], [f64; 3]);

pub fn default_profiles() -> Vec<Profile> {
    let p = |n: &str, d: [f64; 3], w: [f64; 3]| (n.to_string(), d, w);
    vec![
        p("bull 3/7/12 30-40-30", [3.0, 7.0, 12.0], [30.0, 40.0, 30.0]),
        p(
            "chop 5/10/18 30-40-30 (LIVE alts)",
            [5.0, 10.0, 18.0],
            [30.0, 40.0, 30.0],
        ),
        p(
            "ALT 2/5/9 30-40-30 (LIVE ALT)",
            [2.0, 5.0, 9.0],
            [30.0, 40.0, 30.0],
        ),
        p(
            "BTC 4/8/14 25-35-40 (LIVE BTC)",
            [4.0, 8.0, 14.0],
            [25.0, 35.0, 40.0],
        ),
        p("5/12/20 30-40-30", [5.0, 12.0, 20.0], [30.0, 40.0, 30.0]),
        p("8/15/20 30-40-30", [8.0, 15.0, 20.0], [30.0, 40.0, 30.0]),
        p("10/15/20 20-40-40", [10.0, 15.0, 20.0], [20.0, 40.0, 40.0]),
        p("5/10/18 20-30-50", [5.0, 10.0, 18.0], [20.0, 30.0, 50.0]),
        p("5/10/18 50-30-20", [5.0, 10.0, 18.0], [50.0, 30.0, 20.0]),
        p(
            "old Oct AKT 12/28/45 -> capped 12/20/20",
            [12.0, 20.0, 20.0],
            [30.0, 40.0, 30.0],
        ),
    ]
}

fn profile_outcome(dd: f64, ret: f64, depths: &[f64; 3], weights: &[f64; 3], m: f64) -> (f64, f64) {
    let (low_min, c90) = (1.0 + dd / 100.0, 1.0 + ret / 100.0);
    let (mut cash, mut coins, mut deployed) = (1.0 - m, m, m);
    for (d, w) in depths.iter().zip(weights) {
        let a = (1.0 - m) * w / 100.0;
        let px = 1.0 - d / 100.0;
        if low_min <= px {
            coins += a / px;
            cash -= a;
            deployed += a;
        }
    }
    ((coins * c90 + cash - 1.0) * 100.0, deployed)
}

/// Depth/weight profile sweep over variant A's events. A $1 ladder's outcome depends
/// only on the 90-day drawdown and return from entry; also reports the deployed share.
pub fn profile_sweep(results: &Py, profiles: &[Profile]) -> String {
    let mut o = String::new();
    let coins = results
        .get("A")
        .and_then(|a| a.get("coins"))
        .cloned()
        .unwrap_or(Py::None);
    for (sym, c) in coins.items() {
        let sym = sym.to_py_string();
        let ev = c.get("events").unwrap().as_list();
        if ev.len() < 2 {
            o.push_str(&format!("\n{sym}: n={} (too few)\n", ev.len()));
            continue;
        }
        let r90: Vec<f64> = ev.iter().map(|e| g(e, "ret90")).collect();
        let dd: Vec<f64> = ev.iter().map(|e| g(e, "maxdd90")).collect();
        o.push_str(&format!(
            "\n{sym} n={} (ret90 med {}, maxdd90 med {})\n",
            ev.len(),
            repr(round(median(&r90), 1)),
            repr(round(median(&dd), 1))
        ));
        o.push_str(&format!(
            "  {} {} {} {} {} {} {} {}\n",
            fs("profile", "40"),
            fs("m", ">4"),
            fs("median", ">7"),
            fs("mean", ">7"),
            fs("worst", ">7"),
            fs("best", ">7"),
            fs("deployed_med", ">12"),
            fs("beats_mkt", ">9")
        ));
        for (name, d, w) in profiles {
            for m in [0.0, 0.25] {
                let res: Vec<(f64, f64)> = ev
                    .iter()
                    .map(|e| profile_outcome(g(e, "maxdd90"), g(e, "ret90"), d, w, m))
                    .collect();
                let r: Vec<f64> = res.iter().map(|x| x.0).collect();
                let dep: Vec<f64> = res.iter().map(|x| x.1).collect();
                let beats = r.iter().zip(&r90).filter(|(a, b)| a > b).count();
                o.push_str(&format!(
                    "  {} {} {} {} {} {} {} {}/{}\n",
                    fs(name, "40"),
                    ff(m, ">4"),
                    ff(median(&r), ">7.1f"),
                    ff(mean(&r), ">7.1f"),
                    ff(minf(&r), ">7.1f"),
                    ff(maxf(&r), ">7.1f"),
                    ff(median(&dep), ">12.2f"),
                    fi(beats as i64, ">5"),
                    ev.len()
                ));
            }
        }
    }
    o
}

// ------------------------------------------------------------------ current read

/// A coin's live resting rungs against the spot they were set at.
#[derive(Debug, Clone)]
pub struct LiveRungs {
    pub sym: String,
    pub source: String,
    pub rungs: Vec<f64>,
    pub spot: f64,
}

fn p_fill(rows: &[Bar], dist_pct: f64, h: i64, n: i64) -> i64 {
    let len = rows.len();
    let mut hits = 0;
    for t in (len as i64 - n - h)..(len as i64 - h) {
        let lm = minf(
            &rows[slice(len, t + 1, t + 1 + h)]
                .iter()
                .map(|r| r.low)
                .collect::<Vec<_>>(),
        );
        if lm <= rows[at(len, t)].close * (1.0 + dist_pct / 100.0) {
            hits += 1;
        }
    }
    round_half_even(hits as f64 / n as f64 * 100.0)
}

/// Current-event read: moves since the last bear close and since the confirmation, and
/// the odds each live rung fills within 14/30/90 days from the trailing 365 starts.
pub fn current(
    coins: &[LiveRungs],
    load: &dyn Fn(&str) -> Vec<Bar>,
    last_bear_close: &str,
    confirmed: &str,
) -> String {
    let mut o = String::new();
    let flip_next = shift(last_bear_close, 1);
    for c in coins {
        let rows = load(&c.source);
        let m: BTreeMap<&str, &Bar> = rows.iter().map(|r| (r.date.as_str(), r)).collect();
        let c_flip = m[last_bear_close].close;
        let c_conf = m[confirmed].close;
        let c_now = rows.last().expect("rows").close;
        let since: Vec<&Bar> = rows
            .iter()
            .filter(|r| r.date.as_str() >= flip_next.as_str())
            .collect();
        let low_since = minf(&since.iter().map(|r| r.low).collect::<Vec<_>>());
        let hi_since = maxf(&since.iter().map(|r| r.high).collect::<Vec<_>>());
        let dists: Vec<f64> = c
            .rungs
            .iter()
            .map(|p| round((p / c.spot - 1.0) * 100.0, 1))
            .collect();
        let ps: Vec<Py> = dists
            .iter()
            .map(|d| {
                Py::from(
                    [(14, 365), (30, 365), (90, 365)]
                        .iter()
                        .map(|(h, n)| p_fill(&rows, *d, *h, *n).to_string())
                        .collect::<Vec<_>>()
                        .join("/"),
                )
            })
            .collect();
        let pct = |a: f64, b: f64| ff(round((a / b - 1.0) * 100.0, 1), "+.1f");
        o.push_str(&format!(
            "{} since last-bear-close {}: {}%  since confirm {}: {}%  range since flip: low {}% / high {}% | rung dists {} -> P(fill) within 14/30/90d over last 365 starts: {}\n",
            fs(&c.sym, "5"),
            &last_bear_close[5..],
            pct(c_now, c_flip),
            &confirmed[5..],
            pct(c_now, c_conf),
            pct(low_since, c_flip),
            pct(hi_since, c_flip),
            Py::List(dists.iter().map(|d| Py::Float(*d)).collect()).repr(),
            Py::List(ps).repr()
        ));
    }
    o
}
