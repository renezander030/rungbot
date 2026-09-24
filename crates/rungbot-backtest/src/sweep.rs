//! A/B variants of the ladder over the same window and the same start book.
//!
//! The window run writes its start book; the sweep replays that exact book, so the
//! baseline here reproduces the as-configured run and a variant's alpha is directly
//! comparable to it. Variants: wider bands, a higher core, a trailing take-profit, and a
//! regime-gated trail that only trails while the coin is "running".
//!
//! The sweep uses the ladder's rung arithmetic from the core with the variant's bands;
//! the bookkeeping is its own lean replay, like the reference.

use rungbot_core::regime::{running_from_series, RegimeConfig};
use rungbot_core::{buy_rung_for, ladder_increment, sell_rung_for, Bands, Config};

use crate::py::{ff, fi, fs, sum};
use crate::window::{lag_from, HOUR};
use crate::{named_get, Book, History, Named};

/// One variant of the ladder.
#[derive(Debug, Clone, Copy)]
pub struct Variant {
    pub first: f64,
    pub step: f64,
    pub core: f64,
    pub floor: f64,
    pub maxord: f64,
    pub trail: f64,
    /// Trail only while the regime's RUN rule holds for the coin at that point.
    pub gate: bool,
}

const fn v(first: f64, step: f64, core: f64, trail: f64, gate: bool) -> Variant {
    Variant {
        first,
        step,
        core,
        floor: 50.0,
        maxord: 50.0,
        trail,
        gate,
    }
}

/// The variants, in report order.
pub const CONFIGS: [(&str, Variant); 9] = [
    ("baseline 10/5 core20", v(10.0, 5.0, 20.0, 0.0, false)),
    ("wider bands 15/8", v(15.0, 8.0, 20.0, 0.0, false)),
    ("wider bands 20/10", v(20.0, 10.0, 20.0, 0.0, false)),
    ("higher core 40%", v(10.0, 5.0, 40.0, 0.0, false)),
    ("trailing TP 5%", v(10.0, 5.0, 20.0, 5.0, false)),
    ("trailing TP 8%", v(10.0, 5.0, 20.0, 8.0, false)),
    ("trail8 + wider buy15", v(15.0, 8.0, 20.0, 8.0, false)),
    // Trailing only while the coin is running; in a bear or chop window these should
    // track "wider bands 15/8" closely -- the gate staying off is correct there.
    ("regime-trail5 (15/8)", v(15.0, 8.0, 20.0, 5.0, true)),
    ("regime-trail8 (15/8)", v(15.0, 8.0, 20.0, 8.0, true)),
];

#[derive(Debug, Clone)]
pub struct SweepOpts {
    pub days: i64,
    /// Replay with this many dollars of dry powder instead of the book's free stable.
    pub bag: Option<f64>,
    pub regime: RegimeConfig,
}

struct Ctx<'a> {
    syms: Vec<String>,
    route: Vec<(String, String)>,
    count: Vec<(String, i64)>,
    held0: Named,
    minnot: Named,
    start_stable: Named,
    ser: Vec<(String, Vec<f64>)>,
    entries: Vec<(String, Option<f64>)>,
    lag: usize,
    step: i64,
    n: usize,
    regime: RegimeConfig,
    _cfg: &'a Config,
}

impl Ctx<'_> {
    fn ser(&self, s: &str) -> &[f64] {
        &self
            .ser
            .iter()
            .find(|(n, _)| n == s)
            .expect("series present")
            .1
    }
    fn route(&self, s: &str) -> &str {
        &self.route.iter().find(|(n, _)| n == s).expect("routed").1
    }
    fn count(&self, ex: &str) -> f64 {
        self.count
            .iter()
            .find(|(n, _)| n == ex)
            .map(|(_, c)| *c as f64)
            .unwrap_or_else(|| panic!("KeyError: '{ex}'"))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CoinSim {
    cost: f64,
    win_until: f64,
    win_dir: u8, // 0 none, 1 buy, 2 sell
    bhw: i64,
    shw: i64,
    dep: f64,
    sold: f64,
    peak: f64,
    armed: bool,
}

fn simulate(ctx: &Ctx, var: Variant, bag: f64) -> (f64, f64, f64, i64, i64, f64) {
    let bands = Bands {
        first_pct: var.first,
        step_pct: var.step,
    };
    let mut held: Named = ctx.held0.clone();
    let mut stable: Named = ctx.start_stable.clone();
    let mut st: Vec<CoinSim> = ctx
        .syms
        .iter()
        .map(|s| {
            let e = ctx
                .entries
                .iter()
                .find(|(n, _)| n == s)
                .and_then(|(_, e)| *e)
                .filter(|e| *e != 0.0);
            CoinSim {
                cost: e.unwrap_or(ctx.ser(s)[0]),
                ..Default::default()
            }
        })
        .collect();
    let (mut buys, mut sells) = (0i64, 0i64);
    for i in ctx.lag..ctx.n {
        let now = (i as i64 * ctx.step) as f64;
        for (k, s) in ctx.syms.iter().enumerate() {
            let ex = ctx.route(s).to_string();
            let ser = ctx.ser(s);
            let price = ser[i];
            let p24 = ser[i - ctx.lag];
            let chg = (price / p24 - 1.0) * 100.0;
            let c = &mut st[k];
            let pnl = (price / c.cost - 1.0) * 100.0;
            if now >= c.win_until {
                c.win_dir = 0;
                c.bhw = 0;
                c.shw = 0;
            }
            let stable_ex = named_get(&stable, &ex).unwrap_or(0.0);
            let base = stable_ex / ctx.count(&ex);
            let hs = named_get(&held, s).unwrap_or(0.0);
            // ---- BUY: 24h dip ladder, dynamic cap, -floor, directional lock ----
            let br = buy_rung_for(Some(chg), bands).unwrap_or(0);
            if br != 0 && chg > -var.floor && c.win_dir != 2 && br > c.bhw {
                let rungs: Vec<i64> = (c.bhw + 1..=br).collect();
                let want = ladder_increment(&rungs, bands) / 100.0 * base;
                let cap = (base * (1.0 + pnl / 100.0)).max(0.0);
                let amt = want.min(cap - c.dep).min(stable_ex).min(var.maxord);
                if amt >= named_get(&ctx.minnot, s).unwrap_or(1.0) {
                    let u = amt / price;
                    c.cost = (hs * c.cost + amt) / (hs + u);
                    set(&mut held, s, hs + u);
                    set(&mut stable, &ex, stable_ex - amt);
                    c.dep += amt;
                    c.win_dir = 1;
                    c.win_until = now + (24 * HOUR) as f64;
                    buys += 1;
                }
                c.bhw = br;
            }
            // ---- SELL ----
            let hs = named_get(&held, s).unwrap_or(0.0);
            let trailing_now = var.trail > 0.0
                && (!var.gate || running_from_series(&ser[..i + 1], ctx.lag, ctx.regime).0);
            if trailing_now {
                // trailing take-profit: arm at +first%, sell a chunk on the reversal
                if pnl >= var.first && c.win_dir != 1 {
                    if !c.armed {
                        c.armed = true;
                        c.peak = price;
                    }
                    c.peak = c.peak.max(price);
                    if price <= c.peak * (1.0 - var.trail / 100.0) {
                        let take = var.first.min((100.0 - var.core) - c.sold);
                        let qty = take / 100.0 * hs;
                        let notion = qty * price;
                        if take > 0.0 && notion >= named_get(&ctx.minnot, s).unwrap_or(1.0) {
                            set(&mut held, s, hs - qty);
                            let se = named_get(&stable, &ex).unwrap_or(0.0);
                            set(&mut stable, &ex, se + notion);
                            c.sold += take;
                            c.win_dir = 2;
                            c.win_until = now + (24 * HOUR) as f64;
                            sells += 1;
                            c.armed = false;
                            c.peak = 0.0;
                        }
                    }
                } else {
                    c.armed = false;
                }
            } else {
                // fixed entry-relative ladder
                let sr = sell_rung_for(Some(pnl), bands).unwrap_or(0);
                if sr != 0 && c.win_dir != 1 && sr > c.shw {
                    let rungs: Vec<i64> = (c.shw + 1..=sr).collect();
                    let take = ladder_increment(&rungs, bands).min((100.0 - var.core) - c.sold);
                    let qty = take / 100.0 * hs;
                    let notion = qty * price;
                    if take > 0.0 && notion >= named_get(&ctx.minnot, s).unwrap_or(1.0) {
                        set(&mut held, s, hs - qty);
                        let se = named_get(&stable, &ex).unwrap_or(0.0);
                        set(&mut stable, &ex, se + notion);
                        c.sold += take;
                        c.win_dir = 2;
                        c.win_until = now + (24 * HOUR) as f64;
                        sells += 1;
                    }
                    c.shw = sr;
                }
            }
        }
    }
    let endp = |s: &str| *ctx.ser(s).last().expect("non-empty");
    let strat = sum(ctx
        .syms
        .iter()
        .map(|s| named_get(&held, s).unwrap_or(0.0) * endp(s)))
        + sum(stable.iter().map(|x| x.1));
    let bh = sum(ctx
        .syms
        .iter()
        .map(|s| named_get(&ctx.held0, s).unwrap_or(0.0) * endp(s)))
        + bag;
    (
        strat,
        bh,
        (strat / bh - 1.0) * 100.0,
        buys,
        sells,
        sum(stable.iter().map(|x| x.1)),
    )
}

fn set(m: &mut Named, k: &str, v: f64) {
    crate::named_set(m, k, v)
}

/// Run the sweep. `Err` carries what the reference would have died with.
pub fn run(
    cfg: &Config,
    history: Result<&History, String>,
    book: Option<&Book>,
    opts: &SweepOpts,
) -> Result<String, String> {
    let mut out = String::new();
    let mut route: Vec<(String, String)> = cfg
        .coins
        .iter()
        .map(|c| (c.symbol.clone(), c.venue.as_str().to_string()))
        .collect();
    let mut count: Vec<(String, i64)> = Vec::new();
    for (_, ex) in &route {
        match count.iter_mut().find(|(n, _)| n == ex) {
            Some(slot) => slot.1 += 1,
            None => count.push((ex.clone(), 1)),
        }
    }
    let book = book.ok_or_else(|| {
        "bt-book.json missing -- run the window backtest first; it writes the start book."
            .to_string()
    })?;
    if !book.counts.is_empty() {
        count = book.counts.clone(); // production's per-venue count
    }
    let mut held0: Named = route
        .iter()
        .map(|(s, _)| (s.clone(), named_get(&book.held, s).unwrap_or(0.0)))
        .collect();
    let minnot: Named = route
        .iter()
        .map(|(s, _)| (s.clone(), named_get(&book.min_notional, s).unwrap_or(1.0)))
        .collect();
    let start_stable: Named = match opts.bag {
        Some(b) => count
            .iter()
            .map(|(ex, _)| (ex.clone(), b / count.len() as f64))
            .collect(),
        None => count
            .iter()
            .map(|(ex, _)| (ex.clone(), named_get(&book.stable, ex).unwrap_or(0.0)))
            .collect(),
    };

    let hist = history?;
    let n = hist.0.iter().map(|(_, v)| v.len()).min().unwrap_or(0);
    let ser: Vec<(String, Vec<f64>)> = hist
        .0
        .iter()
        .map(|(s, v)| (s.clone(), v[v.len() - n..].iter().map(|p| p.1).collect()))
        .collect();
    let missing: Vec<String> = held0
        .iter()
        .map(|(s, _)| s.clone())
        .filter(|s| !ser.iter().any(|(n, _)| n == s))
        .collect();
    if !missing.is_empty() {
        // The venue count deliberately keeps every routed coin: live divides the bag
        // by all of them, whether or not this replay has history for each.
        out.push_str(&format!(
            "no cached history for {} -- excluded from the sweep\n",
            missing.join(", ")
        ));
        held0.retain(|(s, _)| !missing.contains(s));
        route.retain(|(s, _)| !missing.contains(s));
    }
    let first = &hist.0[0].1;
    let ts: Vec<i64> = first[first.len() - n..].iter().map(|p| p.0).collect();
    let (lag, _dt) = lag_from(&ts);
    let step = (86400.0 / lag as f64) as i64;

    let ctx = Ctx {
        syms: held0.iter().map(|(s, _)| s.clone()).collect(),
        route,
        count,
        held0,
        minnot,
        start_stable: start_stable.clone(),
        ser,
        entries: cfg
            .coins
            .iter()
            .map(|c| (c.symbol.clone(), c.entry))
            .collect(),
        lag,
        step,
        n,
        regime: opts.regime,
        _cfg: cfg,
    };

    let bag = sum(start_stable.iter().map(|x| x.1));
    out.push_str(&format!(
        "=== variant sweep, same cached {}d window, ${} bag ===\n",
        opts.days,
        ff(bag, ",.2f")
    ));
    out.push_str(&format!(
        "{} {} {} {} {} {} {}\n",
        fs("config", "24"),
        fs("strat%", ">8"),
        fs("b&h%", ">7"),
        fs("alpha", ">7"),
        fs("buys", ">5"),
        fs("sells", ">6"),
        fs("endcash", ">9")
    ));
    let start_book = sum(ctx
        .syms
        .iter()
        .map(|s| named_get(&ctx.held0, s).unwrap_or(0.0) * ctx.ser(s)[0]))
        + bag;
    for (name, var) in CONFIGS {
        let (strat, bh, alpha, b, sl, cash) = simulate(&ctx, var, bag);
        let sp = (strat / start_book - 1.0) * 100.0;
        let bhp = (bh / start_book - 1.0) * 100.0;
        out.push_str(&format!(
            "{} {}% {}% {}% {} {} ${}\n",
            fs(name, "24"),
            ff(sp, ">7.1f"),
            ff(bhp, ">6.1f"),
            ff(alpha, ">+6.1f"),
            fi(b, ">5"),
            fi(sl, ">6"),
            ff(cash, ">8.0f")
        ));
    }
    Ok(out)
}
