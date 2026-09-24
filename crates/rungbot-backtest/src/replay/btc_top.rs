//! Anatomy of BTC cycle tops, and what simple exit rules would have captured.
//!
//! One merged daily series: Binance closes where they exist, Coin Metrics before that
//! (aligned by whichever day shift fits best), forward-filled across gaps. On it:
//!
//! * each cycle top and the leg from the preceding cycle's low: final-month gains, how
//!   long price sat near the top, how fast it fell, the pullbacks inside the leg;
//! * top indicators (Pi-cycle, Mayer multiple, weekly RSI, 30-day gain, multiple of the
//!   200-week average, Fear & Greed) — when each first fired before each top, and how
//!   often it fired falsely;
//! * trailing stops from the leg's low (re-entering on a new high), trailing stops armed
//!   late, and selling a tenth of the remainder on every +X% on the way up.

use std::collections::BTreeMap;

use crate::py::{ff, repr, round, round_half_even, sum, Py};
use crate::pydict;
use crate::replay::data::{day_num, day_of_epoch, day_str, iso_week, shift};

/// Inputs: Binance daily closes, Coin Metrics daily prices, and the Fear & Greed index.
pub struct Inputs {
    /// `(date, close)` from Binance, oldest first, last partial candle already dropped.
    pub binance: Vec<(String, f64)>,
    /// `(date, price)` from Coin Metrics.
    pub coinmetrics: Vec<(String, f64)>,
    /// `(epoch_seconds, value)` Fear & Greed readings.
    pub fng: Vec<(i64, i64)>,
}

/// The cycle tops studied: label and the window searched for the top close.
pub const SPEC: [(&str, &str, &str); 6] = [
    ("2013-04", "2013-03-01", "2013-05-31"),
    ("2013-12", "2013-10-01", "2014-01-31"),
    ("2017-12", "2017-11-01", "2018-01-31"),
    ("2021-04", "2021-03-01", "2021-05-31"),
    ("2021-11", "2021-10-01", "2021-12-31"),
    ("2025", "2024-01-01", ""),
];

fn rolling_sma(arr: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; arr.len()];
    let mut s = 0.0f64;
    for (i, v) in arr.iter().enumerate() {
        s += v;
        if i >= n {
            s -= arr[i - n];
        }
        if i + 1 >= n {
            out[i] = Some(s / n as f64);
        }
    }
    out
}

fn rsi_series(vals: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; vals.len()];
    let (mut ag, mut al): (Option<f64>, Option<f64>) = (None, None);
    let nf = n as f64;
    let rsi = |ag: f64, al: f64| 100.0 - 100.0 / (1.0 + if al != 0.0 { ag / al } else { 1e9 });
    for k in 1..vals.len() {
        let ch = vals[k] - vals[k - 1];
        let (g, l) = (ch.max(0.0), (-ch).max(0.0));
        if k <= n {
            ag = Some(ag.unwrap_or(0.0) + g / nf);
            al = Some(al.unwrap_or(0.0) + l / nf);
            if k == n {
                out[k] = Some(rsi(ag.unwrap(), al.unwrap()));
            }
        } else {
            let a = (ag.unwrap() * (nf - 1.0) + g) / nf;
            let b = (al.unwrap() * (nf - 1.0) + l) / nf;
            ag = Some(a);
            al = Some(b);
            out[k] = Some(rsi(a, b));
        }
    }
    out
}

fn opt_round(v: Option<f64>, n: i32) -> Py {
    match v {
        Some(x) if x != 0.0 => Py::Float(round(x, n)),
        _ => Py::None,
    }
}

struct S {
    closes: Vec<f64>,
    idx: BTreeMap<String, usize>,
}

impl S {
    fn argmax(&self, a: &str, b: &str) -> usize {
        let (i0, i1) = (self.idx[a], self.idx[b]);
        let mut best = i0;
        for i in i0..=i1 {
            if self.closes[i] > self.closes[best] {
                best = i;
            }
        }
        best
    }
    fn argmin_i(&self, i0: usize, i1: usize) -> usize {
        let mut best = i0;
        for i in i0..=i1 {
            if self.closes[i] < self.closes[best] {
                best = i;
            }
        }
        best
    }
}

struct Top {
    label: String,
    t: usize,
    bot: usize,
}

/// Pullbacks inside `[i0, i1]` on closes: `(peak_i, trough_i, depth, new_high_i)`.
fn pullbacks(c: &[f64], i0: usize, i1: usize) -> Vec<(usize, usize, f64, Option<usize>)> {
    let mut out = Vec::new();
    let (mut peak, mut peak_i) = (c[i0], i0);
    let (mut trough, mut trough_i): (Option<f64>, Option<usize>) = (None, None);
    for (i, &ci) in c.iter().enumerate().take(i1 + 1).skip(i0 + 1) {
        if ci >= peak {
            if let (Some(tr), Some(ti)) = (trough, trough_i) {
                out.push((peak_i, ti, 1.0 - tr / peak, Some(i)));
            }
            peak = ci;
            peak_i = i;
            trough = None;
            trough_i = None;
        } else if trough.is_none_or(|t| ci < t) {
            trough = Some(ci);
            trough_i = Some(i);
        }
    }
    if let (Some(tr), Some(ti)) = (trough, trough_i) {
        out.push((peak_i, ti, 1.0 - tr / peak, None));
    }
    out
}

pub fn run(inp: &Inputs) -> (String, Py) {
    let bin_map: BTreeMap<&str, f64> = inp.binance.iter().map(|(d, c)| (d.as_str(), *c)).collect();
    let mut cm_map: BTreeMap<String, f64> = BTreeMap::new();
    for (d, v) in &inp.coinmetrics {
        cm_map.insert(d[..10].to_string(), *v);
    }

    // Which day shift aligns Coin Metrics with Binance best.
    let mut align: Vec<(i64, f64)> = Vec::new();
    for sh in [-1i64, 0, 1] {
        let e: Vec<f64> = inp
            .binance
            .iter()
            .filter_map(|(d, c)| cm_map.get(&shift(d, sh)).map(|v| (v / c - 1.0).abs()))
            .collect();
        align.push((sh, sum(e.iter().copied()) / e.len() as f64));
    }
    let sh_best = align
        .iter()
        .fold(None, |b: Option<(i64, f64)>, x| match b {
            Some(bb) if bb.1 <= x.1 => Some(bb),
            _ => Some(*x),
        })
        .expect("three shifts")
        .0;

    let mut dates = Vec::new();
    let mut closes = Vec::new();
    let mut src = Vec::new();
    let end = inp
        .binance
        .iter()
        .map(|x| x.0.as_str())
        .max()
        .expect("binance rows")
        .to_string();
    let mut d = day_num(cm_map.keys().next().expect("coinmetrics rows"));
    let end_n = day_num(&end);
    let mut last = f64::NAN;
    while d <= end_n {
        let ds = day_str(d);
        let (v, s) = if let Some(v) = bin_map.get(ds.as_str()) {
            (*v, "binance")
        } else if let Some(v) = cm_map.get(&day_str(d + sh_best)) {
            (*v, "coinmetrics")
        } else {
            (last, "ffill")
        };
        dates.push(ds);
        closes.push(v);
        src.push(s);
        last = v;
        d += 1;
    }
    let idx: BTreeMap<String, usize> = dates
        .iter()
        .enumerate()
        .map(|(i, d)| (d.clone(), i))
        .collect();
    let n = closes.len();
    let s = S {
        closes: closes.clone(),
        idx,
    };
    let c = &closes;

    // ---- tops + legs
    let mut prev_top = s.argmax("2011-05-01", "2011-07-31");
    let mut tops: Vec<Top> = Vec::new();
    let mut tops_py = Vec::new();
    for (label, a, b) in SPEC {
        let b = if b.is_empty() { end.as_str() } else { b };
        let t = s.argmax(a, b);
        let bot = s.argmin_i(prev_top, t);
        tops_py.push(pydict![
            ("label", label),
            ("date", dates[t].as_str()),
            ("close", c[t]),
            ("source", src[t]),
            ("bottom_date", dates[bot].as_str()),
            ("bottom_close", c[bot]),
            ("leg_days", t - bot),
            ("leg_multiple", c[t] / c[bot]),
            ("prev_top_date", dates[prev_top].as_str())
        ]);
        tops.push(Top {
            label: label.into(),
            t,
            bot,
        });
        prev_top = t;
    }
    let top_idx: Vec<usize> = tops.iter().map(|x| x.t).collect();

    // ---- indicator series
    let sma111 = rolling_sma(c, 111);
    let sma200 = rolling_sma(c, 200);
    let sma350 = rolling_sma(c, 350);
    let pi: Vec<bool> = (0..n)
        .map(|i| matches!((sma111[i], sma350[i]), (Some(a), Some(b)) if a >= 2.0 * b))
        .collect();
    let mayer: Vec<Option<f64>> = (0..n)
        .map(|i| sma200[i].filter(|v| *v != 0.0).map(|m| c[i] / m))
        .collect();
    let gain = |k: usize| -> Vec<Option<f64>> {
        (0..n)
            .map(|i| {
                if i >= k {
                    Some(c[i] / c[i - k] - 1.0)
                } else {
                    None
                }
            })
            .collect()
    };
    let g30 = gain(30);

    let mut wk: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    for (i, ds) in dates.iter().enumerate() {
        wk.insert(iso_week(day_num(ds)), i);
    }
    let wk_i: Vec<usize> = wk.values().copied().collect();
    let wk_close: Vec<f64> = wk_i.iter().map(|i| c[*i]).collect();
    let wk_rsi = rsi_series(&wk_close, 14);
    let wk_wma200 = rolling_sma(&wk_close, 200);
    let mut rsi_d: Vec<Option<f64>> = vec![None; n];
    for (k, i) in wk_i.iter().enumerate() {
        rsi_d[*i] = wk_rsi[k];
    }
    let wkset: BTreeMap<usize, usize> = wk_i.iter().enumerate().map(|(k, i)| (*i, k)).collect();
    let mut wma_ff: Vec<Option<f64>> = vec![None; n];
    let mut cur = None;
    for (i, slot) in wma_ff.iter_mut().enumerate() {
        if let Some(k) = wkset.get(&i) {
            if wk_wma200[*k].is_some() {
                cur = wk_wma200[*k];
            }
        }
        *slot = cur;
    }
    let wma_mult: Vec<Option<f64>> = (0..n)
        .map(|i| wma_ff[i].filter(|v| *v != 0.0).map(|w| c[i] / w))
        .collect();

    let mut fng_map: BTreeMap<String, i64> = BTreeMap::new();
    for (ts, v) in &inp.fng {
        fng_map.insert(day_of_epoch(*ts), *v);
    }
    let fng_d: Vec<Option<f64>> = dates
        .iter()
        .map(|d| fng_map.get(d).map(|v| *v as f64))
        .collect();

    let cond = |series: &[Option<f64>], thr: f64| -> Vec<bool> {
        series.iter().map(|v| v.is_some_and(|x| x >= thr)).collect()
    };
    let indicators: Vec<(&str, Vec<bool>, usize)> = vec![
        ("pi_cycle_111dma_ge_2x350dma", pi.clone(), 10),
        ("mayer_ge_2.0", cond(&mayer, 2.0), 10),
        ("mayer_ge_2.4", cond(&mayer, 2.4), 10),
        ("weekly_rsi14_ge_80", cond(&rsi_d, 80.0), 21),
        ("weekly_rsi14_ge_85", cond(&rsi_d, 85.0), 21),
        ("gain30d_ge_40pct", cond(&g30, 0.40), 10),
        ("gain30d_ge_60pct", cond(&g30, 0.60), 10),
        ("close_ge_3x_200wma", cond(&wma_mult, 3.0), 10),
        ("close_ge_4x_200wma", cond(&wma_mult, 4.0), 10),
        ("close_ge_5x_200wma", cond(&wma_mult, 5.0), 10),
        ("fng_ge_85", cond(&fng_d, 85.0), 10),
        ("fng_ge_90", cond(&fng_d, 90.0), 10),
    ];

    let episodes = |cv: &[bool], gap: usize| -> Vec<usize> {
        let mut fires = Vec::new();
        let mut last_true: Option<usize> = None;
        for (i, on) in cv.iter().enumerate() {
            if *on {
                if last_true.is_none_or(|l| i - l > gap) {
                    fires.push(i);
                }
                last_true = Some(i);
            }
        }
        fires
    };
    // ('TOP', t) | ('FALSE', None) | ('LOCAL', None)
    let classify = |f: usize| -> (&'static str, Option<usize>) {
        let near: Vec<(usize, usize)> = top_idx
            .iter()
            .filter(|t| **t as i64 - 180 <= f as i64 && f <= **t + 30)
            .map(|t| ((f as i64 - *t as i64).unsigned_abs() as usize, *t))
            .collect();
        if let Some(m) = near.iter().min() {
            return ("TOP", Some(m.1));
        }
        let fwd = &c[(f + 1).min(n)..(f + 181).min(n)];
        if !fwd.is_empty() && fwd.iter().cloned().fold(f64::NEG_INFINITY, f64::max) > c[f] * 1.10 {
            ("FALSE", None)
        } else {
            ("LOCAL", None)
        }
    };

    let mut ind_results = Py::dict();
    for (name, cv, gap) in &indicators {
        let fires = episodes(cv, *gap);
        let avail_from = (0..n)
            .find(|&i| {
                (name.starts_with("pi") && sma350[i].is_some())
                    || (name.starts_with("mayer") && mayer[i].is_some())
                    || (name.starts_with("weekly") && rsi_d[i].is_some())
                    || (name.starts_with("gain30") && g30[i].is_some())
                    || (name.starts_with("close_ge") && wma_mult[i].is_some())
                    || (name.starts_with("fng") && fng_d[i].is_some())
            })
            .map(|i| dates[i].clone());
        let mut per_top = Py::dict();
        for x in &tops {
            let t = x.t;
            let tf: Vec<usize> = fires
                .iter()
                .copied()
                .filter(|f| classify(*f) == ("TOP", Some(t)))
                .collect();
            let pct_of_top = |f: usize| round(c[f] / c[t] * 100.0, 1);
            per_top.set(
                x.label.as_str(),
                pydict![
                    ("fires", tf.len()),
                    ("first_fire", tf.first().map(|f| dates[*f].as_str())),
                    ("first_lead_days", tf.first().map(|f| t as i64 - *f as i64)),
                    (
                        "first_fire_price_pct_of_top",
                        tf.first().map(|f| pct_of_top(*f))
                    ),
                    ("last_fire", tf.last().map(|f| dates[*f].as_str())),
                    ("last_lead_days", tf.last().map(|f| t as i64 - *f as i64)),
                    (
                        "last_fire_price_pct_of_top",
                        tf.last().map(|f| pct_of_top(*f))
                    ),
                    ("on_at_top", cv[t]),
                    (
                        "available",
                        avail_from
                            .as_ref()
                            .is_some_and(|a| a.as_str() <= dates[t].as_str())
                    )
                ],
            );
        }
        let false_f: Vec<Py> = fires
            .iter()
            .filter(|f| classify(**f).0 == "FALSE")
            .map(|f| Py::from(dates[*f].as_str()))
            .collect();
        let local_f: Vec<Py> = fires
            .iter()
            .filter(|f| classify(**f).0 == "LOCAL")
            .map(|f| Py::from(dates[*f].as_str()))
            .collect();
        ind_results.set(
            *name,
            pydict![
                ("available_from", avail_from),
                ("total_fires", fires.len()),
                ("per_top", per_top),
                ("false_fires", false_f.len()),
                ("false_fire_dates", Py::List(false_f)),
                ("local_top_fires", local_f.len()),
                ("local_fire_dates", Py::List(local_f)),
                ("on_now", cv[n - 1])
            ],
        );
    }

    // ---- part 1: anatomy
    let fmt_pb = |p: &(usize, usize, f64, Option<usize>)| {
        pydict![
            ("peak", dates[p.0].as_str()),
            ("trough", dates[p.1].as_str()),
            ("depth_pct", round(p.2 * 100.0, 1)),
            ("days_peak_to_trough", p.1 - p.0),
            ("days_trough_to_new_high", p.3.map(|nh| nh - p.1))
        ]
    };
    let mut anatomy = Vec::new();
    for (x, tp) in tops.iter().zip(&tops_py) {
        let (t, bot, top) = (x.t, x.bot, c[x.t]);
        let mut a = pydict![
            ("label", x.label.as_str()),
            ("top_date", dates[t].as_str()),
            ("top_close", round(top, 2)),
            ("source", src[t]),
            ("leg_from", dates[bot].as_str()),
            ("leg_bottom_close", round(c[bot], 2)),
            (
                "leg_multiple",
                round(tp.get("leg_multiple").unwrap().as_f64().unwrap(), 2)
            ),
            ("leg_days", t - bot)
        ];
        for k in [30usize, 60, 90, 180, 365] {
            a.set(
                format!("gain_final_{k}d_pct"),
                round((top / c[t - k] - 1.0) * 100.0, 1),
            );
        }
        let (lo, hi) = (t.saturating_sub(120), (t + 120).min(n - 1));
        for (pct, thr) in [(10, 0.9), (15, 0.85)] {
            let w: Vec<usize> = (lo..=hi).filter(|i| c[*i] >= thr * top).collect();
            a.set(format!("days_within_{pct}pct_of_top"), w.len());
            a.set(
                format!("days_within_{pct}pct_before"),
                w.iter().filter(|i| **i < t).count(),
            );
            a.set(
                format!("days_within_{pct}pct_after"),
                w.iter().filter(|i| **i > t).count(),
            );
            a.set(
                format!("within_{pct}pct_window"),
                format!(
                    "{}..{}",
                    dates[w[0]],
                    dates[*w.last().expect("the top itself")]
                ),
            );
        }
        for pct in [20.0, 30.0, 50.0] {
            let j = (t + 1..n).find(|i| c[*i] <= top * (1.0 - pct / 100.0));
            a.set(format!("days_top_to_minus{}", pct as i64), j.map(|j| j - t));
            a.set(
                format!("date_minus{}", pct as i64),
                j.map(|j| dates[j].as_str()),
            );
        }
        a.set(
            "close_30d_after_pct_of_top",
            round(c[(t + 30).min(n - 1)] / top * 100.0, 1),
        );
        a.set(
            "close_60d_after_pct_of_top",
            round(c[(t + 60).min(n - 1)] / top * 100.0, 1),
        );
        let pbs = pullbacks(c, bot, t);
        let mut sorted = pbs.clone();
        sorted.sort_by(|p, q| q.2.partial_cmp(&p.2).unwrap_or(std::cmp::Ordering::Equal));
        a.set(
            "largest_nontop_pullback_in_leg",
            sorted.first().map(fmt_pb).unwrap_or(Py::None),
        );
        a.set(
            "top3_pullbacks_in_leg",
            Py::List(sorted.iter().take(3).map(fmt_pb).collect()),
        );
        for win in [365usize, 180, 90] {
            let late: Vec<&(usize, usize, f64, Option<usize>)> = pbs
                .iter()
                .filter(|p| p.0 as i64 >= t as i64 - win as i64)
                .collect();
            let best = late.iter().fold(
                None,
                |b: Option<&&(usize, usize, f64, Option<usize>)>, p| match b {
                    Some(bb) if bb.2 >= p.2 => Some(bb),
                    _ => Some(p),
                },
            );
            a.set(
                format!("largest_pullback_final_{win}d"),
                best.map(|p| fmt_pb(p)).unwrap_or(Py::None),
            );
        }
        // range(t, max(0, t - 7), -1): t down to max(0,t-7)+1
        let wrsi = {
            let stop = t.saturating_sub(7);
            let mut v = None;
            let mut i = t as i64;
            while i > stop as i64 {
                if let Some(r) = rsi_d[i as usize] {
                    v = Some(r);
                    break;
                }
                i -= 1;
            }
            round(v.unwrap_or(0.0), 1)
        };
        a.set(
            "at_top",
            pydict![
                ("mayer", opt_round(mayer[t], 2)),
                ("weekly_rsi", wrsi),
                ("gain30", g30[t].map(|g| round(g * 100.0, 1))),
                ("x_200wma", opt_round(wma_mult[t], 2)),
                ("pi_cycle_on", pi[t]),
                ("fng", fng_d[t].map(|v| v as i64)),
                (
                    "sma111_over_2x350",
                    match (sma111[t], sma350[t]) {
                        (Some(a1), Some(b1)) if b1 != 0.0 => Py::Float(round(a1 / (2.0 * b1), 3)),
                        _ => Py::None,
                    }
                )
            ],
        );
        if fng_d[t].is_some() {
            let win: Vec<f64> = (t.saturating_sub(90)..(t + 31).min(n))
                .filter_map(|i| fng_d[i])
                .collect();
            let pm30 = (t.saturating_sub(30)..(t + 31).min(n))
                .filter_map(|i| fng_d[i])
                .fold(f64::NEG_INFINITY, f64::max);
            a.set("fng_max_pm30", pm30 as i64);
            a.set(
                "fng_days_ge85_t-90..t+30",
                win.iter().filter(|v| **v >= 85.0).count(),
            );
            a.set(
                "fng_days_ge90_t-90..t+30",
                win.iter().filter(|v| **v >= 90.0).count(),
            );
        }
        anatomy.push(a);
    }

    // ---- part 2: trailing stops + selling on the way up
    let trail = |bot: usize, t: usize, gi: i64, arm_i: Option<usize>| -> Py {
        let gv = gi as f64 / 100.0;
        let mut entry_i = bot;
        let mut peak = c[bot];
        let mut in_pos = true;
        let mut armed = arm_i.is_none();
        let mut mult = 1.0f64;
        let mut false_stops: Vec<String> = Vec::new();
        let mut whipsaw = 0.0f64;
        let mut ex_price = f64::NAN;
        let mut fin: Option<usize> = None;
        for i in bot + 1..n {
            let ci = c[i];
            if in_pos {
                if ci > peak {
                    peak = ci;
                }
                if arm_i.is_some_and(|a| i >= a) {
                    armed = true;
                }
                if armed && ci <= peak * (1.0 - gv) {
                    mult *= ci / c[entry_i];
                    in_pos = false;
                    ex_price = ci;
                    if i <= t {
                        false_stops.push(dates[i].clone());
                    } else {
                        fin = Some(i);
                        break;
                    }
                }
            } else if ci > peak {
                whipsaw += ci / ex_price - 1.0;
                in_pos = true;
                entry_i = i;
                peak = ci;
            }
        }
        if fin.is_none() && in_pos {
            mult *= c[n - 1] / c[entry_i];
        }
        let m50 = (t + 1..n).find(|i| c[*i] <= 0.5 * c[t]);
        let fin = fin.filter(|f| *f != 0);
        pydict![
            ("giveback_pct", (gv * 100.0) as i64),
            ("false_stops", false_stops.len()),
            (
                "false_stop_dates",
                Py::List(false_stops.iter().map(|d| Py::from(d.as_str())).collect())
            ),
            ("whipsaw_cost_pct", round(whipsaw * 100.0, 1)),
            (
                "final_exit_date",
                fin.map(|f| dates[f].clone())
                    .unwrap_or_else(|| "OPEN".into())
            ),
            (
                "final_exit_days_after_top",
                fin.map(|f| f as i64 - t as i64)
            ),
            (
                "exit_pct_of_top",
                fin.map(|f| round(c[f] / c[t] * 100.0, 1))
            ),
            ("captured_multiple", round(mult, 2)),
            ("bh_to_top_multiple", round(c[t] / c[bot], 2)),
            (
                "hold_to_minus50_multiple",
                m50.filter(|m| *m != 0).map(|m| round(c[m] / c[bot], 2))
            ),
            (
                "captured_pct_of_bh_top",
                round(mult / (c[t] / c[bot]) * 100.0, 1)
            )
        ]
    };
    let sell_up = |bot: usize, t: usize, x: f64| -> Py {
        let frac = 0.10;
        let p0 = c[bot];
        let (mut rem, mut realized) = (1.0f64, 0.0f64);
        let mut nxt = p0 * (1.0 + x);
        let mut sales: Vec<Py> = Vec::new();
        for i in bot + 1..=t {
            let ci = c[i];
            if ci >= nxt {
                realized += rem * frac * ci;
                rem -= rem * frac;
                sales.push(Py::Tuple(vec![
                    Py::from(dates[i].as_str()),
                    Py::Int(round_half_even(ci)),
                ]));
                nxt = ci * (1.0 + x);
            }
        }
        let top = c[t];
        pydict![
            ("step_pct", (x * 100.0) as i64),
            ("sales", sales.len()),
            ("sold_pct", round((1.0 - rem) * 100.0, 1)),
            (
                "avg_sale_pct_of_top",
                if rem < 1.0 {
                    Py::Float(round((realized / (1.0 - rem)) / top * 100.0, 1))
                } else {
                    Py::None
                }
            ),
            (
                "value_at_top_pct_of_bh",
                round((realized + rem * top) / top * 100.0, 1)
            ),
            (
                "value_at_minus50_pct_of_bh",
                round((realized + rem * 0.5 * top) / top * 100.0, 1)
            ),
            ("first_sale", sales.first().cloned().unwrap_or(Py::None)),
            ("last_sale", sales.last().cloned().unwrap_or(Py::None))
        ]
    };

    let mut capture = Vec::new();
    for (x, tp) in tops.iter().zip(&tops_py) {
        let (t, bot) = (x.t, x.bot);
        let lo = bot.max(t.saturating_sub(365));
        let arm_i = (lo..=t)
            .find(|i| {
                mayer[*i].is_some_and(|m| m >= 2.0)
                    || g30[*i].is_some_and(|g| g != 0.0 && g >= 0.40)
            })
            .filter(|a| *a != 0);
        let leg = format!(
            "{} ({}) -> {} ({}) = {}x",
            dates[bot],
            round_half_even(c[bot]),
            dates[t],
            round_half_even(c[t]),
            repr(round(tp.get("leg_multiple").unwrap().as_f64().unwrap(), 1))
        );
        capture.push(pydict![
            ("label", x.label.as_str()),
            ("leg", leg),
            (
                "trailing_from_bottom",
                Py::List(
                    [10, 15, 20, 25, 30, 35]
                        .iter()
                        .map(|g| trail(bot, t, *g, None))
                        .collect()
                )
            ),
            (
                "trailing_armed_late",
                pydict![
                    ("armed_on", arm_i.map(|a| dates[a].as_str())),
                    ("armed_lead_days", arm_i.map(|a| t as i64 - a as i64)),
                    (
                        "armed_price_pct_of_top",
                        arm_i.map(|a| round(c[a] / c[t] * 100.0, 1))
                    ),
                    (
                        "runs",
                        Py::List(match arm_i {
                            Some(a) => [10, 15, 20, 25, 30]
                                .iter()
                                .map(|g| trail(bot, t, *g, Some(a)))
                                .collect(),
                            None => Vec::new(),
                        })
                    )
                ]
            ),
            (
                "sell_on_way_up",
                Py::List(
                    [0.25, 0.50, 1.00]
                        .iter()
                        .map(|x| sell_up(bot, t, *x))
                        .collect()
                )
            )
        ]);
    }

    // ---- now
    let last = n - 1;
    let wrsi_now = (last.saturating_sub(7) + 1..=last)
        .rev()
        .find_map(|i| rsi_d[i])
        .expect("a weekly RSI in the last week");
    let now = pydict![
        ("date", dates[last].as_str()),
        ("close", c[last]),
        ("mayer", round(mayer[last].expect("mayer"), 2)),
        (
            "pi_ratio",
            round(
                sma111[last].expect("sma111") / (2.0 * sma350[last].expect("sma350")),
                3
            )
        ),
        ("gain30", round(g30[last].expect("gain30") * 100.0, 1)),
        ("weekly_rsi", round(wrsi_now, 1)),
        ("x_200wma", round(wma_mult[last].expect("200wma"), 2)),
        ("fng", fng_d[last].map(|v| v as i64)),
        ("sma200", round_half_even(sma200[last].expect("sma200"))),
        ("wma200", round_half_even(wma_ff[last].expect("wma200")))
    ];

    let align_py = Py::Dict(
        align
            .iter()
            .map(|(k, v)| (Py::Int(*k), Py::Float(*v)))
            .collect(),
    );
    let res = pydict![
        (
            "meta",
            pydict![
                ("binance_rows", inp.binance.len()),
                ("coinmetrics_rows", cm_map.len()),
                ("cm_shift_days", sh_best),
                ("cm_align_err", align_py.clone()),
                ("merged_days", n),
                ("merged_from", dates[0].as_str()),
                ("merged_to", dates[last].as_str()),
                ("fng_from", fng_map.keys().next().map(|s| s.as_str()))
            ]
        ),
        ("tops", Py::List(tops_py.clone())),
        ("anatomy", Py::List(anatomy.clone())),
        ("capture", Py::List(capture.clone())),
        ("indicators", ind_results.clone()),
        ("now", now.clone())
    ];

    // ---- stdout summary
    let mut o = String::new();
    let align_r = Py::Dict(
        align
            .iter()
            .map(|(k, v)| (Py::Int(*k), Py::Float(round(*v, 4))))
            .collect(),
    );
    o.push_str(&format!(
        "merge: cm shift {sh_best} {} N {n} {} .. {}\n",
        align_r.repr(),
        dates[0],
        dates[last]
    ));
    o.push_str("\n== TOPS ==\n");
    let s_ = |p: &Py, k: &str| {
        p.get(k)
            .map(|v| v.to_py_string())
            .unwrap_or_else(|| "None".into())
    };
    for a in &anatomy {
        let gf = |k: &str| a.get(k).unwrap().as_f64().unwrap();
        o.push_str(&format!(
            "{}: top {} ${} [{}] leg from {} ${} = {}x in {}d\n",
            s_(a, "label"),
            s_(a, "top_date"),
            ff(gf("top_close"), ",.0f"),
            s_(a, "source"),
            s_(a, "leg_from"),
            ff(gf("leg_bottom_close"), ",.0f"),
            s_(a, "leg_multiple"),
            s_(a, "leg_days")
        ));
        o.push_str(&format!(
            "   gain final 30/60/90/180d: {}/{}/{}/{}%  | 30d-after {}% of top\n",
            s_(a, "gain_final_30d_pct"),
            s_(a, "gain_final_60d_pct"),
            s_(a, "gain_final_90d_pct"),
            s_(a, "gain_final_180d_pct"),
            s_(a, "close_30d_after_pct_of_top")
        ));
        o.push_str(&format!(
            "   within10%: {}d ({} before/{} after) {} | within15%: {}d ({}/{}) {}\n",
            s_(a, "days_within_10pct_of_top"),
            s_(a, "days_within_10pct_before"),
            s_(a, "days_within_10pct_after"),
            s_(a, "within_10pct_window"),
            s_(a, "days_within_15pct_of_top"),
            s_(a, "days_within_15pct_before"),
            s_(a, "days_within_15pct_after"),
            s_(a, "within_15pct_window")
        ));
        o.push_str(&format!(
            "   days to -20/-30/-50: {}/{}/{} ({})\n",
            s_(a, "days_top_to_minus20"),
            s_(a, "days_top_to_minus30"),
            s_(a, "days_top_to_minus50"),
            s_(a, "date_minus50")
        ));
        let p = a.get("largest_nontop_pullback_in_leg").unwrap();
        let (p1, p9, p3) = (
            a.get("largest_pullback_final_180d").unwrap(),
            a.get("largest_pullback_final_90d").unwrap(),
            a.get("largest_pullback_final_365d").unwrap(),
        );
        o.push_str(&format!(
            "   largest non-top pullback in leg: {}% ({}->{}, {}d, new high +{}d)\n",
            s_(p, "depth_pct"),
            s_(p, "peak"),
            s_(p, "trough"),
            s_(p, "days_peak_to_trough"),
            s_(p, "days_trough_to_new_high")
        ));
        let dp = |p: &Py| {
            if p.truthy() {
                s_(p, "depth_pct")
            } else {
                "None".into()
            }
        };
        o.push_str(&format!(
            "   largest pullback final 365d: {}% | final 180d: {}% ({}) | final 90d: {}%\n",
            dp(p3),
            dp(p1),
            if p1.truthy() {
                s_(p1, "peak")
            } else {
                String::new()
            },
            dp(p9)
        ));
        let fng_part = match a.get("fng_max_pm30") {
            Some(v) if v.truthy() => format!(
                " | F&G max±30 {} days>=85 {} >=90 {}",
                v.to_py_string(),
                s_(a, "fng_days_ge85_t-90..t+30"),
                s_(a, "fng_days_ge90_t-90..t+30")
            ),
            _ => String::new(),
        };
        o.push_str(&format!(
            "   at top: {}{fng_part}\n",
            a.get("at_top").unwrap().repr()
        ));
    }
    o.push_str("\n== TRAILING STOP (from leg bottom, re-enter on new high) ==\n");
    for cp in &capture {
        o.push_str(&format!("{} {}\n", s_(cp, "label"), s_(cp, "leg")));
        for r in cp.get("trailing_from_bottom").unwrap().as_list() {
            o.push_str(&format!(
                "   gb{}: false {} (whipsaw {}%) exit {} +{}d @ {}% of top | captured {}x = {}% of B&H-top {}x | hold-to-50% {}x\n",
                r.get("giveback_pct").unwrap().fmt(">2"),
                s_(r, "false_stops"),
                s_(r, "whipsaw_cost_pct"),
                s_(r, "final_exit_date"),
                s_(r, "final_exit_days_after_top"),
                s_(r, "exit_pct_of_top"),
                s_(r, "captured_multiple"),
                s_(r, "captured_pct_of_bh_top"),
                s_(r, "bh_to_top_multiple"),
                s_(r, "hold_to_minus50_multiple")
            ));
        }
        let al = cp.get("trailing_armed_late").unwrap();
        o.push_str(&format!(
            "   armed-late on {} ({}d before top @ {}% of top):\n",
            s_(al, "armed_on"),
            s_(al, "armed_lead_days"),
            s_(al, "armed_price_pct_of_top")
        ));
        for r in al.get("runs").unwrap().as_list() {
            o.push_str(&format!(
                "      gb{}: false {} {} exit {} +{}d @ {}% | captured {}% of B&H-top\n",
                r.get("giveback_pct").unwrap().fmt(">2"),
                s_(r, "false_stops"),
                r.get("false_stop_dates").unwrap().repr(),
                s_(r, "final_exit_date"),
                s_(r, "final_exit_days_after_top"),
                s_(r, "exit_pct_of_top"),
                s_(r, "captured_pct_of_bh_top")
            ));
        }
        for sv in cp.get("sell_on_way_up").unwrap().as_list() {
            o.push_str(&format!(
                "   sell10%rem every +{}%: {} sales, sold {}%, avg sale {}% of top, value@top {}% / value@-50% {}% of B&H-top\n",
                s_(sv, "step_pct"),
                s_(sv, "sales"),
                s_(sv, "sold_pct"),
                s_(sv, "avg_sale_pct_of_top"),
                s_(sv, "value_at_top_pct_of_bh"),
                s_(sv, "value_at_minus50_pct_of_bh")
            ));
        }
    }
    o.push_str("\n== INDICATORS (lead = days before top; neg = after) ==\n");
    for (name, r) in ind_results.items() {
        let row: Vec<String> = tops
            .iter()
            .map(|x| {
                let p = r.get("per_top").unwrap().get(&x.label).unwrap();
                let fires = p.get("fires").unwrap().as_f64().unwrap() as i64;
                if !p.get("available").unwrap().truthy() {
                    format!("{}:n/a", x.label)
                } else if fires == 0 {
                    format!("{}:MISS", x.label)
                } else {
                    let tail = if fires > 1 {
                        format!(
                            "/{}d@{}%",
                            s_(p, "last_lead_days"),
                            s_(p, "last_fire_price_pct_of_top")
                        )
                    } else {
                        String::new()
                    };
                    format!(
                        "{}:{}d@{}%{tail}(x{fires})",
                        x.label,
                        s_(p, "first_lead_days"),
                        s_(p, "first_fire_price_pct_of_top")
                    )
                }
            })
            .collect();
        o.push_str(&format!(
            "{} from {} | {} | FALSE {} {} LOCAL {} {} | now {}\n",
            name.fmt("<30"),
            s_(r, "available_from"),
            row.join(" "),
            s_(r, "false_fires"),
            r.get("false_fire_dates").unwrap().repr(),
            s_(r, "local_top_fires"),
            r.get("local_fire_dates").unwrap().repr(),
            s_(r, "on_now")
        ));
    }
    o.push_str(&format!("\n== NOW == {}\n", now.repr()));
    (o, res)
}
