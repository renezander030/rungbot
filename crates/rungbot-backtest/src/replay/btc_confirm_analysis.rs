//! Follow-up analysis over the BTC confirmation replay.
//!
//! Which fill definition reproduces the headline fill odds; how often a pure ladder beats
//! a partial market buy; the planner's snapped rungs (30-day SMA / 30-day low snaps, a
//! 20% floor) replayed at each confirmation; conditional subsets (strong run-ups at day
//! 3 or 17); deployment shapes measured from day 3; and the expected deployed fraction
//! of a tranche.

use crate::py::{ff, mean, median, round_half_even, sum, Py};
use crate::pydict;
use crate::replay::btc_confirm::{key, sma, Btc, H};

fn pct(x: Option<f64>) -> String {
    match x {
        Some(v) => format!("{}%", ff(v * 100.0, "+.1f")),
        None => "n/a".into(),
    }
}

fn med(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        None
    } else {
        Some(median(v))
    }
}

fn fget(e: &Py, k: &str) -> Option<f64> {
    e.get(k).and_then(|v| v.as_f64())
}

fn idx(e: &Py) -> usize {
    fget(e, "idx").expect("event idx") as usize
}

fn minf(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::INFINITY, f64::min)
}
fn maxf(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
}

struct Ctx<'a> {
    b: &'a Btc,
    all: Vec<&'a Py>,
    ev: Vec<&'a Py>,
}

impl Ctx<'_> {
    fn fillprob(
        &self,
        depths: &[f64],
        low: bool,
        start_off: i64,
        window: usize,
        incl_current: bool,
    ) -> Py {
        let c = &self.b.c;
        let mut out = Py::dict();
        for &d in depths {
            let (mut n, mut k) = (0, 0);
            let evs = if incl_current { &self.all } else { &self.ev };
            for e in evs {
                let t = idx(e);
                let end = (t + window).min(c.len() - 1);
                let src = if low { &self.b.l } else { c };
                let a = (t as i64 + start_off).max(0) as usize;
                if a > end {
                    continue;
                }
                let seq = &src[a..=end];
                if seq.is_empty() {
                    continue;
                }
                n += 1;
                if minf(seq) <= c[t] * (1.0 - d / 100.0) {
                    k += 1;
                }
            }
            out.set(
                key(d),
                if n > 0 {
                    Py::Int(round_half_even(100.0 * k as f64 / n as f64))
                } else {
                    Py::None
                },
            );
        }
        out
    }

    /// The planner's snap logic replayed at day `t`: depths in % below spot.
    fn snapped_rungs(&self, t: usize, depths: &[f64]) -> Vec<f64> {
        let c = &self.b.c;
        let spot = c[t];
        let w30 = &c[t - 29..=t];
        let (sma30, low30) = (sma(w30), minf(w30));
        let bands: Vec<f64> = depths.iter().map(|d| spot * (1.0 - d / 100.0)).collect();
        let mut rungs = Vec::new();
        for (j, &p0) in bands.iter().enumerate() {
            let mut p = p0;
            let floor = if j + 1 < bands.len() {
                bands[j + 1]
            } else {
                p * 0.90
            };
            let mut cands = Vec::new();
            if floor < sma30 && sma30 < p {
                cands.push(sma30);
            }
            if floor < low30 * 1.01 && low30 * 1.01 < p {
                cands.push(low30 * 1.01);
            }
            if !cands.is_empty() {
                p = maxf(&cands);
            }
            rungs.push(p);
        }
        for j in 1..rungs.len() {
            rungs[j] = rungs[j].min(rungs[j - 1] * 0.985);
        }
        let floor_px = spot * 0.80;
        rungs
            .iter()
            .map(|p| (1.0 - p.max(floor_px) / spot) * 100.0)
            .collect()
    }

    fn outcome(
        &self,
        m: f64,
        depths: &[f64],
        weights: &[f64],
        start: usize,
        end: usize,
        spot: f64,
    ) -> (f64, f64, i64) {
        let (c, l) = (&self.b.c, &self.b.l);
        let mut btc = m / spot;
        let mut dep = m;
        let mut nf = 0;
        let tw = sum(weights.iter().copied());
        for (d, w) in depths.iter().zip(weights) {
            let p = spot * (1.0 - d / 100.0);
            let amt = (1.0 - m) * w / tw;
            if minf(&l[start..=end]) <= p {
                btc += amt / p;
                dep += amt;
                nf += 1;
            }
        }
        (btc * c[end] + (1.0 - dep) - 1.0, dep, nf)
    }
}

fn sstats(o: &mut String, name: &str, res: &[(f64, f64, i64)]) -> Py {
    let rs: Vec<f64> = res.iter().map(|r| r.0).collect();
    let none = res.iter().filter(|r| r.2 == 0).count() as f64 / rs.len() as f64;
    let dep: Vec<f64> = res.iter().map(|r| r.1).collect();
    let s = pydict![
        ("n", rs.len()),
        ("median", med(&rs)),
        ("mean", mean(&rs)),
        ("worst", minf(&rs)),
        ("best", maxf(&rs)),
        ("none_filled", none),
        ("mean_deployed", mean(&dep))
    ];
    o.push_str(&format!(
        "  {} med={} mean={} worst={} best={} none={}% depl={}%\n",
        crate::py::fs(name, "<62"),
        pct(med(&rs)),
        pct(Some(mean(&rs))),
        pct(Some(minf(&rs))),
        pct(Some(maxf(&rs))),
        ff(none * 100.0, "3.0f"),
        ff(mean(&dep) * 100.0, "3.0f")
    ));
    s
}

/// Run the analysis. `replay` is the `nobreadth` replay document; `tranche_usd` is the
/// BTC tranche the expected-deployment line prices (the market leg is 25% of it).
pub fn run(b: &Btc, replay: &Py, tranche_usd: f64) -> (String, Py) {
    let all: Vec<&Py> = replay
        .get("events")
        .map(|e| e.as_list().iter().collect())
        .unwrap_or_default();
    let ev: Vec<&Py> = all
        .iter()
        .copied()
        .filter(|e| e.get("complete90").is_some_and(|v| v.truthy()))
        .collect();
    let cx = Ctx { b, all, ev };
    let c = &b.c;
    let mut o = String::new();
    let mut out = Py::dict();

    // ---- A. which fill definition reproduces the headline odds ----
    let grid = [3.0, 5.0, 8.0, 10.0, 12.0, 15.0, 20.0, 25.0, 30.0];
    let mut defs = Py::dict();
    defs.set(
        "lows, t+1..t+90, n=15 (my baseline)",
        cx.fillprob(&grid, true, 1, 90, false),
    );
    defs.set(
        "closes, t+1..t+90, n=15",
        cx.fillprob(&grid, false, 1, 90, false),
    );
    defs.set(
        "lows, incl current partial event n=16",
        cx.fillprob(&grid, true, 1, 90, true),
    );
    defs.set(
        "closes, incl current partial n=16",
        cx.fillprob(&grid, false, 1, 90, true),
    );
    defs.set(
        "lows, from flip date (t-13)..t+90",
        cx.fillprob(&grid, true, -13, 90, false),
    );
    defs.set("lows, 60d window", cx.fillprob(&grid, true, 1, 60, false));
    defs.set(
        "closes, 60d window",
        cx.fillprob(&grid, false, 1, 60, false),
    );
    for (k, v) in defs.items() {
        o.push_str(&format!("FILLDEF {} {}\n", k.fmt("<45"), v.repr()));
    }
    out.set("fill_defs", defs);

    // ---- B. does the pure ladder beat a 25% market leg? ----
    let pure = "L4/8/14 w25/35/40 (bot BTC default)";
    let m25 = "L4/8/14 + 25% market";
    let sret = |e: &Py, nm: &str| {
        e.get("shapes")
            .and_then(|s| s.get(nm))
            .and_then(|s| fget(s, "ret"))
            .unwrap_or(f64::NAN)
    };
    let wins: Vec<(String, f64, f64)> = cx
        .ev
        .iter()
        .filter(|e| sret(e, pure) > sret(e, m25))
        .map(|e| {
            (
                e.get("date").unwrap().to_py_string(),
                sret(e, pure),
                sret(e, m25),
            )
        })
        .collect();
    let shown = Py::List(
        wins.iter()
            .map(|w| {
                Py::Tuple(vec![
                    Py::from(w.0.as_str()),
                    Py::from(pct(Some(w.1))),
                    Py::from(pct(Some(w.2))),
                ])
            })
            .collect(),
    );
    o.push_str(&format!(
        "\nPURE LADDER beats 25%-market in {}/{} events: {}\n",
        wins.len(),
        cx.ev.len(),
        shown.repr()
    ));
    out.set(
        "ladder_wins",
        Py::List(
            wins.iter()
                .map(|w| Py::Tuple(vec![Py::from(w.0.as_str()), Py::Float(w.1), Py::Float(w.2)]))
                .collect(),
        ),
    );

    // ---- C. the planner's snapped rungs, replayed at each confirmation ----
    o.push_str("\nSNAPPED (bot plan_coin logic, 30d SMA / 30d low+1% snap, 20% cap) at confirmation, 90d fwd:\n");
    let mut snap = Py::dict();
    let mut snap_v: Vec<(String, Vec<f64>)> = Vec::new();
    for e in &cx.ev {
        let d = e.get("date").unwrap().to_py_string();
        let r = cx.snapped_rungs(idx(e), &[4.0, 8.0, 14.0]);
        snap.set(
            d.as_str(),
            Py::List(r.iter().map(|x| Py::Float(*x)).collect()),
        );
        snap_v.push((d, r));
    }
    for (d, r) in &snap_v {
        let shown = Py::List(
            r.iter()
                .map(|x| Py::Float(crate::py::round(*x, 1)))
                .collect(),
        );
        o.push_str(&format!("   {d} rungs -> {}\n", shown.repr()));
    }
    out.set("snap_depths_at_conf", snap);
    let mut snapped = Py::dict();
    let variants: [(&str, f64, Option<[f64; 3]>); 4] = [
        (
            "snapped L4/8/14 w25/35/40, 0% mkt",
            0.0,
            Some([25.0, 35.0, 40.0]),
        ),
        (
            "snapped L4/8/14 w25/35/40 + 25% mkt",
            0.25,
            Some([25.0, 35.0, 40.0]),
        ),
        (
            "snapped L4/8/14 w40/35/25 + 25% mkt",
            0.25,
            Some([40.0, 35.0, 25.0]),
        ),
        ("fixed L4/8/14 w25/35/40 + 25% mkt (ref)", 0.25, None),
    ];
    for (nm, m, w) in variants {
        let res: Vec<(f64, f64, i64)> = cx
            .ev
            .iter()
            .zip(&snap_v)
            .map(|(e, (_, sd))| {
                let t = idx(e);
                let depths: Vec<f64> = if w.is_some() {
                    sd.clone()
                } else {
                    vec![4.0, 8.0, 14.0]
                };
                let weights = w.unwrap_or([25.0, 35.0, 40.0]);
                cx.outcome(m, &depths, &weights, t + 1, t + H, c[t])
            })
            .collect();
        snapped.set(nm, sstats(&mut o, nm, &res));
    }
    out.set("snapped_shapes", snapped);

    // ---- D. conditional subsets ----
    let mut conds = Py::dict();
    let cond = |o: &mut String, label: &str, k_: &str, thr: f64, k: usize| -> Py {
        let sub: Vec<&&Py> = cx
            .ev
            .iter()
            .filter(|e| fget(e, k_).is_some_and(|v| v >= thr))
            .collect();
        let dates: Vec<Py> = sub.iter().map(|e| e.get("date").unwrap().clone()).collect();
        o.push_str(&format!(
            "\nCOND {label}: n={} -> {}\n",
            sub.len(),
            Py::List(dates.clone()).repr()
        ));
        if sub.is_empty() {
            return pydict![("n", 0i64)];
        }
        let ra: Vec<f64> = sub
            .iter()
            .map(|e| fget(e, &format!("ret_after{k}")).unwrap_or(f64::NAN))
            .collect();
        let dd: Vec<f64> = sub
            .iter()
            .map(|e| fget(e, &format!("maxdd_after{k}")).unwrap_or(f64::NAN))
            .collect();
        let mut fills = Py::dict();
        let ffrom = format!("fills_from{k}");
        for d in ["4", "8", "10.9", "14", "20"] {
            let hits = sub
                .iter()
                .filter(|e| {
                    e.get(&ffrom)
                        .and_then(|f| f.get(d))
                        .is_some_and(|v| v.truthy())
                })
                .count();
            fills.set(d, hits as f64 / sub.len() as f64);
        }
        o.push_str(&format!(
            "   next {}d from day-{k} spot: median={} worst={} best={}; maxDD median={} worst={}\n",
            H - k,
            pct(med(&ra)),
            pct(Some(minf(&ra))),
            pct(Some(maxf(&ra))),
            pct(med(&dd)),
            pct(Some(minf(&dd)))
        ));
        o.push_str(&format!(
            "   fills from day-{k} spot: {}\n",
            fills
                .items()
                .iter()
                .map(|(d, v)| format!(
                    "-{}%:{}%",
                    d.to_py_string(),
                    ff(v.as_f64().unwrap() * 100.0, ".0f")
                ))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        for e in &sub {
            let fk = e.get(&ffrom).unwrap();
            let ints = Py::Dict(
                fk.items()
                    .iter()
                    .map(|(d, v)| (d.clone(), Py::Int(v.truthy() as i64)))
                    .collect(),
            );
            o.push_str(&format!(
                "     {} {k_}={} ret_after{k}={} maxdd_after{k}={} fills={}\n",
                e.get("date").unwrap().to_py_string(),
                pct(fget(e, k_)),
                pct(fget(e, &format!("ret_after{k}"))),
                pct(fget(e, &format!("maxdd_after{k}"))),
                ints.repr()
            ));
        }
        pydict![
            ("n", sub.len()),
            ("dates", Py::List(dates)),
            ("ret_after_median", med(&ra)),
            ("ret_after_worst", minf(&ra)),
            ("ret_after_best", maxf(&ra)),
            ("maxdd_after_median", med(&dd)),
            ("maxdd_after_worst", minf(&dd)),
            ("fills_from_k", fills)
        ]
    };
    let specs: [(&str, &str, &str, f64, usize); 8] = [
        (
            "all, from day17",
            "ALL events, forward from day 17",
            "run17",
            -9.0,
            17,
        ),
        (
            "run17>=15",
            "run since confirmation >= +15% at day 17",
            "run17",
            0.15,
            17,
        ),
        (
            "run17>=25",
            "run since confirmation >= +25% at day 17",
            "run17",
            0.25,
            17,
        ),
        (
            "ret30@17>=25",
            "30d return >= +25% at day 17",
            "ret30_at17",
            0.25,
            17,
        ),
        (
            "all, from day3",
            "ALL events, forward from day 3 (TODAY = day 3 post-confirmation)",
            "run3",
            -9.0,
            3,
        ),
        (
            "ret30@3>=20",
            "30d return >= +20% at day 3 (today +25.5%)",
            "ret30_at3",
            0.20,
            3,
        ),
        (
            "ret30@3>=25",
            "30d return >= +25% at day 3",
            "ret30_at3",
            0.25,
            3,
        ),
        (
            "ret30@conf>=20",
            "30d return >= +20% at confirmation (today +21.8%)",
            "ret30_at_conf",
            0.20,
            3,
        ),
    ];
    for (id, label, k_, thr, k) in specs {
        let r = cond(&mut o, label, k_, thr, k);
        conds.set(id, r);
    }

    // ---- E. shapes evaluated from day 3 after confirmation ----
    type Sh = Option<(f64, Vec<f64>, Vec<f64>)>;
    let sh: Vec<(&str, Sh)> = vec![
        (
            "pure L4/8/14 w25/35/40",
            Some((0.0, vec![4.0, 8.0, 14.0], vec![25.0, 35.0, 40.0])),
        ),
        (
            "CURRENT: 25% mkt + L4/10.9/20 w25/35/40",
            Some((0.25, vec![4.0, 10.9, 20.0], vec![25.0, 35.0, 40.0])),
        ),
        (
            "25% mkt + L4/8/14 w25/35/40",
            Some((0.25, vec![4.0, 8.0, 14.0], vec![25.0, 35.0, 40.0])),
        ),
        (
            "25% mkt + L4/8/14 w40/35/25",
            Some((0.25, vec![4.0, 8.0, 14.0], vec![40.0, 35.0, 25.0])),
        ),
        (
            "25% mkt + L3/6/10 w40/35/25",
            Some((0.25, vec![3.0, 6.0, 10.0], vec![40.0, 35.0, 25.0])),
        ),
        (
            "25% mkt + L4/10.9/20 w40/35/25",
            Some((0.25, vec![4.0, 10.9, 20.0], vec![40.0, 35.0, 25.0])),
        ),
        (
            "40% mkt + L4/8/14 w25/35/40",
            Some((0.40, vec![4.0, 8.0, 14.0], vec![25.0, 35.0, 40.0])),
        ),
        (
            "50% mkt + L4/8/14 w25/35/40",
            Some((0.50, vec![4.0, 8.0, 14.0], vec![25.0, 35.0, 40.0])),
        ),
        ("100% mkt", Some((1.0, vec![], vec![]))),
        ("snapped(bot) L4/8/14 w25/35/40 + 25% mkt", None),
    ];
    let mut from3 = Py::dict();
    let subsets: [(&str, Vec<&Py>); 2] = [
        ("ALL n=15", cx.ev.clone()),
        (
            "ret30@3>=20%",
            cx.ev
                .iter()
                .copied()
                .filter(|e| fget(e, "ret30_at3").unwrap_or(f64::NAN) >= 0.20)
                .collect(),
        ),
    ];
    for (subname, sub) in &subsets {
        o.push_str(&format!(
            "\nSHAPES FROM DAY-3 SPOT, forward to day 90 ({subname}, n={}):\n",
            sub.len()
        ));
        let mut group = Py::dict();
        for (nm, sp) in &sh {
            let res: Vec<(f64, f64, i64)> = sub
                .iter()
                .map(|e| {
                    let t = idx(e);
                    let spot = c[t + 3];
                    let (m, d, w) = match sp {
                        None => (
                            0.25,
                            cx.snapped_rungs(t + 3, &[4.0, 8.0, 14.0]),
                            vec![25.0, 35.0, 40.0],
                        ),
                        Some((m, d, w)) => (*m, d.clone(), w.clone()),
                    };
                    cx.outcome(m, &d, &w, t + 4, t + H, spot)
                })
                .collect();
            group.set(*nm, sstats(&mut o, nm, &res));
        }
        from3.set(*subname, group);
    }

    // ---- F. expected deployed fraction of the tranche ----
    let p = replay.get("fill_prob").unwrap();
    let pf = |k: &str| fget(p, k).unwrap_or(f64::NAN);
    let exp_dep =
        |p4: f64, p109: f64, p20: f64| 0.25 + (1.0 - 0.25) * (0.25 * p4 + 0.35 * p109 + 0.40 * p20);
    let uncond = exp_dep(pf("4"), pf("10.9"), pf("20"));
    let fk = |c: &Py, k: &str| {
        c.get("fills_from_k")
            .and_then(|f| fget(f, k))
            .unwrap_or(f64::NAN)
    };
    let c3 = conds.get("all, from day3").unwrap().clone();
    let d3 = exp_dep(fk(&c3, "4"), fk(&c3, "10.9"), fk(&c3, "20"));
    let c2 = conds.get("ret30@3>=20").unwrap().clone();
    let d3c = if fget(&c2, "n").unwrap_or(0.0) != 0.0 {
        Some(exp_dep(fk(&c2, "4"), fk(&c2, "10.9"), fk(&c2, "20")))
    } else {
        None
    };
    let or_nan =
        |v: Option<f64>, mul: f64| v.filter(|x| *x != 0.0).map(|x| x * mul).unwrap_or(f64::NAN);
    o.push_str(&format!(
        "\nEXPECTED DEPLOYED FRACTION of the ${} BTC tranche at day 90: unconditional (from conf) {}%; from day-3 spot all events {}%; ret30@3>=20 subset {}%\n",
        ff(tranche_usd, ",.0f"),
        ff(uncond * 100.0, ".0f"),
        ff(d3 * 100.0, ".0f"),
        ff(or_nan(d3c, 100.0), ".0f")
    ));
    o.push_str(&format!(
        "   = $ {} / {} / {} deployed of ${} (market leg ${} already in)\n",
        ff(uncond * tranche_usd, ".0f"),
        ff(d3 * tranche_usd, ".0f"),
        ff(or_nan(d3c, tranche_usd), ".0f"),
        ff(tranche_usd, ",.0f"),
        ff(tranche_usd * 0.25, ",.0f")
    ));
    out.set("cond", conds);
    out.set("from_day3", from3);
    out.set(
        "expected_deployed",
        pydict![
            ("uncond", uncond),
            ("from_day3_all", d3),
            ("from_day3_ret30ge20", d3c)
        ],
    );
    // key order of the reference document
    let order = [
        "fill_defs",
        "ladder_wins",
        "snap_depths_at_conf",
        "snapped_shapes",
        "cond",
        "from_day3",
        "expected_deployed",
    ];
    let out = Py::Dict(
        order
            .iter()
            .map(|k| (Py::from(*k), out.get(k).cloned().unwrap_or(Py::None)))
            .collect(),
    );
    (o, out)
}
