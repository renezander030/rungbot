//! BTC bull-label confirmations, replayed over the whole daily history.
//!
//! The market label is the regime rule (`bull` = close above both the 100- and 200-day
//! SMA with enough breadth; `bear` = below the 200-day with thin breadth; else `chop`),
//! applied to completed daily closes. A **confirmation** is the 14th consecutive bull
//! day; its close is the entry. From each confirmation this measures what resting dip
//! rungs would have filled in the next 90 days (fills on daily lows, marks on closes)
//! and what each deployment shape returned.
//!
//! Breadth cannot be replayed back to 2017 — the watchlist has no history then — so two
//! variants: `nobreadth` drops the breadth clauses, `btcbreadth` lets BTC stand in for the
//! watchlist (breadth = close above its own 30-day SMA).

use crate::py::{ff, fs, mean, median, round_half_even, sum, Py};
use crate::pydict;
use crate::replay::data::Bar;

pub const CONFIRM_DAYS: usize = 14;
pub const H: usize = 90;
pub const DEPTHS: [f64; 14] = [
    2.0, 3.0, 4.0, 5.0, 8.0, 10.0, 10.9, 12.0, 14.0, 15.0, 18.0, 20.0, 25.0, 30.0,
];

/// A deployment shape: market share bought at confirmation, rung depths %, weights.
#[derive(Debug, Clone)]
pub struct Shape {
    pub name: String,
    pub market: f64,
    pub depths: Vec<f64>,
    pub weights: Vec<f64>,
}

fn shape(name: &str, market: f64, depths: &[f64], weights: &[f64]) -> Shape {
    Shape {
        name: name.into(),
        market,
        depths: depths.to_vec(),
        weights: weights.to_vec(),
    }
}

/// The shapes the study compares.
pub fn default_shapes() -> Vec<Shape> {
    vec![
        shape(
            "L4/8/14 w25/35/40 (bot BTC default)",
            0.0,
            &[4.0, 8.0, 14.0],
            &[25.0, 35.0, 40.0],
        ),
        shape(
            "L4/8/14 + 25% market",
            0.25,
            &[4.0, 8.0, 14.0],
            &[25.0, 35.0, 40.0],
        ),
        shape(
            "L4/8/14 + 30% market",
            0.30,
            &[4.0, 8.0, 14.0],
            &[25.0, 35.0, 40.0],
        ),
        shape(
            "L4/8/14 + 50% market",
            0.50,
            &[4.0, 8.0, 14.0],
            &[25.0, 35.0, 40.0],
        ),
        shape("100% market", 1.0, &[], &[]),
        shape(
            "L5/10/18 w30/40/30 (alt chop profile)",
            0.0,
            &[5.0, 10.0, 18.0],
            &[30.0, 40.0, 30.0],
        ),
        shape(
            "L4/8/14 w40/35/25 front-loaded",
            0.0,
            &[4.0, 8.0, 14.0],
            &[40.0, 35.0, 25.0],
        ),
        shape(
            "L2/5/9 w30/40/30 (ALT profile)",
            0.0,
            &[2.0, 5.0, 9.0],
            &[30.0, 40.0, 30.0],
        ),
        shape(
            "CURRENT BTC as placed: 25% mkt + L4/10.9/20 w25/35/40",
            0.25,
            &[4.0, 10.9, 20.0],
            &[25.0, 35.0, 40.0],
        ),
        shape(
            "alt: 25% mkt + L4/8/14 w40/35/25",
            0.25,
            &[4.0, 8.0, 14.0],
            &[40.0, 35.0, 25.0],
        ),
        shape(
            "alt: 25% mkt + L3/6/10 w40/35/25",
            0.25,
            &[3.0, 6.0, 10.0],
            &[40.0, 35.0, 25.0],
        ),
        shape(
            "alt: 40% mkt + L4/8/14 w40/35/25",
            0.40,
            &[4.0, 8.0, 14.0],
            &[40.0, 35.0, 25.0],
        ),
    ]
}

/// A number printed as the reference's `str(d)` for a depth key.
pub fn key(d: f64) -> String {
    crate::py::repr(d)
        .strip_suffix(".0")
        .map(str::to_string)
        .unwrap_or_else(|| crate::py::repr(d))
}

pub fn sma(v: &[f64]) -> f64 {
    sum(v.iter().copied()) / v.len() as f64
}

/// The regime label rule, verbatim.
pub fn market_label(btc: &[f64], breadth: i64, n_coins: i64) -> &'static str {
    if btc.len() < 200 {
        return "unknown";
    }
    let px = btc[btc.len() - 1];
    let s100 = sma(&btc[btc.len() - 100..]);
    let s200 = sma(&btc[btc.len() - 200..]);
    if px > s100 && px > s200 && breadth * 2 >= n_coins {
        return "bull";
    }
    if px < s200 && breadth * 3 <= n_coins {
        return "bear";
    }
    "chop"
}

pub struct Btc {
    pub dates: Vec<String>,
    pub c: Vec<f64>,
    pub l: Vec<f64>,
}

impl Btc {
    pub fn new(bars: &[Bar]) -> Btc {
        Btc {
            dates: bars.iter().map(|b| b.date.clone()).collect(),
            c: bars.iter().map(|b| b.close).collect(),
            l: bars.iter().map(|b| b.low).collect(),
        }
    }

    pub fn labels(&self, variant: &str) -> Vec<&'static str> {
        (1..=self.c.len())
            .map(|i| {
                let win = &self.c[..i];
                let (b, n) = if variant == "nobreadth" {
                    (0, 0)
                } else {
                    let above = i >= 30 && win[win.len() - 1] > sma(&win[win.len() - 30..]);
                    (above as i64, 1)
                };
                market_label(win, b, n)
            })
            .collect()
    }

    fn min_l(&self, a: usize, b_incl: usize) -> f64 {
        self.l[a..=b_incl]
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min)
    }

    /// Return on $1 at day t+H, deployed fraction, rungs filled. `None` without 90 days.
    pub fn shape_outcome(
        &self,
        t: usize,
        sp: &Shape,
        start: Option<usize>,
        spot: Option<f64>,
    ) -> Py {
        let start = start.unwrap_or(t + 1);
        let spot = spot.unwrap_or(self.c[t]);
        if t + H >= self.c.len() {
            return Py::None;
        }
        let m = sp.market;
        let mut btc = m / spot;
        let mut deployed = m;
        let mut nf = 0i64;
        let tw = {
            let s = sum(sp.weights.iter().copied());
            if s == 0.0 {
                1.0
            } else {
                s
            }
        };
        for (d, w) in sp.depths.iter().zip(&sp.weights) {
            let p = spot * (1.0 - d / 100.0);
            let amt = (1.0 - m) * w / tw;
            if self.min_l(start, t + H) <= p {
                btc += amt / p;
                deployed += amt;
                nf += 1;
            }
        }
        pydict![
            ("ret", btc * self.c[t + H] + (1.0 - deployed) - 1.0),
            ("deployed", deployed),
            ("nfilled", nf)
        ]
    }
}

fn stats(vals: &[f64]) -> Py {
    if vals.is_empty() {
        return Py::dict();
    }
    pydict![
        ("n", vals.len()),
        ("median", median(vals)),
        ("mean", mean(vals)),
        ("worst", vals.iter().cloned().fold(f64::INFINITY, f64::min)),
        (
            "best",
            vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
        )
    ]
}

fn opt(v: bool, x: f64) -> Py {
    if v {
        Py::Float(x)
    } else {
        Py::None
    }
}

/// One variant's replay: the `replay_<variant>.json` document.
pub fn run(btc: &Btc, variant: &str, shapes: &[Shape]) -> Py {
    let lab = btc.labels(variant);
    let (c, l, n) = (&btc.c, &btc.l, btc.c.len());
    let mut events = Vec::new();
    let mut run_len = 0usize;
    for i in 0..lab.len() {
        run_len = if lab[i] == "bull" { run_len + 1 } else { 0 };
        if run_len != CONFIRM_DAYS {
            continue;
        }
        let s = i + 1 - CONFIRM_DAYS;
        let prev = (0..s).rev().map(|j| lab[j]).find(|x| *x != "bull");
        let mut k = i;
        while k + 1 < lab.len() && lab[k + 1] == "bull" {
            k += 1;
        }
        let complete = i + H < n;
        let fw_end = (i + H).min(n - 1);
        let minl = |a: usize, b: usize| l[a..=b].iter().cloned().fold(f64::INFINITY, f64::min);
        let mut ev = pydict![
            ("idx", i),
            ("date", btc.dates[i].as_str()),
            ("flip_date", btc.dates[s].as_str()),
            ("prev", prev),
            ("spot", c[i]),
            ("bull_run_total_days", k - s + 1),
            ("complete90", complete)
        ];
        ev.set(
            "ret90",
            opt(complete, if complete { c[i + H] / c[i] - 1.0 } else { 0.0 }),
        );
        ev.set(
            "maxdd90",
            opt(
                fw_end > i,
                if fw_end > i {
                    minl(i + 1, fw_end) / c[i] - 1.0
                } else {
                    0.0
                },
            ),
        );
        ev.set("ret30_at_conf", c[i] / c[i - 30] - 1.0);
        let mut fills = Py::dict();
        for d in DEPTHS {
            fills.set(
                key(d),
                if fw_end > i {
                    Py::Bool(minl(i + 1, fw_end) <= c[i] * (1.0 - d / 100.0))
                } else {
                    Py::None
                },
            );
        }
        ev.set("fills", fills);
        for k_ in [3usize, 17] {
            if i + k_ < n {
                ev.set(format!("run{k_}"), c[i + k_] / c[i] - 1.0);
                ev.set(format!("ret30_at{k_}"), c[i + k_] / c[i + k_ - 30] - 1.0);
                ev.set(
                    format!("ret_after{k_}"),
                    opt(
                        complete,
                        if complete {
                            c[i + H] / c[i + k_] - 1.0
                        } else {
                            0.0
                        },
                    ),
                );
                let has = fw_end > i + k_;
                ev.set(
                    format!("maxdd_after{k_}"),
                    opt(
                        has,
                        if has {
                            minl(i + k_ + 1, fw_end) / c[i + k_] - 1.0
                        } else {
                            0.0
                        },
                    ),
                );
                let mut ff_ = Py::dict();
                for d in [4.0, 8.0, 10.9, 14.0, 20.0] {
                    ff_.set(
                        key(d),
                        if has {
                            Py::Bool(minl(i + k_ + 1, fw_end) <= c[i + k_] * (1.0 - d / 100.0))
                        } else {
                            Py::None
                        },
                    );
                }
                ev.set(format!("fills_from{k_}"), ff_);
            }
        }
        let mut sh = Py::dict();
        for sp in shapes {
            sh.set(sp.name.as_str(), btc.shape_outcome(i, sp, None, None));
        }
        ev.set("shapes", sh);
        events.push(ev);
    }
    let comp: Vec<&Py> = events
        .iter()
        .filter(|e| e.get("complete90") == Some(&Py::Bool(true)))
        .collect();
    let mut out = pydict![
        ("variant", variant),
        ("n_events", events.len()),
        ("n_complete90", comp.len())
    ];
    let nc = comp.len() as f64;
    let mut fp = Py::dict();
    for d in DEPTHS {
        let hits = comp
            .iter()
            .filter(|e| e.get("fills").and_then(|f| f.get(&key(d))) == Some(&Py::Bool(true)))
            .count();
        fp.set(key(d), hits as f64 / nc);
    }
    let mut ss = Py::dict();
    for sp in shapes {
        let get = |e: &Py, k: &str| {
            e.get("shapes")
                .and_then(|s| s.get(&sp.name))
                .and_then(|s| s.get(k))
                .and_then(|v| v.as_f64())
                .unwrap_or(f64::NAN)
        };
        let rs: Vec<f64> = comp.iter().map(|e| get(e, "ret")).collect();
        let mut st = stats(&rs);
        let none = comp
            .iter()
            .filter(|e| get(e, "nfilled") == 0.0 && sp.market == 0.0)
            .count() as f64
            / nc;
        st.set("none_filled", none);
        let dep: Vec<f64> = comp.iter().map(|e| get(e, "deployed")).collect();
        st.set("mean_deployed", mean(&dep));
        ss.set(sp.name.as_str(), st);
    }
    out.set("events", Py::List(events));
    out.set("fill_prob", fp);
    out.set("shape_stats", ss);
    out
}

fn pct_nan(v: Option<f64>) -> f64 {
    v.unwrap_or(f64::NAN) * 100.0
}

/// The console report for the given variants, plus each variant's document.
pub fn report(
    btc: &Btc,
    variants: &[&str],
    shapes: &[Shape],
    recent_from: &str,
) -> (String, Vec<(String, Py)>) {
    let mut o = String::new();
    let mut docs = Vec::new();
    for v in variants {
        let out = run(btc, v, shapes);
        let g = |k: &str| out.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) as i64;
        o.push_str(&format!(
            "\n##### variant={v}: {} confirmations ({} with full 90d)\n",
            g("n_events"),
            g("n_complete90")
        ));
        for e in out.get("events").map(|x| x.as_list()).unwrap_or(&[]) {
            let f = |k: &str| e.get(k).and_then(|x| x.as_f64());
            // `(e['ret90'] or nan)`: a zero return prints as nan too, like the reference.
            let or_nan = |k: &str| f(k).filter(|x| *x != 0.0);
            o.push_str(&format!(
                "  {} flip={} prev={} spot={} ret30@conf={}% run17={}% ret90={}% maxdd90={}% bull_days={}\n",
                e.get("date").unwrap().to_py_string(),
                e.get("flip_date").unwrap().to_py_string(),
                e.get("prev").unwrap().fmt("<4"),
                ff(f("spot").unwrap(), ">9.0f"),
                ff(f("ret30_at_conf").unwrap() * 100.0, "+6.1f"),
                ff(pct_nan(f("run17")), "+6.1f"),
                ff(pct_nan(or_nan("ret90")), "+6.1f"),
                ff(pct_nan(or_nan("maxdd90")), "+6.1f"),
                f("bull_run_total_days").unwrap() as i64
            ));
        }
        let fp = out.get("fill_prob").unwrap();
        let fpd = Py::Dict(
            fp.items()
                .iter()
                .map(|(k, x)| {
                    (
                        k.clone(),
                        Py::Int(round_half_even(x.as_f64().unwrap() * 100.0)),
                    )
                })
                .collect(),
        );
        o.push_str(&format!("  fill prob within 90d: {}\n", fpd.repr()));
        for (nm, s) in out.get("shape_stats").unwrap().items() {
            let f = |k: &str| s.get(k).and_then(|x| x.as_f64()).unwrap_or(f64::NAN);
            o.push_str(&format!(
                "  {} med={}% mean={}% worst={}% best={}% none={}% depl={}%\n",
                fs(&nm.to_py_string(), "<58"),
                ff(f("median") * 100.0, "+6.1f"),
                ff(f("mean") * 100.0, "+6.1f"),
                ff(f("worst") * 100.0, "+6.1f"),
                ff(f("best") * 100.0, "+6.1f"),
                ff(f("none_filled") * 100.0, "3.0f"),
                ff(f("mean_deployed") * 100.0, "3.0f")
            ));
        }
        docs.push((v.to_string(), out));
    }
    for v in variants {
        let lab = btc.labels(v);
        let seq: Vec<String> = (0..btc.c.len())
            .filter(|&i| btc.dates[i].as_str() >= recent_from)
            .map(|i| format!("{}:{}", &btc.dates[i][5..], &lab[i][..2]))
            .collect();
        o.push_str(&format!("\n{v} recent labels: {}\n", seq.join(" ")));
    }
    let i = btc.c.len() - 1;
    for j in i - 20..=i {
        let w = &btc.c[..=j];
        o.push_str(&format!(
            "  {} close={} sma30={} sma100={} sma200={} ret30={}%\n",
            btc.dates[j],
            ff(btc.c[j], ".0f"),
            ff(sma(&w[w.len() - 30..]), ".0f"),
            ff(sma(&w[w.len() - 100..]), ".0f"),
            ff(sma(&w[w.len() - 200..]), ".0f"),
            ff((btc.c[j] / btc.c[j - 30] - 1.0) * 100.0, "+.1f")
        ));
    }
    (o, docs)
}
