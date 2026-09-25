//! The real bull sell policy ([`rungbot_core::sellpolicy::decide`]) replayed over the
//! last cycles' tops.
//!
//! Alt events come from the alt top study (cost basis = close at the preceding bear low,
//! run to the top and on to -50% after it, or +120 days); BTC legs from the BTC top study
//! (Coin Metrics before the venues, Binance after). Daily closes; a live run samples far
//! more often, so live trails fire marginally earlier on wicks.
//!
//! Captured fraction = (proceeds + remaining position value) / (buy-and-hold value at
//! the top), per $1 of original position — at the top, at -50% after it, and at the bear
//! low within a year.

use rungbot_core::sellpolicy::{decide, BullState, SellPolicyConfig};

use crate::py::{fs, median, round, Py};
use crate::pydict;
use crate::replay::data::{day_num, day_str, iso_week};

pub const FIRST: f64 = 15.0;
/// Froth-watch reads once a day and the bot uses that read all day: lag-1 close.
pub const LAG_DAYS: usize = 1;
pub const LATCH_DAYS: usize = 14;

/// A daily close series: `(day number, close)`, oldest first.
#[derive(Debug, Clone, Default)]
pub struct Closes {
    pub d: Vec<i64>,
    pub c: Vec<f64>,
}

impl Closes {
    pub fn idx(&self, day: i64) -> usize {
        let i = self.d.partition_point(|x| *x <= day) as i64 - 1;
        i.clamp(0, self.d.len() as i64 - 1) as usize
    }
}

fn rsi(vals: &[f64], n: usize) -> f64 {
    if vals.len() < n + 1 {
        return 0.0;
    }
    let g: Vec<f64> = (1..vals.len())
        .map(|i| (vals[i] - vals[i - 1]).max(0.0))
        .collect();
    let l: Vec<f64> = (1..vals.len())
        .map(|i| (vals[i - 1] - vals[i]).max(0.0))
        .collect();
    let nf = n as f64;
    let mut ag = crate::py::sum(g[..n].iter().copied()) / nf;
    let mut al = crate::py::sum(l[..n].iter().copied()) / nf;
    for (a, b) in g[n..].iter().zip(&l[n..]) {
        ag = (ag * (nf - 1.0) + a) / nf;
        al = (al * (nf - 1.0) + b) / nf;
    }
    if al == 0.0 {
        100.0
    } else {
        100.0 - 100.0 / (1.0 + ag / al)
    }
}

/// How BTC's froth read arms the one-shot exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmRule {
    /// Pi-cycle ratio >= 0.95 alone (the live rule).
    Pi,
    /// Pi alone, or any two of Pi / Mayer >= 2.4 / weekly RSI >= 85.
    TwoOfThree,
}

impl ArmRule {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArmRule::Pi => "pi",
            ArmRule::TwoOfThree => "2of3",
        }
    }
}

/// The armed flag the bot sees each day: computed on completed weeks, latched, lagged.
pub fn btc_arm_series(s: &Closes, rule: ArmRule) -> Vec<bool> {
    let n = s.c.len();
    let c = &s.c;
    let mut raw = vec![false; n];
    let mut weekly: Vec<f64> = Vec::new();
    let mut keys: Vec<(i64, i64)> = Vec::new();
    for i in 0..n {
        let k = iso_week(s.d[i]);
        if keys.last() == Some(&k) {
            *weekly.last_mut().unwrap() = c[i];
        } else {
            keys.push(k);
            weekly.push(c[i]);
        }
        if i < 350 {
            continue;
        }
        let sum = |a: usize, b: usize| crate::py::sum(c[a..=b].iter().copied());
        let pi = sum(i - 110, i) / 111.0 / (2.0 * sum(i - 349, i) / 350.0);
        let mayer = c[i] / (sum(i - 199, i) / 200.0);
        let w = rsi(&weekly[crate::py::slice(weekly.len(), -121, -1)], 14);
        let hits = (pi >= 0.95) as i32 + (mayer >= 2.4) as i32 + (w >= 85.0) as i32;
        raw[i] = match rule {
            ArmRule::Pi => pi >= 0.95,
            ArmRule::TwoOfThree => pi >= 0.95 || hits >= 2,
        };
    }
    let mut armed = vec![false; n];
    let mut until: i64 = -1;
    for (i, slot) in armed.iter_mut().enumerate() {
        let j = i as i64 - LAG_DAYS as i64;
        if j >= 0 && raw[j as usize] {
            until = j + LATCH_DAYS as i64;
        }
        *slot = (j >= 0 && raw[j as usize]) || (i as i64) <= until;
    }
    armed
}

/// Armed while the 30-day gain is at least `thresh`.
pub fn gain30_arm(c: &[f64], thresh: f64) -> Vec<bool> {
    (0..c.len())
        .map(|i| i >= 30 && c[i] / c[i - 30] - 1.0 >= thresh)
        .collect()
}

type Sold = (String, f64, f64, i64);

struct Sim {
    cap_top: f64,
    cap_end: f64,
    cap_bear: f64,
    log: Vec<Sold>,
    hold_bear: f64,
}

#[allow(clippy::too_many_arguments)]
fn simulate(
    cfg: &SellPolicyConfig,
    sym: &str,
    s: &Closes,
    i0: usize,
    i_top: usize,
    i_end: usize,
    cost: f64,
    armed: Option<&[bool]>,
) -> Sim {
    let c = &s.c;
    let mut b: Option<BullState> = None;
    let mut proceeds = 0.0f64;
    let mut log = Vec::new();
    let i_bear = (c.len() - 1).min(i_top + 365);
    let mut i_bear_min = i_top;
    for i in i_top..=i_bear {
        if c[i] < c[i_bear_min] {
            i_bear_min = i;
        }
    }
    let (mut at_top, mut at_end, mut at_bear) = (None, None, None);
    for i in i0..=i_bear {
        let px = c[i];
        let arm = armed.is_some_and(|a| a[i]);
        let (nb, sells) = decide(sym, Some(px), Some(cost), b.as_ref(), FIRST, arm, None, cfg);
        for sl in &sells {
            proceeds += sl.pct_of_original / 100.0 * px;
            log.push((
                day_str(s.d[i]),
                round(px / cost, 2),
                sl.pct_of_original,
                sl.rung,
            ));
        }
        let rem = nb.remaining / 100.0;
        b = Some(nb);
        if i == i_top {
            at_top = Some((proceeds, rem));
        }
        if i == i_end {
            at_end = Some((proceeds, rem));
        }
        if i == i_bear_min {
            at_bear = Some((proceeds, rem));
        }
    }
    let top = c[i_top];
    let (pt, rt) = at_top.expect("the top is inside the replay");
    let (pe, re) = at_end.expect("the -50% point is inside the replay");
    let (pb, rb) = at_bear.expect("the bear low is inside the replay");
    Sim {
        cap_top: (pt + rt * top) / top,
        cap_end: (pe + re * c[i_end]) / top,
        cap_bear: (pb + rb * c[i_bear_min]) / top,
        log,
        hold_bear: c[i_bear_min] / top,
    }
}

/// `set_params` of the reference: the variant's knobs on top of fixed replay defaults.
#[derive(Debug, Clone)]
pub struct Params {
    pub gb_default: f64,
    pub arm_default: f64,
    pub tranche_pct: f64,
    pub armed_gb: f64,
    pub slice_pct: f64,
    pub gb_next: Option<f64>,
    pub slice_next: Option<f64>,
}

const fn p7(gb: f64, arm: f64, tranche: f64, armed_gb: f64, slice: f64) -> Params {
    Params {
        gb_default: gb,
        arm_default: arm,
        tranche_pct: tranche,
        armed_gb,
        slice_pct: slice,
        gb_next: None,
        slice_next: None,
    }
}

impl Params {
    pub fn config(&self, tranches: &[f64], no_tranche: &[&str]) -> SellPolicyConfig {
        SellPolicyConfig {
            giveback_pct: self.gb_default,
            giveback_next_pct: self.gb_next.unwrap_or(self.gb_default),
            armed_giveback_pct: self.armed_gb,
            core_pct: 20.0,
            tranche_pct: self.tranche_pct,
            tranches: tranches.to_vec(),
            trail_slice_pct: self.slice_pct,
            trail_slice_next_pct: self.slice_next.unwrap_or(self.slice_pct),
            trail_arm_mult: self.arm_default,
            no_tranche: no_tranche.iter().map(|s| s.to_string()).collect(),
            no_base_trail: Vec::new(),
            giveback: Default::default(),
            trail_arm: Default::default(),
        }
    }
}

/// One alt variant: params, optional 30-day-gain arming, optional tranche schedule and
/// trail-only coins. `None` params = the hold-through reference.
pub struct AltVariant {
    pub tag: &'static str,
    pub p: Option<Params>,
    pub gain: Option<f64>,
    pub tranches: Option<Vec<f64>>,
    pub no_tranche: Option<Vec<&'static str>>,
}

fn av(tag: &'static str, p: Params, tr: Option<&[f64]>, nt: Option<&[&'static str]>) -> AltVariant {
    AltVariant {
        tag,
        p: Some(p),
        gain: None,
        tranches: tr.map(|t| t.to_vec()),
        no_tranche: nt.map(|n| n.to_vec()),
    }
}

/// The alt variants the report shows, in order.
pub fn alt_variants() -> Vec<AltVariant> {
    let t4 = [4.0, 8.0, 16.0, 32.0];
    let t5 = [3.0, 5.0, 8.0, 12.0, 20.0];
    let nt: [&'static str; 3] = ["BTC", "ALT", "ALX"];
    let mut live = p7(40.0, 2.0, 20.0, 15.0, 25.0);
    live.gb_next = Some(20.0);
    live.slice_next = Some(0.0);
    vec![
        AltVariant {
            tag: "V0 hold-through reference",
            p: None,
            gain: None,
            tranches: None,
            no_tranche: None,
        },
        av(
            "V1 tranches 20@3/5/8/12, no trail",
            p7(100.0, 99.0, 20.0, 100.0, 0.0),
            None,
            None,
        ),
        av(
            "PREV mid caps tranches, ALT/ALX trail-only, slice 25@40% x4",
            p7(40.0, 2.0, 20.0, 15.0, 25.0),
            Some(&t4),
            Some(&nt),
        ),
        av(
            "LIVE same + accelerating exit: 2nd hit 20% below the hit sells the rest",
            live,
            Some(&t4),
            Some(&nt),
        ),
        av(
            "V7x tranches 15@3/5/8/12/20 + slice 20@40% arm2",
            p7(40.0, 2.0, 15.0, 100.0, 20.0),
            Some(&t5),
            None,
        ),
        av(
            "V8 tranches 15@3/5/8/12/20, no trail",
            p7(100.0, 99.0, 15.0, 100.0, 0.0),
            Some(&t5),
            None,
        ),
        av(
            "V9 tranches 20@4/8/16/32, no trail",
            p7(100.0, 99.0, 20.0, 100.0, 0.0),
            Some(&t4),
            None,
        ),
        av(
            "V10 tranches 20@4/8/16/32 + slice 20@40% arm2",
            p7(40.0, 2.0, 20.0, 100.0, 20.0),
            Some(&t4),
            None,
        ),
        av(
            "V11 tranches 15@3/5/8/12/20 + slice 20@35% arm2",
            p7(35.0, 2.0, 15.0, 100.0, 20.0),
            Some(&t5),
            None,
        ),
        av(
            "V12 tranches 20@4/8/16/32 + slice 25@40% arm2",
            p7(40.0, 2.0, 20.0, 100.0, 25.0),
            Some(&t4),
            None,
        ),
        av(
            "V13 tranches 20@4/8/16/32 + one-shot 40% arm3",
            p7(40.0, 3.0, 20.0, 100.0, 0.0),
            Some(&t4),
            None,
        ),
        av(
            "V14 tranches 20@4/8/16/32 + slice 30@40% arm2",
            p7(40.0, 2.0, 20.0, 100.0, 30.0),
            Some(&t4),
            None,
        ),
        av(
            "V15 tranches 15@3/5/8/12/20 + slice 30@40% arm2",
            p7(40.0, 2.0, 15.0, 100.0, 30.0),
            Some(&t5),
            None,
        ),
    ]
}

/// The BTC variants kept in the report.
pub fn btc_variants() -> Vec<(&'static str, Params)> {
    vec![
        (
            "B1 armed one-shot 15% only (froth signals)",
            p7(100.0, 1.25, 0.0, 15.0, 0.0),
        ),
        (
            "B12 slice 20@40% arm2 + armed one-shot 15%",
            p7(40.0, 2.0, 0.0, 15.0, 20.0),
        ),
    ]
}

/// A cycle-top event from the alt study.
pub struct AltEvent {
    pub coin: String,
    pub window: String,
    pub cost_date: String,
    pub peak_date: String,
    pub days_to_minus50: Option<i64>,
    pub ladder_top: Py,
    pub ladder_end: Py,
}

pub fn alt_events(results: &Py) -> Vec<AltEvent> {
    results
        .get("events")
        .map(|e| e.as_list())
        .unwrap_or(&[])
        .iter()
        .map(|e| {
            let lad = e.get("sims").and_then(|s| s.get("ladder"));
            AltEvent {
                coin: e.get("coin").unwrap().to_py_string(),
                window: e.get("window").unwrap().to_py_string(),
                cost_date: e.get("cost_date").unwrap().to_py_string(),
                peak_date: e.get("peak_date").unwrap().to_py_string(),
                days_to_minus50: e
                    .get("days_to_minus50")
                    .and_then(|v| v.as_f64())
                    .filter(|v| *v != 0.0)
                    .map(|v| v as i64),
                ladder_top: lad
                    .and_then(|l| l.get("captured_at_top"))
                    .cloned()
                    .unwrap_or(Py::None),
                ladder_end: lad
                    .and_then(|l| l.get("captured_at_minus50"))
                    .cloned()
                    .unwrap_or(Py::None),
            }
        })
        .collect()
}

struct Row {
    coin: String,
    window: String,
    mult: f64,
    policy_top: f64,
    policy_end: f64,
    policy_bear: f64,
    hold_bear: f64,
    ladder_top: Py,
    ladder_end: Py,
    hold_end: f64,
    sells: Vec<Sold>,
    armed_days: i64,
}

fn sells_py(s: &[Sold]) -> Py {
    Py::List(
        s.iter()
            .map(|(d, m, p, r)| {
                Py::Tuple(vec![
                    Py::from(d.as_str()),
                    Py::Float(*m),
                    Py::Float(*p),
                    Py::Int(*r),
                ])
            })
            .collect(),
    )
}

fn run_alts(
    cfg: &SellPolicyConfig,
    events: &[AltEvent],
    series: &dyn Fn(&str) -> Closes,
    gain: Option<f64>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut cache: Vec<(String, Closes)> = Vec::new();
    for e in events {
        if !cache.iter().any(|(s, _)| *s == e.coin) {
            cache.push((e.coin.clone(), series(&e.coin)));
        }
        let s = &cache.iter().find(|(x, _)| *x == e.coin).unwrap().1;
        if s.d.is_empty() {
            continue;
        }
        let i0 = s.idx(day_num(&e.cost_date));
        let i_top = s.idx(day_num(&e.peak_date));
        let i_end = s.idx(day_num(&e.peak_date) + e.days_to_minus50.unwrap_or(120));
        let cost = s.c[i0];
        let armed = gain.map(|g| gain30_arm(&s.c, g));
        let sim = simulate(cfg, &e.coin, s, i0, i_top, i_end, cost, armed.as_deref());
        rows.push(Row {
            coin: e.coin.clone(),
            window: e.window.clone(),
            mult: round(s.c[i_top] / cost, 1),
            policy_top: round(sim.cap_top, 3),
            policy_end: round(sim.cap_end, 3),
            policy_bear: round(sim.cap_bear, 3),
            hold_bear: round(sim.hold_bear, 3),
            ladder_top: e.ladder_top.clone(),
            ladder_end: e.ladder_end.clone(),
            hold_end: round(s.c[i_end] / s.c[i_top], 3),
            sells: sim.log,
            armed_days: 0,
        });
    }
    rows
}

/// A BTC cycle leg from the BTC top study.
pub struct BtcLeg {
    pub label: String,
    pub bottom_date: String,
    pub date: String,
}

pub fn btc_legs(results: &Py) -> Vec<BtcLeg> {
    results
        .get("tops")
        .map(|t| t.as_list())
        .unwrap_or(&[])
        .iter()
        .map(|t| BtcLeg {
            label: t.get("label").unwrap().to_py_string(),
            bottom_date: t.get("bottom_date").unwrap().to_py_string(),
            date: t.get("date").unwrap().to_py_string(),
        })
        .collect()
}

fn run_btc(cfg: &SellPolicyConfig, s: &Closes, legs: &[BtcLeg], rule: ArmRule) -> Vec<Row> {
    if s.d.is_empty() {
        return Vec::new();
    }
    let armed = btc_arm_series(s, rule);
    let c = &s.c;
    let mut rows = Vec::new();
    for t in legs {
        if t.label.starts_with("2013") {
            continue;
        }
        let i0 = s.idx(day_num(&t.bottom_date));
        let i_top = s.idx(day_num(&t.date));
        // end: first close <= 50% of the top after the top, else +120 days
        let lim = (c.len() - 1).min(i_top + 400);
        let i_end = (i_top..lim)
            .find(|i| c[*i] <= 0.5 * c[i_top])
            .unwrap_or((c.len() - 1).min(i_top + 120));
        let cost = c[i0];
        let sim = simulate(cfg, "BTC", s, i0, i_top, i_end, cost, Some(&armed));
        rows.push(Row {
            coin: "BTC".into(),
            window: t.label.clone(),
            mult: round(c[i_top] / cost, 1),
            policy_top: round(sim.cap_top, 3),
            policy_end: round(sim.cap_end, 3),
            policy_bear: round(sim.cap_bear, 3),
            hold_bear: round(sim.hold_bear, 3),
            ladder_top: Py::None,
            ladder_end: Py::None,
            hold_end: round(c[i_end] / c[i_top], 3),
            sells: sim.log,
            armed_days: armed[i0..=i_top].iter().filter(|a| **a).count() as i64,
        });
    }
    rows
}

fn med(xs: &[Option<f64>]) -> Py {
    let v: Vec<f64> = xs.iter().flatten().copied().collect();
    if v.is_empty() {
        Py::None
    } else {
        Py::Float(round(median(&v), 3))
    }
}

fn summarize(rows: &[Row], big_only: bool) -> Py {
    let rs: Vec<&Row> = rows.iter().filter(|r| !big_only || r.mult >= 4.0).collect();
    let mn = |f: &dyn Fn(&Row) -> f64| {
        if rs.is_empty() {
            Py::None
        } else {
            Py::Float(round(
                rs.iter().map(|r| f(r)).fold(f64::INFINITY, f64::min),
                3,
            ))
        }
    };
    pydict![
        ("n", rs.len()),
        (
            "policy_top",
            med(&rs.iter().map(|r| Some(r.policy_top)).collect::<Vec<_>>())
        ),
        (
            "policy_end",
            med(&rs.iter().map(|r| Some(r.policy_end)).collect::<Vec<_>>())
        ),
        (
            "policy_bear",
            med(&rs.iter().map(|r| Some(r.policy_bear)).collect::<Vec<_>>())
        ),
        ("bear_worst", mn(&|r: &Row| r.policy_bear)),
        (
            "hold_bear",
            med(&rs.iter().map(|r| Some(r.hold_bear)).collect::<Vec<_>>())
        ),
        ("policy_end_worst", mn(&|r: &Row| r.policy_end)),
        (
            "ladder_top",
            med(&rs.iter().map(|r| r.ladder_top.as_f64()).collect::<Vec<_>>())
        ),
        (
            "ladder_end",
            med(&rs.iter().map(|r| r.ladder_end.as_f64()).collect::<Vec<_>>())
        ),
        (
            "hold_end",
            med(&rs.iter().map(|r| Some(r.hold_end)).collect::<Vec<_>>())
        )
    ]
}

fn f5(v: &Py) -> String {
    v.fmt("5")
}

fn line(tag: &str, rows: &[Row]) -> String {
    let (a, bg) = (summarize(rows, false), summarize(rows, true));
    let g = |p: &Py, k: &str| f5(p.get(k).unwrap());
    format!(
        "{} all: top {} -50% {} bear {} (hold {}) | big: top {} -50% {} bear {} worst {}",
        fs(tag, "40s"),
        g(&a, "policy_top"),
        g(&a, "policy_end"),
        g(&a, "policy_bear"),
        g(&a, "hold_bear"),
        g(&bg, "policy_top"),
        g(&bg, "policy_end"),
        g(&bg, "policy_bear"),
        g(&bg, "bear_worst")
    )
}

/// The replay report and its JSON document. `alt_series(coin)` and `btc` are daily
/// closes; `alt_results` / `btc_results` are the two top studies' documents.
pub fn run(
    alt_results: &Py,
    btc_results: &Py,
    alt_series: &dyn Fn(&str) -> Closes,
    btc: &Closes,
) -> (String, Py) {
    let events = alt_events(alt_results);
    let default_tranches = [3.0, 5.0, 8.0, 12.0];
    let mut o = String::from(
        "ALT VARIANTS  (captured fraction of top value, median; end = at -50% after the top; 19 events, big = >=4x)\n",
    );
    let mut alts = Py::dict();
    for v in alt_variants() {
        let Some(p) = &v.p else {
            let hold = p7(100.0, 99.0, 0.0, 100.0, 0.0).config(&default_tranches, &["BTC"]);
            let rows = run_alts(&hold, &events, alt_series, None);
            o.push_str(&line(v.tag, &rows).replace("policy", "hold"));
            o.push('\n');
            continue;
        };
        // A custom schedule replays without the gain arm, with its own trail-only coins.
        let rows = match &v.tranches {
            Some(tr) => {
                let nt: Vec<&str> = v.no_tranche.clone().unwrap_or_else(|| vec!["BTC"]);
                run_alts(&p.config(tr, &nt), &events, alt_series, None)
            }
            None => run_alts(
                &p.config(&default_tranches, &["BTC"]),
                &events,
                alt_series,
                v.gain,
            ),
        };
        let evs: Vec<Py> = rows
            .iter()
            .map(|r| {
                pydict![
                    ("coin", r.coin.as_str()),
                    ("window", r.window.as_str()),
                    ("mult", r.mult),
                    ("policy_top", r.policy_top),
                    ("policy_end", r.policy_end),
                    ("sells", sells_py(&r.sells))
                ]
            })
            .collect();
        alts.set(
            v.tag,
            pydict![
                ("all", summarize(&rows, false)),
                ("big", summarize(&rows, true)),
                ("events", Py::List(evs))
            ],
        );
        o.push_str(&line(v.tag, &rows));
        o.push('\n');
        if v.tag.split_whitespace().next() == Some("LIVE") {
            for e in &rows {
                let shown = Py::List(
                    e.sells
                        .iter()
                        .map(|s| {
                            Py::Tuple(vec![
                                Py::from(s.0.chars().take(7).collect::<String>()),
                                Py::Float(s.1),
                                Py::Float(s.2),
                            ])
                        })
                        .collect(),
                );
                o.push_str(&format!(
                    "      {}{}{}x top {} bear {} (hold {}) sells {}\n",
                    fs(&e.coin, "5"),
                    fs(&e.window, "12"),
                    Py::Float(e.mult).fmt("6"),
                    crate::py::ff(e.policy_top, ".2f"),
                    crate::py::ff(e.policy_bear, ".2f"),
                    crate::py::ff(e.hold_bear, ".2f"),
                    shown.repr()
                ));
            }
        }
    }
    o.push_str("\nBTC VARIANTS  (4 legs; per-leg end in brackets)\n");
    let legs = btc_legs(btc_results);
    let mut btc_out = Py::dict();
    for rule in [ArmRule::TwoOfThree, ArmRule::Pi] {
        o.push_str(&format!(
            "-- arming rule: {} (lag {LAG_DAYS}d, latch {LATCH_DAYS}d, completed-week RSI)\n",
            rule.as_str()
        ));
        for (tag, p) in btc_variants() {
            let rows = run_btc(&p.config(&default_tranches, &["BTC"]), btc, &legs, rule);
            let s = summarize(&rows, false);
            let evs: Vec<Py> = rows
                .iter()
                .map(|r| {
                    pydict![
                        ("window", r.window.as_str()),
                        ("mult", r.mult),
                        ("policy_top", r.policy_top),
                        ("policy_end", r.policy_end),
                        ("hold_end", r.hold_end),
                        ("sells", sells_py(&r.sells)),
                        ("armed_days", r.armed_days)
                    ]
                })
                .collect();
            let g = |k: &str| f5(s.get(k).unwrap());
            o.push_str(&format!(
                "{} top {} -50% {} bear {} worst {} (hold bear {}) | {}\n",
                fs(tag, "50s"),
                g("policy_top"),
                g("policy_end"),
                g("policy_bear"),
                g("bear_worst"),
                g("hold_bear"),
                rows.iter()
                    .map(|r| format!("{}:{}", r.window, crate::py::repr(r.policy_bear)))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
            btc_out.set(
                format!("{tag} [{}]", rule.as_str()),
                pydict![("summary", s), ("events", Py::List(evs))],
            );
        }
    }
    let out = pydict![("alts", alts), ("btc", btc_out)];
    (o, out)
}
