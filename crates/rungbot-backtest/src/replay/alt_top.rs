//! What each alt did at every cycle top it lived through, and what the ladder's sell side
//! would have captured from a cost basis set at the preceding bear low.
//!
//! The sell side is replicated rule for rule (rung = `(pnl - first) // step + 1`, the
//! first rung trades `first`, each further rung `step`, the core is never sold, a close
//! back under `+first` re-arms the ladder), in four flavours: the real semantics (a
//! rung's percentage applies to the *current* holding), nominal (to the original),
//! touch (limit-like fills at each threshold on the intraday high) and the trailing
//! give-back. Next to it: plain trailing stops on closes, scaling out on multiples, the
//! final leg into each top, and how the alt top lined up with BTC's top and its signals.

use std::collections::BTreeMap;

use crate::py::{ff, floordiv, fs, round, sum, Py};
use crate::pydict;
use crate::replay::data::{day_num, day_str, Bar};

/// The ladder the study replays: first rung, step, protected core, trailing give-back.
#[derive(Debug, Clone, Copy)]
pub struct Ladder {
    pub first: f64,
    pub step: f64,
    pub core: f64,
    pub giveback: f64,
}

impl Default for Ladder {
    fn default() -> Self {
        Ladder {
            first: 15.0,
            step: 8.0,
            core: 20.0,
            giveback: 5.0,
        }
    }
}

/// `(label, low search start, top search start, top search end)`.
pub const WINDOWS: [(&str, &str, &str, &str); 4] = [
    ("2021-spring", "2020-09-01", "2021-01-01", "2021-07-31"),
    ("2021-late", "2021-06-01", "2021-08-01", "2022-03-31"),
    ("2024", "2022-11-01", "2023-10-01", "2024-07-31"),
    ("2025", "2024-08-01", "2024-10-01", "2026-01-31"),
];

/// One source of a coin's daily history: `(name, bars)`. A price-only source has
/// open = high = low = close.
pub type Source = (String, Vec<Bar>);

/// A merged daily series; earlier sources win on a shared day.
#[derive(Debug, Clone)]
pub struct Series {
    pub dates: Vec<String>,
    pub d: Vec<i64>,
    pub o: Vec<f64>,
    pub h: Vec<f64>,
    pub l: Vec<f64>,
    pub c: Vec<f64>,
    pub src: Vec<String>,
}

impl Series {
    pub fn merge(sources: &[Source]) -> Series {
        let mut merged: BTreeMap<String, (&Bar, &str)> = BTreeMap::new();
        for (name, bars) in sources {
            for b in bars {
                merged.entry(b.date.clone()).or_insert((b, name.as_str()));
            }
        }
        let mut s = Series {
            dates: Vec::new(),
            d: Vec::new(),
            o: Vec::new(),
            h: Vec::new(),
            l: Vec::new(),
            c: Vec::new(),
            src: Vec::new(),
        };
        for (k, (b, name)) in merged {
            s.d.push(day_num(&k));
            s.dates.push(k);
            s.o.push(b.open);
            s.h.push(b.high);
            s.l.push(b.low);
            s.c.push(b.close);
            s.src.push(name.to_string());
        }
        s
    }

    /// Last bar on or before `day` (`le`), or first on or after (`ge`), clamped.
    pub fn idx_at(&self, day: i64, ge: bool) -> usize {
        let i = if ge {
            self.d.partition_point(|x| *x < day) as i64
        } else {
            self.d.partition_point(|x| *x <= day) as i64 - 1
        };
        i.clamp(0, self.d.len() as i64 - 1) as usize
    }

    fn argmax(&self, i0: usize, i1: usize) -> usize {
        let mut b = i0;
        for i in i0..=i1 {
            if self.c[i] > self.c[b] {
                b = i;
            }
        }
        b
    }

    fn argmin(&self, i0: usize, i1: usize) -> usize {
        let mut b = i0;
        for i in i0..=i1 {
            if self.c[i] < self.c[b] {
                b = i;
            }
        }
        b
    }
}

type Trade = (String, f64, f64, f64); // (date, px, nominal %, units)

struct Sim {
    trades: Vec<Trade>,
    exhausted_at: Option<(String, f64)>,
}

fn rung(lad: &Ladder, pnl: f64) -> i64 {
    if pnl >= lad.first {
        floordiv(pnl - lad.first, lad.step) as i64 + 1
    } else {
        0
    }
}

#[allow(clippy::too_many_arguments)]
fn sim_ladder(
    lad: &Ladder,
    s: &Series,
    cost: f64,
    i0: usize,
    i1: usize,
    trailing: bool,
    nominal: bool,
    touch: bool,
) -> Sim {
    let (mut units, mut sold_nom, mut sell_hw, mut peak) = (1.0f64, 0.0f64, 0i64, 0.0f64);
    let mut trades = Vec::new();
    let mut exhausted_at = None;
    for i in i0..=i1 {
        let (c, h, lo) = (s.c[i], s.h[i], s.l[i]);
        let pnl_c = (c / cost - 1.0) * 100.0;
        let pnl_h = if touch {
            (h / cost - 1.0) * 100.0
        } else {
            pnl_c
        };
        let pnl_l = if touch {
            (lo / cost - 1.0) * 100.0
        } else {
            pnl_c
        };
        let mut fires: Vec<(i64, f64)> = Vec::new();
        if rung(lad, pnl_h) == 0 && rung(lad, pnl_c) == 0 {
            sell_hw = 0;
            peak = 0.0;
            continue;
        }
        if !trailing {
            let sr = rung(lad, pnl_h);
            if sr > sell_hw {
                if touch {
                    for r in sell_hw + 1..=sr {
                        fires.push((
                            r,
                            cost * (1.0 + (lad.first + (r - 1) as f64 * lad.step) / 100.0),
                        ));
                    }
                } else {
                    fires.push((sr, c));
                }
            }
        } else {
            peak = peak.max(pnl_h);
            if sell_hw == 0 {
                fires.push((
                    1,
                    if touch {
                        cost * (1.0 + lad.first / 100.0)
                    } else {
                        c
                    },
                ));
            } else {
                let pr = rung(lad, peak);
                if pr > sell_hw && pnl_l <= peak - lad.giveback {
                    let px = if touch {
                        (cost * (1.0 + (peak - lad.giveback) / 100.0))
                            .max(lo)
                            .min(h)
                    } else {
                        c
                    };
                    fires.push((pr, px));
                }
            }
        }
        for (fire_to, px) in fires {
            if fire_to <= sell_hw {
                continue;
            }
            let want =
                sum((sell_hw + 1..=fire_to).map(|r| if r == 1 { lad.first } else { lad.step }));
            let sellable = ((100.0 - lad.core) - sold_nom).max(0.0);
            let allowed = want.min(sellable);
            if allowed >= 1.0 {
                let q = (allowed / 100.0 * if nominal { 1.0 } else { units }).min(units);
                units -= q;
                sold_nom += allowed;
                trades.push((s.dates[i].clone(), px, allowed, q));
                if sold_nom >= 80.0 - 1e-9 && exhausted_at.is_none() {
                    exhausted_at = Some((s.dates[i].clone(), px / cost));
                }
            }
            sell_hw = fire_to;
        }
        if rung(lad, pnl_c) == 0 {
            sell_hw = 0;
            peak = 0.0;
        }
    }
    Sim {
        trades,
        exhausted_at,
    }
}

/// Units held and cumulative proceeds as of index `i`.
fn value_at(s: &Series, trades: &[Trade], i: usize) -> (f64, f64) {
    let (mut u, mut p) = (1.0f64, 0.0f64);
    for (d, px, _nom, q) in trades {
        if day_num(d) <= s.d[i] {
            u -= q;
            p += q * px;
        }
    }
    (u, p)
}

fn trail_stop(s: &Series, cost: f64, i0: usize, i1: usize, g: f64) -> Option<usize> {
    let mut hi = cost;
    for i in i0..=i1 {
        hi = hi.max(s.c[i]);
        if s.c[i] <= hi * (1.0 - g / 100.0) {
            return Some(i);
        }
    }
    None
}

fn scale_out(s: &Series, cost: f64, i0: usize, i1: usize) -> Vec<Trade> {
    let mut trades = Vec::new();
    let mut done: Vec<i64> = Vec::new();
    for i in i0..=i1 {
        for m in [3i64, 5, 8, 12] {
            if !done.contains(&m) && s.c[i] >= m as f64 * cost {
                done.push(m);
                trades.push((s.dates[i].clone(), s.c[i], 0.2 * 100.0, 0.2));
            }
        }
    }
    trades
}

fn pull(best: &(f64, Option<String>, Option<String>, Option<i64>)) -> Py {
    pydict![
        (
            "depth_pct",
            if best.1.is_none() && best.0 == 0.0 {
                Py::Int(0)
            } else {
                Py::Float(round(best.0, 1))
            }
        ),
        ("from", best.1.clone()),
        ("to", best.2.clone()),
        ("days", best.3)
    ]
}

/// BTC's tops per window and the day each top signal first fired.
pub struct BtcTop {
    pub date: i64,
    pub close: f64,
    pub signals: Vec<(&'static str, Option<String>)>,
    pub last_before_top: Vec<(&'static str, String)>,
    pub mayer_at_top: f64,
    pub ret30_at_top: f64,
}

pub fn btc_signals(b: &Series) -> Vec<(String, BtcTop)> {
    let c = &b.c;
    let n = c.len();
    let sma = |i: usize, w: usize| -> Option<f64> {
        if i + 1 >= w {
            Some(sum(c[i + 1 - w..=i].iter().copied()) / w as f64)
        } else {
            None
        }
    };
    let names = [
        "pi_cycle_111>=2x350",
        "mayer>=2.4",
        "mayer>=1.8",
        "ret30d>=25%",
        "ret30d>=50%",
    ];
    let mut out = Vec::new();
    for (label, low0, top0, top1) in WINDOWS {
        let it0 = b.idx_at(day_num(top0), true);
        let it1 = b.idx_at(day_num(top1), false);
        let ip = b.argmax(it0, it1);
        let s0 = b.idx_at(day_num(low0), true);
        let s1 = (n - 1).min(it1 + 60);
        let mut sig: Vec<(&'static str, Option<String>)> =
            names.iter().map(|k| (*k, None)).collect();
        let mut last: Vec<(&'static str, String)> = Vec::new();
        for i in s0..=s1 {
            let (m111, m350, m200) = (sma(i, 111), sma(i, 350), sma(i, 200));
            let j = b.idx_at(b.d[i] - 30, false);
            let r30 = c[i] / c[j] - 1.0;
            let checks = [
                matches!((m111, m350), (Some(a), Some(z)) if a >= 2.0 * z),
                m200.is_some_and(|m| c[i] / m >= 2.4),
                m200.is_some_and(|m| c[i] / m >= 1.8),
                r30 >= 0.25,
                r30 >= 0.5,
            ];
            for (k, v) in names.iter().zip(checks) {
                if v {
                    let slot = sig.iter_mut().find(|(n_, _)| n_ == k).expect("named");
                    if slot.1.is_none() {
                        slot.1 = Some(b.dates[i].clone());
                    }
                    if i <= ip {
                        match last.iter_mut().find(|(n_, _)| n_ == k) {
                            Some(s) => s.1 = b.dates[i].clone(),
                            None => last.push((k, b.dates[i].clone())),
                        }
                    }
                }
            }
        }
        let mayer = c[ip] / sma(ip, 200).expect("200 days before the top");
        let j = b.idx_at(b.d[ip] - 30, false);
        out.push((
            label.to_string(),
            BtcTop {
                date: b.d[ip],
                close: c[ip],
                signals: sig,
                last_before_top: last,
                mayer_at_top: round(mayer, 2),
                ret30_at_top: round(c[ip] / c[j] - 1.0, 3),
            },
        ));
    }
    out
}

fn btc_top_py(t: &BtcTop) -> Py {
    pydict![
        ("date", day_str(t.date)),
        ("close", t.close),
        (
            "signals",
            Py::Dict(
                t.signals
                    .iter()
                    .map(|(k, v)| (Py::from(*k), Py::from(v.clone())))
                    .collect()
            )
        ),
        (
            "last_fire_on_or_before_top",
            Py::Dict(
                t.last_before_top
                    .iter()
                    .map(|(k, v)| (Py::from(*k), Py::from(v.as_str())))
                    .collect()
            )
        ),
        ("mayer_at_top", t.mayer_at_top),
        ("ret30_at_top", t.ret30_at_top)
    ]
}

fn tuple_trade(t: &Trade) -> Py {
    Py::Tuple(vec![
        Py::from(t.0.as_str()),
        Py::Float(round(t.1, 6)),
        Py::Float(t.2),
        Py::Float(round(t.3, 4)),
    ])
}

pub fn analyse(lad: &Ladder, sym: &str, s: &Series, btc_tops: &[(String, BtcTop)]) -> Vec<Py> {
    let n = s.c.len();
    let mut out = Vec::new();
    for (label, low0, top0, top1) in WINDOWS {
        if s.d[0] > day_num(top0) {
            continue; // the coin did not live through this top
        }
        let (it0, it1) = (
            s.idx_at(day_num(top0), true),
            s.idx_at(day_num(top1), false),
        );
        let ip = s.argmax(it0, it1);
        let (peak, pdate) = (s.c[ip], s.d[ip]);
        let il0 = s.idx_at(day_num(low0), true);
        let partial = s.d[0] > day_num(low0);
        let ic = s.argmin(il0, ip);
        let (cost, cdate) = (s.c[ic], s.d[ic]);
        let i50 = (ip + 1..n).find(|i| s.c[*i] <= 0.5 * peak);
        let i30 = (ip + 1..n).find(|i| s.c[*i] <= 0.7 * peak);
        let i_end = i50.unwrap_or((n - 1).min(ip + 365));
        let ret = |days: i64| peak / s.c[s.idx_at(pdate - days, false)] - 1.0;
        let near: Vec<usize> = (ic..=i_end).filter(|i| s.c[*i] >= 0.85 * peak).collect();
        let near_before = near.iter().filter(|i| **i < ip).count();
        let near_after = near.iter().filter(|i| **i > ip).count();
        let span = if near.is_empty() {
            0
        } else {
            s.d[*near.last().unwrap()] - s.d[near[0]] + 1
        };
        // largest pullback during the run (cost -> peak), close basis
        let (mut run_hi, mut hi_i) = (cost, ic);
        let mut best: (f64, Option<String>, Option<String>, Option<i64>) = (0.0, None, None, None);
        let (mut pulls20, mut pulls30, mut in_pull, mut cur) = (0, 0, false, 0.0f64);
        for i in ic..=ip {
            if s.c[i] >= run_hi {
                if in_pull && cur >= 20.0 {
                    pulls20 += 1;
                }
                if in_pull && cur >= 30.0 {
                    pulls30 += 1;
                }
                run_hi = s.c[i];
                hi_i = i;
                in_pull = false;
                cur = 0.0;
            } else {
                let dd = (1.0 - s.c[i] / run_hi) * 100.0;
                in_pull = true;
                cur = cur.max(dd);
                if dd > best.0 {
                    best = (
                        dd,
                        Some(s.dates[hi_i].clone()),
                        Some(s.dates[i].clone()),
                        Some(s.d[i] - s.d[hi_i]),
                    );
                }
            }
        }
        // ---- simulations from cost ----
        let mut sims = Py::dict();
        for (name, trailing, nominal, touch) in [
            ("ladder", false, false, false),
            ("ladder_nominal", false, true, false),
            ("ladder_touch", false, false, true),
            ("trail5", true, false, false),
            ("trail5_touch", true, false, true),
        ] {
            let sim = sim_ladder(lad, s, cost, ic, i_end, trailing, nominal, touch);
            let tr = &sim.trades;
            let (u_top, p_top) = value_at(s, tr, ip);
            let (u_50, p_50) = value_at(s, tr, i_end);
            let sold_top = 1.0 - u_top;
            let avg_mult = if sold_top > 1e-9 {
                Some(p_top / sold_top / cost)
            } else {
                None
            };
            let first_2x = (ic..=ip)
                .find(|i| s.c[*i] >= 2.0 * cost)
                .filter(|i| *i != 0);
            let sold_before_2x = first_2x.map(|f| 1.0 - value_at(s, tr, f - 1).0);
            sims.set(
                name,
                pydict![
                    ("sold_frac_at_top", round(sold_top, 3)),
                    (
                        "avg_sale_mult",
                        avg_mult.filter(|v| *v != 0.0).map(|v| round(v, 2))
                    ),
                    ("captured_at_top", round((p_top + u_top * peak) / peak, 3)),
                    (
                        "captured_at_minus50",
                        round((p_50 + u_50 * s.c[i_end]) / peak, 3)
                    ),
                    ("n_sells", tr.len()),
                    (
                        "rung1_fires",
                        tr.iter().filter(|t| t.2 >= lad.first).count()
                    ),
                    (
                        "exhausted_80pct_nominal_at",
                        sim.exhausted_at
                            .as_ref()
                            .map(|(d, m)| Py::Tuple(vec![Py::from(d.as_str()), Py::Float(*m)]))
                            .unwrap_or(Py::None)
                    ),
                    ("sold_frac_before_2x", sold_before_2x.map(|v| round(v, 3))),
                    (
                        "trades",
                        Py::List(tr.iter().take(40).map(tuple_trade).collect())
                    )
                ],
            );
        }
        // trailing stops on closes
        let mut stops = Py::dict();
        let mut mid: Vec<(i64, bool)> = Vec::new();
        for g in [10i64, 15, 20, 25, 30, 35, 40, 50] {
            let ei = trail_stop(s, cost, ic, i_end, g as f64);
            let v = match ei {
                None => {
                    mid.push((g, false));
                    pydict![
                        ("exit", Py::None),
                        ("captured_at_top", 1.0),
                        ("captured_at_minus50", round(s.c[i_end] / peak, 3)),
                        ("mid_run", false)
                    ]
                }
                Some(ei) => {
                    mid.push((g, ei < ip));
                    pydict![
                        ("exit", s.dates[ei].as_str()),
                        ("exit_mult", round(s.c[ei] / cost, 2)),
                        ("captured_at_top", round(s.c[ei] / peak, 3)),
                        ("captured_at_minus50", round(s.c[ei] / peak, 3)),
                        ("mid_run", ei < ip),
                        ("days_after_peak", s.d[ei] - pdate)
                    ]
                }
            };
            stops.set(g, v);
        }
        let min_g = mid.iter().find(|(_, m)| !m).map(|(g, _)| *g);
        // final leg: from the last significant low inside the 120 days before the peak
        let il = s.argmin(s.idx_at(pdate - 120, false), ip);
        let (mut run_hi, mut hi_i) = (s.c[il], il);
        let mut best_l: (f64, Option<String>, Option<String>, Option<i64>) =
            (0.0, None, None, None);
        for i in il..=ip {
            if s.c[i] >= run_hi {
                run_hi = s.c[i];
                hi_i = i;
            } else {
                let dd = (1.0 - s.c[i] / run_hi) * 100.0;
                if dd > best_l.0 {
                    best_l = (
                        dd,
                        Some(s.dates[hi_i].clone()),
                        Some(s.dates[i].clone()),
                        Some(s.d[i] - s.d[hi_i]),
                    );
                }
            }
        }
        let mut stops_l = Py::dict();
        let mut mid_l: Vec<(i64, bool)> = Vec::new();
        for g in [10i64, 15, 20, 25, 30, 40] {
            let ei = trail_stop(s, s.c[il], il, i_end, g as f64);
            let mr = ei.is_some_and(|e| e < ip);
            mid_l.push((g, mr));
            stops_l.set(
                g,
                pydict![
                    ("exit", ei.map(|e| s.dates[e].as_str())),
                    (
                        "captured_at_top",
                        round(ei.map(|e| s.c[e]).unwrap_or(s.c[i_end]) / peak, 3)
                    ),
                    ("mid_run", mr),
                    ("days_after_peak", ei.map(|e| s.d[e] - pdate))
                ],
            );
        }
        let min_gl = mid_l.iter().find(|(_, m)| !m).map(|(g, _)| *g);
        let final_leg = pydict![
            ("low_date", s.dates[il].as_str()),
            ("low", s.c[il]),
            ("leg_mult", round(peak / s.c[il], 2)),
            ("days_low_to_peak", pdate - s.d[il]),
            ("largest_pullback", pull(&best_l)),
            ("trail_stops_from_leg_low", stops_l),
            ("min_giveback_no_midrun_stop", min_gl)
        ];
        // scale-out on multiples
        let so = scale_out(s, cost, ic, i_end);
        let (u_top, p_top) = value_at(s, &so, ip);
        let (u_50, p_50) = value_at(s, &so, i_end);
        let scale = pydict![
            ("sold_frac_at_top", round(1.0 - u_top, 2)),
            ("captured_at_top", round((p_top + u_top * peak) / peak, 3)),
            (
                "captured_at_minus50",
                round((p_50 + u_50 * s.c[i_end]) / peak, 3)
            ),
            (
                "levels_hit",
                Py::List(so.iter().map(|t| Py::from(t.0.as_str())).collect())
            )
        ];
        // BTC coupling for this window
        let bt = &btc_tops
            .iter()
            .find(|(l, _)| l == label)
            .expect("a BTC top per window")
            .1;
        let alt_ret = |d0: i64, d1: i64| {
            let (j0, j1) = (s.idx_at(d0, false), s.idx_at(d1, false));
            round(s.c[j1] / s.c[j0] - 1.0, 3)
        };
        let coupling = pydict![
            ("btc_top", day_str(bt.date)),
            ("offset_days_alt_minus_btc", pdate - bt.date),
            ("alt_ret_30d_before_btc_top", alt_ret(bt.date - 30, bt.date)),
            ("alt_ret_30d_after_btc_top", alt_ret(bt.date, bt.date + 30)),
            (
                "signal_lead_days",
                Py::Dict(
                    bt.signals
                        .iter()
                        .map(|(k, v)| (
                            Py::from(*k),
                            Py::from(v.as_ref().map(|d| pdate - day_num(d)))
                        ))
                        .collect()
                )
            )
        ];
        out.push(pydict![
            ("coin", sym),
            ("window", label),
            ("partial_history", partial),
            ("source_at_peak", s.src[ip].as_str()),
            ("cost_date", day_str(cdate)),
            ("cost", cost),
            ("peak_date", day_str(pdate)),
            ("peak", peak),
            ("peak_mult", round(peak / cost, 2)),
            ("peak_intraday_high", s.h[ip]),
            ("gain_final_30d", round(ret(30), 3)),
            ("gain_final_60d", round(ret(60), 3)),
            (
                "days_within_15pct",
                pydict![
                    ("total", near.len()),
                    ("before", near_before),
                    ("after", near_after),
                    ("span", span)
                ]
            ),
            ("days_to_minus30", i30.map(|i| s.d[i] - pdate)),
            ("days_to_minus50", i50.map(|i| s.d[i] - pdate)),
            ("price_at_minus50_date", s.c[i_end]),
            ("largest_pullback_in_run", pull(&best)),
            ("pullbacks_ge20", pulls20 as i64),
            ("pullbacks_ge30", pulls30 as i64),
            ("sims", sims),
            ("trail_stops", stops),
            ("min_giveback_no_midrun_stop", min_g),
            ("scale_out", scale),
            ("final_leg", final_leg),
            ("btc", coupling)
        ]);
    }
    out
}

/// A clean monotone run from cost to 5x, valued at the top and at -50%.
pub fn synthetic_5x(lad: &Ladder) -> Py {
    let mut res = Py::dict();
    for nominal in [false, true] {
        let (mut units, mut proceeds, mut sold_nom) = (1.0f64, 0.0f64, 0.0f64);
        let mut r = 1i64;
        while sold_nom < 80.0 - 1e-9 {
            let thr = lad.first + (r - 1) as f64 * lad.step;
            if thr > 400.0 {
                break;
            }
            let want = if r == 1 { lad.first } else { lad.step };
            let allowed = want.min(80.0 - sold_nom);
            let q = allowed / 100.0 * if nominal { 1.0 } else { units };
            units -= q;
            proceeds += q * (1.0 + thr / 100.0);
            sold_nom += allowed;
            r += 1;
        }
        res.set(
            if nominal { "nominal" } else { "actual" },
            pydict![
                ("units_left", round(units, 3)),
                ("captured_at_top", round((proceeds + units * 5.0) / 5.0, 3)),
                (
                    "captured_at_minus50",
                    round((proceeds + units * 2.5) / 5.0, 3)
                )
            ],
        );
    }
    res.set("hold_through_minus50", 0.5);
    res.set("trail30_no_stopout", 0.70);
    res.set(
        "scale_out_3_5_8_12",
        round((0.2 * 3.0 + 0.2 * 5.0 + 0.6 * 5.0) / 5.0, 3),
    );
    res
}

fn gf(p: &Py, k: &str) -> f64 {
    p.get(k).and_then(|v| v.as_f64()).unwrap_or(f64::NAN)
}
fn gs(p: &Py, k: &str) -> String {
    p.get(k)
        .map(|v| v.to_py_string())
        .unwrap_or_else(|| "None".into())
}
fn int_key(d: &Py, g: i64) -> &Py {
    &d.items()
        .iter()
        .find(|(k, _)| *k == Py::Int(g))
        .expect("stop")
        .1
}

/// The whole study: `btc` is BTC's merged series, `coins` each alt's.
pub fn run(lad: &Ladder, btc: &Series, coins: &[(String, Series)]) -> (String, Py) {
    let btc_tops = btc_signals(btc);
    let mut events = Vec::new();
    for (sym, s) in coins {
        events.extend(analyse(lad, sym, s, &btc_tops));
    }
    let tops_py = Py::Dict(
        btc_tops
            .iter()
            .map(|(k, v)| (Py::from(k.as_str()), btc_top_py(v)))
            .collect(),
    );
    let synth = synthetic_5x(lad);
    let results = pydict![
        ("btc_tops", tops_py.clone()),
        ("synthetic_5x", synth.clone()),
        ("events", Py::List(events.clone()))
    ];

    let mut o = String::from("BTC tops / signals:\n");
    for (k, v) in tops_py.items() {
        o.push_str(&format!(
            "  {} top {} ${} mayer@top {} ret30@top {}\n",
            fs(&k.to_py_string(), "12s"),
            gs(v, "date"),
            ff(gf(v, "close"), ",.0f"),
            gs(v, "mayer_at_top"),
            ff(gf(v, "ret30_at_top"), "+.0%")
        ));
        o.push_str(&format!(
            "     first-fire: {}\n",
            v.get("signals").unwrap().repr()
        ));
    }
    o.push_str(&format!("synthetic clean 5x: {}\n\n", synth.repr()));
    o.push_str("coin win        cost_date  cost      peak_date  peak     x    g30   g60  n15(b/a) d-30 d-50 pull(max) #p20 offBTC | ladder top/-50 sold avg | nom top/-50 | trail5 top/-50 | st20 st30 st40 (mid) minG | scale top/-50\n");
    for e in &events {
        let sims = e.get("sims").unwrap();
        let (l, nm, t) = (
            sims.get("ladder").unwrap(),
            sims.get("ladder_nominal").unwrap(),
            sims.get("trail5").unwrap(),
        );
        let st = e.get("trail_stops").unwrap();
        let sc = e.get("scale_out").unwrap();
        let n15 = e.get("days_within_15pct").unwrap();
        let pb = e.get("largest_pullback_in_run").unwrap();
        let stfmt = |g: i64| {
            let x = int_key(st, g);
            format!(
                "{}{}",
                ff(gf(x, "captured_at_top"), ".2f"),
                if x.get("mid_run").unwrap().truthy() {
                    "m"
                } else {
                    " "
                }
            )
        };
        let avg = l
            .get("avg_sale_mult")
            .filter(|v| v.truthy())
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        o.push_str(&format!(
            "{} {} {} {} {} {} {}{} {} {}({}/{}) {} {} {}%/{}d {} {} | {}/{} {} {}x | {}/{} | {}/{} | {} {} {} minG={} | {}/{}{}\n",
            fs(&gs(e, "coin"), "4s"),
            fs(&gs(e, "window"), "10s"),
            gs(e, "cost_date"),
            ff(gf(e, "cost"), "<9.4g"),
            gs(e, "peak_date"),
            ff(gf(e, "peak"), "<8.4g"),
            ff(gf(e, "peak_mult"), "<5.1f"),
            ff(gf(e, "gain_final_30d"), "+.0%"),
            ff(gf(e, "gain_final_60d"), "+.0%"),
            n15.get("total").unwrap().fmt(">3"),
            gs(n15, "before"),
            gs(n15, "after"),
            fs(&gs(e, "days_to_minus30"), ">4"),
            fs(&gs(e, "days_to_minus50"), ">4"),
            pb.get("depth_pct").unwrap().fmt(">4.0f"),
            fs(&gs(pb, "days"), ">3"),
            e.get("pullbacks_ge20").unwrap().fmt(">2"),
            e.get("btc").unwrap().get("offset_days_alt_minus_btc").unwrap().fmt(">+4"),
            ff(gf(l, "captured_at_top"), ".2f"),
            ff(gf(l, "captured_at_minus50"), ".2f"),
            ff(gf(l, "sold_frac_at_top"), ".2f"),
            ff(avg, ".2f"),
            ff(gf(nm, "captured_at_top"), ".2f"),
            ff(gf(nm, "captured_at_minus50"), ".2f"),
            ff(gf(t, "captured_at_top"), ".2f"),
            ff(gf(t, "captured_at_minus50"), ".2f"),
            stfmt(20),
            stfmt(30),
            stfmt(40),
            gs(e, "min_giveback_no_midrun_stop"),
            ff(gf(sc, "captured_at_top"), ".2f"),
            ff(gf(sc, "captured_at_minus50"), ".2f"),
            if e.get("partial_history").unwrap().truthy() { " PARTIAL" } else { "" }
        ));
    }
    o.push('\n');
    o.push_str("coupling: coin win  alt30dBefore alt30dAfter | lead days (alt peak - signal first fire; +=signal earlier): pi mayer2.4 mayer1.8 r30>=25 r30>=50\n");
    for e in &events {
        let b = e.get("btc").unwrap();
        let ld = b.get("signal_lead_days").unwrap();
        o.push_str(&format!(
            "  {} {} {} {} | {}\n",
            fs(&gs(e, "coin"), "4s"),
            fs(&gs(e, "window"), "10s"),
            ff(gf(b, "alt_ret_30d_before_btc_top"), "+.0%"),
            ff(gf(b, "alt_ret_30d_after_btc_top"), "+.0%"),
            ld.items()
                .iter()
                .map(|(k, v)| {
                    let k = k.to_py_string();
                    let head: String = k.split('>').next().unwrap_or("").chars().take(8).collect();
                    format!("{head}={}", v.to_py_string())
                })
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    o.push('\n');
    o.push_str("ladder detail (actual code semantics): coin win  sold_before_2x  exhausted(80% nominal) at  rung1 fires  first trades\n");
    for e in &events {
        let l = e.get("sims").unwrap().get("ladder").unwrap();
        let cost = gf(e, "cost");
        let trades = l
            .get("trades")
            .unwrap()
            .as_list()
            .iter()
            .take(6)
            .map(|t| {
                let t = t.as_list();
                let d = t[0].to_py_string();
                format!(
                    "{}@{}x:{}",
                    &d[2..],
                    ff(t[1].as_f64().unwrap() / cost, ".2f"),
                    ff(t[3].as_f64().unwrap(), ".2f")
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        o.push_str(&format!(
            "  {} {} {} exh={} r1={} n={} {}\n",
            fs(&gs(e, "coin"), "4s"),
            fs(&gs(e, "window"), "10s"),
            gs(l, "sold_frac_before_2x"),
            l.get("exhausted_80pct_nominal_at").unwrap().repr(),
            gs(l, "rung1_fires"),
            gs(l, "n_sells"),
            trades
        ));
    }
    o.push('\n');
    o.push_str("final leg (last low within 120d before peak): coin win  low_date  legX  days  pull(max)  minG | trail from leg low captured@top: g10 g15 g20 g25 g30 g40 (m=mid-run stop) days-after-peak@g20/30\n");
    for e in &events {
        let f = e.get("final_leg").unwrap();
        let sl = f.get("trail_stops_from_leg_low").unwrap();
        let lp = f.get("largest_pullback").unwrap();
        let fm = |g: i64| {
            let x = int_key(sl, g);
            format!(
                "{}{}",
                ff(gf(x, "captured_at_top"), ".2f"),
                if x.get("mid_run").unwrap().truthy() {
                    "m"
                } else {
                    " "
                }
            )
        };
        o.push_str(&format!(
            "  {} {} {} {}x {}d {}%/{}d minG={} | {}  +{}/{}d\n",
            fs(&gs(e, "coin"), "4s"),
            fs(&gs(e, "window"), "10s"),
            gs(f, "low_date"),
            ff(gf(f, "leg_mult"), ">5.1f"),
            f.get("days_low_to_peak").unwrap().fmt(">4"),
            lp.get("depth_pct").unwrap().fmt(">4.0f"),
            fs(&gs(lp, "days"), ">3"),
            gs(f, "min_giveback_no_midrun_stop"),
            [10, 15, 20, 25, 30, 40]
                .iter()
                .map(|g| fm(*g))
                .collect::<Vec<_>>()
                .join(" "),
            gs(int_key(sl, 20), "days_after_peak"),
            gs(int_key(sl, 30), "days_after_peak")
        ));
    }
    o.push('\n');
    o.push_str(
        "touch-mode sensitivity (intraday high/low): ladder_touch top/-50, trail5_touch top/-50\n",
    );
    for e in &events {
        let s = e.get("sims").unwrap();
        let (lt, tt) = (
            s.get("ladder_touch").unwrap(),
            s.get("trail5_touch").unwrap(),
        );
        o.push_str(&format!(
            "  {} {} {}/{}  {}/{}\n",
            fs(&gs(e, "coin"), "4s"),
            fs(&gs(e, "window"), "10s"),
            ff(gf(lt, "captured_at_top"), ".2f"),
            ff(gf(lt, "captured_at_minus50"), ".2f"),
            ff(gf(tt, "captured_at_top"), ".2f"),
            ff(gf(tt, "captured_at_minus50"), ".2f")
        ));
    }
    (o, results)
}
