//! The dashboard's scenario model: how today's book would evolve under the price paths
//! of past cycles, and what the sell side does along the way.
//!
//! Each scenario takes a historical start date, rescales every coin's recorded path from
//! that date to today's price, and runs the book day by day: resting buy zones fill on
//! the day's low, then the sell side acts on the close under three policies: `hold`
//! (nothing sold), `ladder` (the old cost ladder, +15% then every +8pp, down to an 80%
//! nominal cap) and `live` (the bull sell policy, [`crate::sellpolicy::decide`]).
//! Coins without data at the start date follow the median path of the alts that have it.
//!
//! Next to the replays sit two static views: the book at fixed fractions of each coin's
//! all-time high, and a Monte Carlo forecast of the next cycle top, seeded so its
//! percentiles are identical on every run ([`crate::pyrandom::PyRandom`]).
//!
//! Pure: the caller loads the book, the price history and the ATH table, and writes the
//! JSON. Output is byte-compatible with the first implementation of this model: key
//! order, integer/float types, Python's `round` and number formats.

use std::collections::BTreeMap;

use crate::pymath::{floordiv, fsum_exact, median};
use crate::pyrandom::PyRandom;
use crate::sellpolicy::{decide, BullState, SellPolicyConfig};
use crate::time::days_from_civil;
use crate::watch::json::{obj, Json};
use crate::watch::pyfmt::{comma, fixed, g, ljust, rjust, round, sum as pysum};

/// The coin whose path anchors every scenario and whose froth arms the exit.
pub const MARKET: &str = "BTC";

/// The old cost ladder, nominal replica: first rung, step, and the most it sells.
pub const LADDER_FIRST: f64 = 15.0;
pub const LADDER_STEP: f64 = 8.0;
pub const LADDER_CORE_NOMINAL: f64 = 80.0;

/// Pi-cycle arming: 111-day SMA over twice the 350-day SMA at or above this.
pub const ARM_RATIO: f64 = 0.95;

/// BTC's multiple to the next cycle top, with its probability.
pub const BTC_MULT: [(f64, f64); 6] = [
    (1.5, 0.15),
    (2.0, 0.25),
    (2.5, 0.25),
    (3.0, 0.20),
    (4.0, 0.10),
    (5.5, 0.05),
];

/// Alt outcome classes: name, probability, log-uniform multiple range.
pub const ALT_MIX: [(&str, f64, (f64, f64)); 4] = [
    ("dead", 0.10, (0.15, 0.5)),
    ("weak", 0.30, (2.0, 4.0)),
    ("normal", 0.40, (5.0, 15.0)),
    ("mania", 0.20, (20.0, 60.0)),
];

/// Weight of the shared alt-season factor.
pub const ALT_CORR: f64 = 0.6;

/// The share of the peak that survives to the following bear low, per class.
pub const KEEP_POLICY: [(&str, f64); 3] = [("tranche", 0.45), ("trail", 0.42), ("btc", 0.58)];
pub const KEEP_HOLD_ALT: f64 = 0.10;
pub const KEEP_HOLD_BTC: f64 = 0.37;

/// ATH recovery levels: label, alt share of ATH, BTC multiple of ATH.
pub const LEVELS: [(&str, f64, f64); 4] = [
    ("10% of ATH, BTC at ATH", 0.10, 1.0),
    ("25% of ATH, BTC at ATH", 0.25, 1.0),
    ("50% of ATH, BTC 1.5x ATH", 0.50, 1.5),
    ("ATH again, BTC 2x ATH", 1.0, 2.0),
];

pub const DEFAULT_TIMING: &str =
    "Q4 2028 to Q2 2029 (tops came 2.5-3 years after the bottom; June 2026 low)";

const ATH_NOTE: &str = "zones assumed filled at their prices; policy sells tranches on the way up (mid caps), the trail and the armed exit act after the peak; 'kept' applies the replay's typical bear-low survival per class";

/// One coin of today's book.
#[derive(Debug, Clone, PartialEq)]
pub struct BookCoin {
    pub sym: String,
    pub held: f64,
    pub price: f64,
    pub cost: f64,
}

/// A resting buy zone: fills when a day's low reaches `price`.
#[derive(Debug, Clone, PartialEq)]
pub struct Zone {
    pub sym: String,
    pub price: f64,
    pub quote: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Book {
    pub coins: Vec<BookCoin>,
    pub cash_free: f64,
    pub zones: Vec<Zone>,
}

/// One coin's daily history: sorted days (epoch days), lows and closes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Series {
    pub days: Vec<i64>,
    pub low: Vec<f64>,
    pub close: Vec<f64>,
}

/// `YYYY-MM-DD` as days since the epoch.
pub fn parse_day(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let n = days_from_civil(y, m, d);
    // Reject 2021-02-30 and friends: the day must round-trip.
    let (yy, mm, dd, _, _, _) = crate::time::civil(n as f64 * 86_400.0);
    ((yy, mm, dd) == (y, m, d)).then_some(n)
}

/// A history from its sources, first source first. Each source is `(kind, rows)`; a row
/// is `[date, open, high, low, close, ...]` for `ohlc` (a shorter row, or any other kind,
/// reads `[date, price]`). The first source to name a date wins it.
pub fn series_from_sources(sources: &[(String, Json)]) -> Result<Series, String> {
    let mut seen: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for (kind, rows) in sources {
        let Json::Arr(rows) = rows else {
            return Err("a price history file is not a list".into());
        };
        for r in rows {
            let items = r.items();
            let date = match items.first() {
                Some(Json::Str(s)) => s.clone(),
                Some(other) => other.py_str(),
                None => return Err("list index out of range".into()),
            };
            if seen.contains_key(&date) {
                continue;
            }
            let num = |i: usize| -> Result<f64, String> {
                items
                    .get(i)
                    .ok_or_else(|| "list index out of range".to_string())?
                    .to_float()
                    .ok_or_else(|| format!("could not convert to float: {}", items[i].py_str()))
            };
            let v = if kind == "ohlc" && items.len() >= 5 {
                (num(3)?, num(4)?)
            } else {
                let px = num(1)?;
                (px, px)
            };
            seen.insert(date, v);
        }
    }
    let mut s = Series::default();
    for (date, (lo, cl)) in seen {
        let d = parse_day(&date).ok_or_else(|| format!("Invalid isoformat string: '{date}'"))?;
        s.days.push(d);
        s.low.push(lo);
        s.close.push(cl);
    }
    Ok(s)
}

/// One replay window.
#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioSpec {
    pub key: String,
    pub label: String,
    pub t0: String,
    pub days: i64,
    pub desc: String,
}

/// Everything the model reads besides the book and the histories.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// Fee in percent per coin, and for a coin not listed.
    pub fee_pct: BTreeMap<String, f64>,
    pub fee_default: f64,
    /// Market-sell slippage in percent.
    pub slip_pct: f64,
    /// Wallets outside the bot, valued along the alt median.
    pub dex_other_usd: f64,
    /// The next deposit the forecast adds at today's prices.
    pub new_cash_usd: f64,
    pub paths: usize,
    pub seed: u64,
    /// The basket whose median path stands in for coins without data.
    pub alts: Vec<String>,
    pub scenarios: Vec<ScenarioSpec>,
    /// All-time high and its date per coin.
    pub ath: Vec<(String, f64, String)>,
    /// Weights of the next deposit per coin.
    pub alloc: Vec<(String, f64)>,
    pub timing: String,
    pub policy: SellPolicyConfig,
}

impl Model {
    pub fn fee(&self, sym: &str) -> f64 {
        self.fee_pct.get(sym).copied().unwrap_or(self.fee_default)
    }

    /// Proceeds of a market sell after fee and slippage.
    pub fn net_sell(&self, sym: &str, gross: f64) -> f64 {
        gross * (1.0 - (self.fee(sym) + self.slip_pct) / 100.0)
    }

    /// Base received by a resting limit buy after the fee.
    pub fn fill_qty(&self, sym: &str, quote: f64, price: f64) -> f64 {
        quote * (1.0 - self.fee(sym) / 100.0) / price
    }

    fn no_tranche(&self, sym: &str) -> bool {
        self.policy.no_tranche.iter().any(|s| s == sym)
    }

    fn ath_of(&self, sym: &str) -> Result<&(String, f64, String), String> {
        self.ath
            .iter()
            .find(|(s, _, _)| s == sym)
            .ok_or_else(|| format!("no all-time high for {sym}: KeyError '{sym}'"))
    }
}

/// The Pi-cycle arming rule over the unscaled market history, with the bot's one-day lag
/// and a 14-day latch.
pub fn armed_series(closes: &[f64]) -> Vec<bool> {
    let (lag, latch) = (1usize, 14i64);
    let n = closes.len();
    let mut raw = vec![false; n];
    for i in 350..n {
        let pi = pysum(closes[i - 110..=i].iter().copied())
            / 111.0
            / (2.0 * pysum(closes[i - 349..=i].iter().copied()) / 350.0);
        raw[i] = pi >= ARM_RATIO;
    }
    let mut until: i64 = -1;
    (0..n)
        .map(|i| {
            let j = i as i64 - lag as i64;
            let fired = j >= 0 && raw[j as usize];
            if fired {
                until = j + latch;
            }
            fired || i as i64 <= until
        })
        .collect()
}

/// `bisect_right(days, day) - 1`.
fn at_or_before(days: &[i64], day: i64) -> isize {
    days.partition_point(|d| *d <= day) as isize - 1
}

type Factors = Vec<(f64, f64)>;

/// Per-day `(low, close)` relative to the start date's close, forward-filled; `None`
/// when the history does not cover the whole window.
pub fn path_factors(s: &Series, t0: i64, n: i64) -> Option<Factors> {
    if s.days.is_empty() || t0 < s.days[0] || t0 + n > *s.days.last()? {
        return None;
    }
    let i0 = at_or_before(&s.days, t0) as usize;
    let base = s.close[i0];
    Some(
        (0..=n)
            .map(|k| {
                let i = at_or_before(&s.days, t0 + k) as usize;
                (s.low[i] / base, s.close[i] / base)
            })
            .collect(),
    )
}

fn median_path(paths: &[&Factors]) -> Factors {
    let n = paths.iter().map(|p| p.len()).min().unwrap_or(0);
    (0..n)
        .map(|k| {
            let lows: Vec<f64> = paths.iter().map(|p| p[k].0).collect();
            let closes: Vec<f64> = paths.iter().map(|p| p[k].1).collect();
            (median(&lows).unwrap_or(0.0), median(&closes).unwrap_or(0.0))
        })
        .collect()
}

/// The old cost ladder, per coin.
#[derive(Debug, Clone, Default)]
struct Ladder {
    hw: i64,
    sold: f64,
}

impl Ladder {
    /// The percent of the CURRENT holding to sell at `price`.
    fn sells(&mut self, price: f64, cost: f64) -> f64 {
        let pnl = if cost != 0.0 {
            (price / cost - 1.0) * 100.0
        } else {
            0.0
        };
        let rung = if pnl >= LADDER_FIRST {
            floordiv(pnl - LADDER_FIRST, LADDER_STEP) as i64 + 1
        } else {
            0
        };
        if rung == 0 {
            self.hw = 0;
            return 0.0;
        }
        if rung <= self.hw {
            return 0.0;
        }
        let want =
            pysum((self.hw + 1..=rung).map(|r| if r == 1 { LADDER_FIRST } else { LADDER_STEP }));
        let room = (LADDER_CORE_NOMINAL - self.sold).max(0.0);
        let allowed = if room < want { room } else { want };
        self.hw = rung;
        if allowed < 1.0 {
            return 0.0;
        }
        self.sold += allowed;
        allowed
    }
}

/// One day's event on a replay.
fn event(d: usize, sym: &str, kind: &str, text: String) -> Json {
    obj(vec![
        ("d", Json::Int(d as i64)),
        ("sym", sym.into()),
        ("kind", kind.into()),
        ("text", text.into()),
    ])
}

struct Replay {
    nw: Vec<f64>,
    cash: Vec<f64>,
    events: Vec<Json>,
    dex: Vec<f64>,
    costs: f64,
}

fn simulate(
    policy: &str,
    m: &Model,
    book: &Book,
    factors: &[(String, Factors)],
    alt_median: &Factors,
    armed: &[bool],
) -> Result<Replay, String> {
    let syms: Vec<&str> = book.coins.iter().map(|c| c.sym.as_str()).collect();
    let idx = |s: &str| syms.iter().position(|x| *x == s);
    let mut held: Vec<f64> = book.coins.iter().map(|c| c.held).collect();
    let mut cost: Vec<f64> = book.coins.iter().map(|c| c.cost).collect();
    let p0: Vec<f64> = book.coins.iter().map(|c| c.price).collect();
    let fac: Vec<&Factors> = syms
        .iter()
        .map(|s| {
            factors
                .iter()
                .find(|(x, _)| x == s)
                .map(|(_, f)| f)
                .ok_or_else(|| format!("KeyError: '{s}'"))
        })
        .collect::<Result<_, _>>()?;
    let mut cash = book.cash_free;
    let mut zones: Vec<(Zone, bool)> = book.zones.iter().map(|z| (z.clone(), false)).collect();
    let mut bull: Vec<Option<BullState>> = vec![None; syms.len()];
    let mut lad: Vec<Ladder> = vec![Ladder::default(); syms.len()];
    let mut out = Replay {
        nw: Vec::new(),
        cash: Vec::new(),
        events: Vec::new(),
        dex: Vec::new(),
        costs: 0.0,
    };
    let n = factors
        .first()
        .map_or(0, |(_, f)| f.len())
        .saturating_sub(1);
    for k in 0..=n {
        let px: Vec<f64> = (0..syms.len()).map(|i| p0[i] * fac[i][k].1).collect();
        let lo: Vec<f64> = (0..syms.len()).map(|i| p0[i] * fac[i][k].0).collect();
        // 1. resting zones fill on the low; the fee comes off the base received
        for (z, filled) in zones.iter_mut() {
            if *filled {
                continue;
            }
            let i = idx(&z.sym).ok_or_else(|| format!("KeyError: '{}'", z.sym))?;
            if lo[i] <= z.price {
                let qty = m.fill_qty(&z.sym, z.quote, z.price);
                out.costs += z.quote - qty * z.price;
                cost[i] = if held[i] + qty > 0.0 {
                    ((held[i] * cost[i]) + z.quote) / (held[i] + qty)
                } else {
                    z.price
                };
                held[i] += qty;
                *filled = true;
                out.events.push(event(
                    k,
                    &z.sym,
                    "fill",
                    format!("zone filled ${} @ {}", comma(z.quote, 0), g(z.price, 6)),
                ));
            }
        }
        // 2. sells at market, net of fee and slippage
        for i in 0..syms.len() {
            let s = syms[i];
            if held[i] <= 0.0 || px[i] <= 0.0 {
                continue;
            }
            if policy == "ladder" {
                let pct = lad[i].sells(px[i], cost[i]);
                if pct > 0.0 {
                    let qty = pct / 100.0 * held[i];
                    let gross = qty * px[i];
                    cash += m.net_sell(s, gross);
                    out.costs += gross - m.net_sell(s, gross);
                    held[i] -= qty;
                    out.events.push(event(
                        k,
                        s,
                        "ladder",
                        format!(
                            "ladder sold {}% @ {}x",
                            fixed(pct, 0),
                            fixed(px[i] / cost[i], 2)
                        ),
                    ));
                }
            } else if policy == "live" {
                let a = s == MARKET && !armed.is_empty() && armed[k];
                let (b, sells) = decide(
                    s,
                    Some(px[i]),
                    Some(cost[i]),
                    bull[i].as_ref(),
                    LADDER_FIRST,
                    a,
                    None,
                    &m.policy,
                );
                bull[i] = Some(b);
                for sl in sells {
                    let qty = sl.pct_of_held / 100.0 * held[i];
                    let gross = qty * px[i];
                    cash += m.net_sell(s, gross);
                    out.costs += gross - m.net_sell(s, gross);
                    held[i] -= qty;
                    let kind = if sl.rung == 99 && a {
                        "armed"
                    } else if sl.rung >= 89 {
                        "trail"
                    } else {
                        "tranche"
                    };
                    out.events.push(event(
                        k,
                        s,
                        kind,
                        format!(
                            "{} -> sold {}% of original @ {}x (${})",
                            sl.reason,
                            g(sl.pct_of_original, 6),
                            fixed(px[i] / cost[i], 2),
                            comma(qty * px[i], 0)
                        ),
                    ));
                }
            }
        }
        let open: Vec<f64> = zones
            .iter()
            .filter(|(_, f)| !f)
            .map(|(z, _)| z.quote)
            .collect();
        let locked = pysum(open.iter().copied());
        let total = pysum((0..syms.len()).map(|i| held[i] * px[i])) + cash + locked;
        out.nw.push(round(total, 2));
        out.cash.push(round(cash + locked, 2));
        out.dex.push(round(m.dex_other_usd * alt_median[k].1, 2));
    }
    if policy == "live" && !armed.is_empty() {
        let mut on = false;
        for (k, a) in armed.iter().enumerate().take(n + 1) {
            if *a != on {
                on = *a;
                out.events.push(event(
                    k,
                    MARKET,
                    if on { "arm_on" } else { "arm_off" },
                    format!(
                        "BTC blow-off signals {}",
                        if on {
                            "ARMED: trail tightens to 15%"
                        } else {
                            "disarmed"
                        }
                    ),
                ));
            }
        }
    }
    // Stable: same-day events keep their order.
    out.events.sort_by_key(|e| match e.get("d") {
        Some(Json::Int(d)) => *d,
        _ => 0,
    });
    out.costs = round(out.costs, 2);
    Ok(out)
}

fn summarize(nw: &[f64], nw0: f64) -> Result<Json, String> {
    let mut ip = 0;
    for (i, v) in nw.iter().enumerate() {
        if *v > nw[ip] {
            ip = i;
        }
    }
    let peak = *nw.get(ip).ok_or("max() arg is an empty sequence")?;
    let mut trough = nw[ip];
    for v in &nw[ip..] {
        if *v < trough {
            trough = *v;
        }
    }
    if nw0 == 0.0 || peak == 0.0 {
        return Err("float division by zero".into());
    }
    let last = *nw.last().expect("non-empty");
    Ok(obj(vec![
        ("start", round(nw0, 2).into()),
        ("peak", round(peak, 2).into()),
        ("peak_day", Json::Int(ip as i64)),
        ("peak_x", round(peak / nw0, 2).into()),
        ("end", round(last, 2).into()),
        ("end_x", round(last / nw0, 2).into()),
        ("trough_after_peak", round(trough, 2).into()),
        ("kept_of_peak", round(trough / peak, 2).into()),
    ]))
}

fn keep_json() -> Json {
    obj(vec![
        (
            "policy",
            Json::Obj(
                KEEP_POLICY
                    .iter()
                    .map(|(k, v)| (k.to_string(), Json::Float(*v)))
                    .collect(),
            ),
        ),
        (
            "hold",
            obj(vec![
                ("alt", KEEP_HOLD_ALT.into()),
                ("btc", KEEP_HOLD_BTC.into()),
            ]),
        ),
    ])
}

fn keep_policy(cls: &str) -> f64 {
    KEEP_POLICY
        .iter()
        .find(|(k, _)| *k == cls)
        .map_or(0.0, |(_, v)| *v)
}

/// Held and cost with every zone filled: `with_fee` fills net of the venue fee (the ATH
/// view), otherwise at the full quote (the forecast).
fn with_zones(m: &Model, book: &Book, with_fee: bool) -> Result<(Vec<f64>, Vec<f64>), String> {
    let mut held: Vec<f64> = book.coins.iter().map(|c| c.held).collect();
    let mut cost: Vec<f64> = book.coins.iter().map(|c| c.cost).collect();
    for z in &book.zones {
        let i = book
            .coins
            .iter()
            .position(|c| c.sym == z.sym)
            .ok_or_else(|| format!("KeyError: '{}'", z.sym))?;
        let qty = if with_fee {
            m.fill_qty(&z.sym, z.quote, z.price)
        } else {
            z.quote / z.price
        };
        cost[i] = if held[i] + qty > 0.0 {
            ((held[i] * cost[i]) + z.quote) / (held[i] + qty)
        } else {
            z.price
        };
        held[i] += qty;
    }
    Ok((held, cost))
}

/// Tranche proceeds on the way to `target` and the share left: sold at exactly
/// `multiple x cost`, never below the core.
fn tranches(m: &Model, sym: &str, held: f64, cost: f64, target: f64) -> (f64, f64) {
    let p = &m.policy;
    let (mut rem, mut proceeds) = (1.0, 0.0);
    for mult in &p.tranches {
        if target / cost >= *mult && rem - p.tranche_pct / 100.0 >= p.core_pct / 100.0 - 1e-9 {
            proceeds += m.net_sell(sym, p.tranche_pct / 100.0 * held * cost * mult);
            rem -= p.tranche_pct / 100.0;
        }
    }
    (rem, proceeds)
}

/// Every coin priced by its distance from its ATH, and the book at each recovery level.
pub fn ath_recovery(m: &Model, book: &Book) -> Result<Json, String> {
    let (held, cost) = with_zones(m, book, true)?;
    let mut rows = Vec::new();
    for (i, c) in book.coins.iter().enumerate() {
        let (_, a, ad) = m.ath_of(&c.sym)?;
        rows.push(obj(vec![
            ("sym", c.sym.as_str().into()),
            ("price", c.price.into()),
            ("ath", (*a).into()),
            ("ath_date", ad.as_str().into()),
            ("down_pct", round(100.0 * (1.0 - c.price / a), 1).into()),
            ("x_to_ath", round(a / c.price, 1).into()),
            ("held_incl_zones", round(held[i], 6).into()),
            ("cost_incl_zones", cost[i].into()),
            ("value_now", round(held[i] * c.price, 2).into()),
        ]));
    }
    let mut levels = Vec::new();
    for (label, alt_share, btc_x) in LEVELS {
        let mut per = Vec::new();
        let (mut hold_total, mut pol_total, mut keep_hold, mut keep_pol) = (0.0, 0.0, 0.0, 0.0);
        for (i, c) in book.coins.iter().enumerate() {
            let s = c.sym.as_str();
            let ath = m.ath_of(s)?.1;
            let target = ath * if s == MARKET { btc_x } else { alt_share };
            let hold_v = held[i] * target;
            let (rem, proceeds) = if !m.no_tranche(s) && m.policy.tranche_pct > 0.0 {
                tranches(m, s, held[i], cost[i], target)
            } else {
                (1.0, 0.0)
            };
            let pol_v = proceeds + rem * held[i] * target;
            let cls = if s == MARKET {
                "btc"
            } else if m.no_tranche(s) {
                "trail"
            } else {
                "tranche"
            };
            let kh = hold_v
                * if s == MARKET {
                    KEEP_HOLD_BTC
                } else {
                    KEEP_HOLD_ALT
                };
            let kp = proceeds + (pol_v - proceeds) * keep_policy(cls);
            per.push((
                s.to_string(),
                obj(vec![
                    ("target", target.into()),
                    ("x", round(target / c.price, 1).into()),
                    ("hold", round(hold_v, 2).into()),
                    ("policy", round(pol_v, 2).into()),
                    ("proceeds", round(proceeds, 2).into()),
                    ("keep_hold", round(kh, 2).into()),
                    ("keep_policy", round(kp, 2).into()),
                ]),
            ));
            hold_total += hold_v;
            pol_total += pol_v;
            keep_hold += kh;
            keep_pol += kp;
        }
        levels.push(obj(vec![
            ("label", label.into()),
            ("alt_share", alt_share.into()),
            ("btc_x", btc_x.into()),
            ("per", Json::Obj(per)),
            ("hold", round(hold_total, 2).into()),
            ("policy", round(pol_total, 2).into()),
            ("keep_hold", round(keep_hold, 2).into()),
            ("keep_policy", round(keep_pol, 2).into()),
        ]));
    }
    Ok(obj(vec![
        ("coins", Json::Arr(rows)),
        ("levels", Json::Arr(levels)),
        ("keep", keep_json()),
        ("note", ATH_NOTE.into()),
    ]))
}

/// `sorted(v)[min(len - 1, int(q * len))]`.
fn pct(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    let i = ((q * n as f64) as usize).min(n - 1);
    sorted[i]
}

/// The next cycle top: base-rate multiples drawn per path, the book valued under the
/// hold and the policy rules, and the share that survives to the following bear.
pub fn forecast(m: &Model, book: &Book) -> Result<Json, String> {
    let mut rnd = PyRandom::new(m.seed);
    let (mut held, mut cost) = with_zones(m, book, false)?;
    let tot_w = pysum(m.alloc.iter().map(|(_, w)| *w));
    for (s, w) in &m.alloc {
        if let Some(i) = book.coins.iter().position(|c| &c.sym == s) {
            let usd = m.new_cash_usd * w / tot_w;
            let qty = usd / book.coins[i].price;
            cost[i] = ((held[i] * cost[i]) + usd) / (held[i] + qty);
            held[i] += qty;
        }
    }
    const KEYS: [&str; 8] = [
        "hold_top",
        "policy_top",
        "keep_hold",
        "keep_policy",
        "dex_top",
        "dex_keep",
        "btc_x",
        "alt_med_x",
    ];
    let mut out: Vec<Vec<f64>> = vec![Vec::with_capacity(m.paths); KEYS.len()];
    let mut cum = Vec::new();
    let mut acc = 0.0;
    for (cls, pr, rng) in ALT_MIX {
        acc += pr;
        cum.push((acc, cls, rng));
    }
    for _ in 0..m.paths {
        let u = rnd.random();
        let mut bx = BTC_MULT[BTC_MULT.len() - 1].0;
        let mut a = 0.0;
        for (mult, pr) in BTC_MULT {
            a += pr;
            if u <= a {
                bx = mult;
                break;
            }
        }
        let season = rnd.random();
        let (mut hold_top, mut policy_top, mut keep_hold, mut keep_pol) = (0.0, 0.0, 0.0, 0.0);
        let mut alt_x = Vec::new();
        for (i, c) in book.coins.iter().enumerate() {
            let s = c.sym.as_str();
            let x = if s == MARKET {
                bx
            } else {
                let u = ALT_CORR * season + (1.0 - ALT_CORR) * rnd.random();
                let last = cum[cum.len() - 1];
                let (_, _, (lo, hi)) = if u <= last.0 {
                    *cum.iter()
                        .find(|(a2, _, _)| u <= a2 + 1e-12)
                        .unwrap_or(&last)
                } else {
                    last
                };
                let x = lo * (hi / lo).powf(rnd.random());
                alt_x.push(x);
                x
            };
            let price = c.price * x;
            let hv = held[i] * price;
            let (rem, proceeds) = if s != MARKET && !m.no_tranche(s) && m.policy.tranche_pct > 0.0 {
                tranches(m, s, held[i], cost[i], price)
            } else {
                (1.0, 0.0)
            };
            let pv = proceeds + rem * hv;
            let k_cls = if s == MARKET {
                "btc"
            } else if m.no_tranche(s) {
                "trail"
            } else {
                "tranche"
            };
            hold_top += hv;
            policy_top += pv;
            keep_hold += hv
                * if s == MARKET {
                    KEEP_HOLD_BTC
                } else {
                    KEEP_HOLD_ALT
                };
            keep_pol += proceeds + (pv - proceeds) * keep_policy(k_cls);
        }
        let dex_x = median(&alt_x).unwrap_or(1.0);
        out[0].push(hold_top);
        out[1].push(policy_top);
        out[2].push(keep_hold);
        out[3].push(keep_pol);
        out[4].push(m.dex_other_usd * dex_x);
        out[5].push(m.dex_other_usd * dex_x * KEEP_HOLD_ALT);
        out[6].push(bx);
        out[7].push(dex_x);
    }
    let mut dist = Vec::new();
    for (k, v) in KEYS.iter().zip(out.iter()) {
        if v.is_empty() {
            return Err("fmean requires at least one data point".into());
        }
        let mut s = v.clone();
        s.sort_by(f64::total_cmp);
        let mean = fsum_exact(v.iter().copied()) / v.len() as f64;
        dist.push((
            k.to_string(),
            obj(vec![
                ("p10", round(pct(&s, 0.10), 2).into()),
                ("p25", round(pct(&s, 0.25), 2).into()),
                ("p50", round(pct(&s, 0.50), 2).into()),
                ("p75", round(pct(&s, 0.75), 2).into()),
                ("p90", round(pct(&s, 0.90), 2).into()),
                ("mean", round(mean, 2).into()),
            ]),
        ));
    }
    let invested = pysum(
        book.coins
            .iter()
            .enumerate()
            .map(|(i, c)| held[i] * c.price),
    );
    let btc_mult = Json::Arr(
        BTC_MULT
            .iter()
            .map(|(a, b)| Json::Arr(vec![(*a).into(), (*b).into()]))
            .collect(),
    );
    let alt_mix = Json::Arr(
        ALT_MIX
            .iter()
            .map(|(c, p, (lo, hi))| {
                Json::Arr(vec![
                    (*c).into(),
                    (*p).into(),
                    Json::Arr(vec![(*lo).into(), (*hi).into()]),
                ])
            })
            .collect(),
    );
    Ok(obj(vec![
        ("invested_now", round(invested, 2).into()),
        ("new_cash", m.new_cash_usd.into()),
        ("dex_other", m.dex_other_usd.into()),
        ("timing", m.timing.as_str().into()),
        (
            "inputs",
            obj(vec![
                ("btc_mult", btc_mult),
                ("alt_mix", alt_mix),
                ("alt_corr", ALT_CORR.into()),
                ("keep", keep_json()),
                ("paths", Json::Int(m.paths as i64)),
            ]),
        ),
        ("dist", Json::Obj(dist)),
    ]))
}

/// The model's output and the log lines around writing it.
#[derive(Debug, Clone)]
pub struct Output {
    pub json: Json,
    /// Printed before the file is written.
    pub head: Vec<String>,
    /// Printed after the size line.
    pub tail: Vec<String>,
    pub nw0: f64,
}

impl Output {
    /// `scenarios.json: N scenarios, nw0 $X, K KB` for a file of `bytes`.
    pub fn size_line(&self, bytes: usize) -> String {
        let n = self.json.get("scenarios").map_or(0, |s| s.items().len());
        format!(
            "scenarios.json: {n} scenarios, nw0 ${}, {} KB",
            comma(self.nw0, 0),
            bytes / 1024
        )
    }
}

fn policy_prose() -> Json {
    obj(vec![
        ("alts", "20% of original at 4x/8x/16x/32x; after 2x a trail sells 25% per hit 40% below the running peak; 20% core; never below cost+15%".into()),
        ("btc", "no base trail; armed (Pi-cycle/Mayer/weekly RSI, or the manual arm file) -> one-shot exit 15% below peak to the core".into()),
        ("ladder", "old rule: sell 15% at +15% over cost, +8pp per +8%, until 20% nominal core".into()),
    ])
}

/// A replayed window's proxied coins and summary; `None` when it was skipped.
type Proxied = Option<(Vec<String>, Json)>;

fn all_finite(j: &Json) -> bool {
    match j {
        Json::Float(f) => f.is_finite(),
        Json::Arr(a) => a.iter().all(all_finite),
        Json::Obj(o) => o.iter().all(|(_, v)| all_finite(v)),
        _ => true,
    }
}

fn py_list_repr(xs: &[String]) -> String {
    let inner: Vec<String> = xs
        .iter()
        .map(|s| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", inner.join(", "))
}

/// Run every scenario, the ATH view and the forecast on `book`. `generated` is the
/// timestamp the file carries.
pub fn run(
    m: &Model,
    book: &Book,
    series: &BTreeMap<String, Series>,
    generated: &str,
) -> Result<Output, String> {
    let empty = Series::default();
    let hist = |s: &str| series.get(s).unwrap_or(&empty);
    let btc = series
        .get(MARKET)
        .ok_or_else(|| format!("KeyError: '{MARKET}'"))?;
    let armed_all = armed_series(&btc.close);
    let nw0 = pysum(book.coins.iter().map(|c| c.held * c.price))
        + book.cash_free
        + pysum(book.zones.iter().map(|z| z.quote));
    let coins_json = Json::Obj(
        book.coins
            .iter()
            .map(|c| {
                (
                    c.sym.clone(),
                    obj(vec![
                        ("held", c.held.into()),
                        ("price", c.price.into()),
                        ("cost", c.cost.into()),
                    ]),
                )
            })
            .collect(),
    );
    let zones_json = Json::Arr(
        book.zones
            .iter()
            .map(|z| {
                obj(vec![
                    ("sym", z.sym.as_str().into()),
                    ("price", z.price.into()),
                    ("quote", z.quote.into()),
                ])
            })
            .collect(),
    );
    let mut scenarios = Vec::new();
    let mut summaries: Vec<(String, Proxied)> = Vec::new();
    for sc in &m.scenarios {
        let t0 =
            parse_day(&sc.t0).ok_or_else(|| format!("Invalid isoformat string: '{}'", sc.t0))?;
        let n = sc.days;
        let mut factors: Vec<(String, Factors)> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for c in &book.coins {
            match path_factors(hist(&c.sym), t0, n) {
                Some(f) => factors.push((c.sym.clone(), f)),
                None => missing.push(c.sym.clone()),
            }
        }
        let alt_paths: Vec<&Factors> = m
            .alts
            .iter()
            .filter_map(|s| factors.iter().find(|(x, _)| x == s).map(|(_, f)| f))
            .collect();
        if !factors.iter().any(|(s, _)| s == MARKET) || alt_paths.is_empty() {
            scenarios.push(obj(vec![
                ("key", sc.key.as_str().into()),
                ("label", sc.label.as_str().into()),
                ("t0", sc.t0.as_str().into()),
                ("skipped", "no data".into()),
            ]));
            summaries.push((sc.label.clone(), None));
            continue;
        }
        let med = median_path(&alt_paths);
        for s in &missing {
            factors.push((s.clone(), med.clone()));
        }
        let i0 = at_or_before(&btc.days, t0).max(0) as usize;
        let mut armed: Vec<bool> = armed_all
            .iter()
            .skip(i0)
            .take((n + 1) as usize)
            .copied()
            .collect();
        armed.resize((n + 1) as usize, false);
        let fac = |s: &str| {
            factors
                .iter()
                .find(|(x, _)| x == s)
                .map(|(_, f)| f)
                .expect("every coin has a path by now")
        };
        let mut series_j = Vec::new();
        let mut cash_j = Vec::new();
        let mut events_j = Vec::new();
        let mut summary_j = Vec::new();
        let mut costs_j = Vec::new();
        let mut dex_j = Json::Arr(Vec::new());
        for pol in ["hold", "ladder", "live"] {
            // simulate reads the factors in book order; the first entry sets the length
            let ordered: Vec<(String, Factors)> = factors.clone();
            let r = simulate(pol, m, book, &ordered, &med, &armed)?;
            let floats = |v: &[f64]| Json::Arr(v.iter().map(|x| Json::Float(*x)).collect());
            series_j.push((pol.to_string(), floats(&r.nw)));
            cash_j.push((pol.to_string(), floats(&r.cash)));
            events_j.push((pol.to_string(), Json::Arr(r.events)));
            summary_j.push((pol.to_string(), summarize(&r.nw, nw0)?));
            costs_j.push((pol.to_string(), Json::Float(r.costs)));
            dex_j = floats(&r.dex);
        }
        let btc_x: Vec<Json> = (0..=n as usize)
            .map(|k| Json::Float(round(fac(MARKET)[k].1, 4)))
            .collect();
        let alt_x: Vec<Json> = (0..=n as usize)
            .map(|k| Json::Float(round(med[k].1, 4)))
            .collect();
        let summary = Json::Obj(summary_j);
        let rec = obj(vec![
            ("key", sc.key.as_str().into()),
            ("label", sc.label.as_str().into()),
            ("t0", sc.t0.as_str().into()),
            ("days", Json::Int(n)),
            ("desc", sc.desc.as_str().into()),
            (
                "proxied",
                Json::Arr(missing.iter().map(|s| s.as_str().into()).collect()),
            ),
            ("btc_x", Json::Arr(btc_x)),
            ("alt_median_x", Json::Arr(alt_x)),
            ("series", Json::Obj(series_j)),
            ("cash", Json::Obj(cash_j)),
            ("events", Json::Obj(events_j)),
            ("summary", summary.clone()),
            ("costs", Json::Obj(costs_j)),
            ("dex", dex_j),
        ]);
        scenarios.push(rec);
        summaries.push((sc.label.clone(), Some((missing, summary))));
    }
    let ath = ath_recovery(m, book)?;
    let fc = forecast(m, book)?;
    let fee_json = Json::Obj(
        book.coins
            .iter()
            .map(|c| (c.sym.clone(), Json::Float(m.fee(&c.sym))))
            .collect(),
    );
    let json = obj(vec![
        ("generated", generated.into()),
        (
            "book",
            obj(vec![
                ("coins", coins_json),
                ("cash_free", round(book.cash_free, 2).into()),
                ("zones", zones_json),
                ("nw0", round(nw0, 2).into()),
                ("dex_other", m.dex_other_usd.into()),
            ]),
        ),
        ("policy", policy_prose()),
        (
            "costs",
            obj(vec![("fee_pct", fee_json), ("slip_pct", m.slip_pct.into())]),
        ),
        ("scenarios", Json::Arr(scenarios)),
        ("ath", ath),
        ("forecast", fc),
    ]);

    // The reference raised on a division by zero (a zero price or cost); an infinity or
    // NaN anywhere in the output is that same failure, not a file to publish.
    if !all_finite(&json) {
        return Err("float division by zero".into());
    }
    let num = |j: Option<&Json>| j.and_then(Json::num).unwrap_or(0.0);
    let dist = json.get("forecast").and_then(|f| f.get("dist"));
    let invested = num(json.get("forecast").and_then(|f| f.get("invested_now")));
    let mut head = vec![format!(
        "  FORECAST cycle top, bot book + ${} new (invested now ${}):",
        comma(m.new_cash_usd, 0),
        comma(invested, 0)
    )];
    for k in [
        "hold_top",
        "policy_top",
        "keep_hold",
        "keep_policy",
        "dex_top",
    ] {
        let d = dist.and_then(|x| x.get(k));
        let f = |q: &str| num(d.and_then(|x| x.get(q)));
        head.push(format!(
            "    {} p10 ${}  p50 ${}  p90 ${}  mean ${}",
            ljust(k, 12),
            rjust(&comma(f("p10"), 0), 9),
            rjust(&comma(f("p50"), 0), 9),
            rjust(&comma(f("p90"), 0), 10),
            rjust(&comma(f("mean"), 0), 9)
        ));
    }
    let mut tail = Vec::new();
    if let Some(Json::Arr(levels)) = json.get("ath").and_then(|a| a.get("levels")) {
        for lv in levels {
            let f = |q: &str| num(lv.get(q));
            tail.push(format!(
                "  ATH level {} hold ${} policy ${} | kept after the bear: hold ${} policy ${}",
                ljust(lv.get("label").and_then(Json::as_str).unwrap_or(""), 26),
                rjust(&comma(f("hold"), 0), 10),
                rjust(&comma(f("policy"), 0), 10),
                rjust(&comma(f("keep_hold"), 0), 9),
                rjust(&comma(f("keep_policy"), 0), 9)
            ));
        }
    }
    for (label, s) in summaries {
        match s {
            None => tail.push(format!("  {label}: skipped")),
            Some((proxied, summary)) => {
                let x = |pol: &str, k: &str| {
                    summary
                        .get(pol)
                        .and_then(|p| p.get(k))
                        .map(Json::py_str)
                        .unwrap_or_default()
                };
                let prox = if proxied.is_empty() {
                    "-".to_string()
                } else {
                    py_list_repr(&proxied)
                };
                tail.push(format!(
                    "  {} proxied {prox} | hold peak {}x end {}x | ladder end {}x | live peak {}x end {}x kept {}",
                    ljust(&label, 16),
                    x("hold", "peak_x"),
                    x("hold", "end_x"),
                    x("ladder", "end_x"),
                    x("live", "peak_x"),
                    x("live", "end_x"),
                    x("live", "kept_of_peak")
                ));
            }
        }
    }
    Ok(Output {
        json,
        head,
        tail,
        nw0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_replica_sells_new_rungs_once_up_to_the_cap() {
        let mut l = Ladder::default();
        assert_eq!(l.sells(1.10, 1.0), 0.0);
        assert_eq!(l.sells(1.16, 1.0), 15.0);
        assert_eq!(l.sells(1.17, 1.0), 0.0, "same rung");
        assert_eq!(l.sells(1.40, 1.0), 24.0, "three more rungs");
        assert_eq!(l.sells(1.0, 1.0), 0.0);
        assert_eq!(l.hw, 0, "back under the first rung re-arms");
    }

    #[test]
    fn days_parse_strictly() {
        assert_eq!(parse_day("1970-01-02"), Some(1));
        assert_eq!(parse_day("2021-02-30"), None);
        assert_eq!(parse_day("2021-2-3"), None);
    }

    #[test]
    fn path_factors_forward_fill_and_need_full_coverage() {
        let s = Series {
            days: vec![0, 1, 3],
            low: vec![1.0, 2.0, 4.0],
            close: vec![2.0, 4.0, 8.0],
        };
        let f = path_factors(&s, 0, 3).unwrap();
        assert_eq!(f, vec![(0.5, 1.0), (1.0, 2.0), (1.0, 2.0), (2.0, 4.0)]);
        assert!(path_factors(&s, 0, 4).is_none());
        assert!(path_factors(&s, -1, 1).is_none());
    }
}
