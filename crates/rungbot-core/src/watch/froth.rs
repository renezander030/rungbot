//! Froth watch: the fast crowding signals the slow regime label cannot see.
//!
//! The regime label runs on 100- and 200-day averages; a monthly market report is slower
//! still. Neither sees a warm market build between runs. Once a day this reads four
//! crowding signals and says so when their count changes level:
//!
//! | signal          | fires when                                                  |
//! |-----------------|-------------------------------------------------------------|
//! | `fear_greed`    | the Fear & Greed index is 75+ (extreme greed)               |
//! | `funding`       | 30-period average BTC perp funding >= 0.03%/8h, none < 0    |
//! | `open_interest` | BTC open interest within 2% of its 30-day high              |
//! | `mayer`         | BTC / its 200-day average >= 1.8                            |
//!
//! 0 firing is `calm`, 1 `elevated`, 2 `warm`, 3+ `hot`. A source that fails reads as not
//! firing, with the error as its text: a dead endpoint never manufactures a hot market.
//!
//! It also refreshes the **BTC blow-off arming** the sell policy reads: the Pi-cycle
//! ratio (111-day average / 2 x 350-day average) >= 0.95 arms, and stays armed 14 days
//! after the last armed read. Mayer >= 2.4 and weekly RSI >= 85 are shown as context
//! only; on their own they armed far too early in past cycles.
//!
//! Not a trading signal: it changes no order. It mails once per level change.

use super::json::{obj, Json};
use super::pyfmt::{comma, fixed, floordiv, g6, ljust, repr, round, signed, sum};
use super::{upper, utc_minute};
use crate::indicators::rsi;

pub const FNG_HOT: i64 = 75;
pub const FUNDING_HOT: f64 = 0.0003;
pub const OI_NEAR_HIGH: f64 = 0.98;
pub const MAYER_HOT: f64 = 1.80;
pub const PI_ARM: f64 = 0.95;
pub const MAYER_ARM: f64 = 2.4;
pub const WRSI_ARM: f64 = 85.0;
pub const ARM_LATCH_DAYS: f64 = 14.0;

/// One signal: `(name, fired, reading)`.
pub type Signal = (&'static str, bool, String);

fn key_error(k: &str) -> String {
    format!("'{k}'")
}

const INDEX_ERROR: &str = "list index out of range";

fn py_int(j: &Json) -> Result<i64, String> {
    match j {
        Json::Int(i) => Ok(*i),
        Json::Float(f) if f.is_finite() => Ok(f.trunc() as i64),
        Json::Bool(b) => Ok(*b as i64),
        Json::Str(s) => s
            .trim()
            .replace('_', "")
            .parse()
            .map_err(|_| format!("invalid literal for int() with base 10: '{s}'")),
        other => Err(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            type_name(other)
        )),
    }
}

fn py_float(j: &Json) -> Result<f64, String> {
    j.to_float().ok_or_else(|| match j {
        Json::Str(s) => format!("could not convert string to float: '{s}'"),
        other => format!(
            "float() argument must be a string or a real number, not '{}'",
            type_name(other)
        ),
    })
}

fn type_name(j: &Json) -> &'static str {
    match j {
        Json::Null => "NoneType",
        Json::Bool(_) => "bool",
        Json::Int(_) => "int",
        Json::Float(_) => "float",
        Json::Str(_) => "str",
        Json::Arr(_) => "list",
        Json::Obj(_) => "dict",
    }
}

fn field<'a>(j: &'a Json, k: &str) -> Result<&'a Json, String> {
    j.get(k).ok_or_else(|| key_error(k))
}

fn fear_greed(body: &Json) -> Result<(bool, String), String> {
    let d = field(body, "data")?.items();
    let vals = d
        .iter()
        .map(|x| field(x, "value").and_then(py_int))
        .collect::<Result<Vec<i64>, String>>()?;
    let v0 = *vals.first().ok_or(INDEX_ERROR)?;
    let cls = field(&d[0], "value_classification")?.py_str();
    let avg = floordiv(vals.iter().sum(), vals.len() as i64);
    Ok((v0 >= FNG_HOT, format!("{v0} ({cls}), 7d avg {avg}")))
}

fn funding(body: &Json) -> Result<(bool, String), String> {
    let rates = body
        .items()
        .iter()
        .map(|x| field(x, "fundingRate").and_then(py_float))
        .collect::<Result<Vec<f64>, String>>()?;
    if rates.is_empty() {
        return Err("division by zero".into());
    }
    let avg = sum(rates.iter().copied()) / rates.len() as f64;
    let neg = rates.iter().filter(|r| **r < 0.0).count();
    Ok((
        avg >= FUNDING_HOT && neg == 0,
        format!(
            "30-period avg {}%/8h, {neg}/30 negative",
            signed(avg * 100.0, 4)
        ),
    ))
}

fn open_interest(body: &Json) -> Result<(bool, String), String> {
    let oi = body
        .items()
        .iter()
        .map(|x| field(x, "sumOpenInterest").and_then(py_float))
        .collect::<Result<Vec<f64>, String>>()?;
    let last = *oi.last().ok_or(INDEX_ERROR)?;
    let hi = oi.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    Ok((
        last >= hi * OI_NEAR_HIGH,
        format!(
            "{} BTC, {}% of 30d high",
            comma(last, 0),
            fixed(last / hi * 100.0, 1)
        ),
    ))
}

fn mayer(closes: &[f64]) -> Result<(bool, String), String> {
    let last = *closes.last().ok_or(INDEX_ERROR)?;
    let tail = &closes[closes.len().saturating_sub(200)..];
    let m = last / (sum(tail.iter().copied()) / 200.0);
    Ok((
        m >= MAYER_HOT,
        format!("{} (spot ${})", fixed(m, 3), comma(last, 0)),
    ))
}

fn reading(r: Result<(bool, String), String>) -> (bool, String) {
    r.unwrap_or_else(|e| (false, format!("unavailable: {e}")))
}

/// The four signals from what each source returned. `history` is BTC's last 220 daily
/// closes, for the Mayer multiple.
pub fn read_signals(
    fng: &Result<Json, String>,
    funding_body: &Result<Json, String>,
    oi_body: &Result<Json, String>,
    history: &Result<Vec<f64>, String>,
) -> Vec<Signal> {
    let pass = |r: &Result<Json, String>, f: fn(&Json) -> Result<(bool, String), String>| {
        reading(r.as_ref().map_err(Clone::clone).and_then(f))
    };
    let (a, at) = pass(fng, fear_greed);
    let (b, bt) = pass(funding_body, funding);
    let (c, ct) = pass(oi_body, open_interest);
    let (d, dt) = reading(
        history
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|h| mayer(h)),
    );
    vec![
        ("fear_greed", a, at),
        ("funding", b, bt),
        ("open_interest", c, ct),
        ("mayer", d, dt),
    ]
}

pub fn level_for(n: usize) -> &'static str {
    match n {
        0 => "calm",
        1 => "elevated",
        2 => "warm",
        _ => "hot",
    }
}

/// Last close of each ISO week (keyed by that week's Thursday), oldest first.
fn weekly(rows: &[(f64, f64)]) -> (Vec<f64>, Vec<i64>) {
    let key = |ms: f64| {
        let days = ((ms / 1000.0) as i64).div_euclid(86_400);
        let dow = (days % 7 + 7 + 3) % 7;
        days - dow + 3
    };
    let (mut weeks, mut keys): (Vec<f64>, Vec<i64>) = (Vec::new(), Vec::new());
    for (t, c) in rows {
        let k = key(*t);
        if keys.last() == Some(&k) {
            *weeks.last_mut().expect("in step with keys") = *c;
        } else {
            keys.push(k);
            weeks.push(*c);
        }
    }
    (weeks, keys)
}

/// The BTC blow-off arming read from daily candles `(open time in ms, close)`.
pub fn btc_arm(rows: &[(f64, f64)], now: f64) -> Result<Json, String> {
    let closes: Vec<f64> = rows.iter().map(|r| r.1).collect();
    let sma = |n: usize| sum(closes[closes.len().saturating_sub(n)..].iter().copied()) / n as f64;
    let den = 2.0 * sma(350);
    if den == 0.0 {
        return Err("float division by zero".into());
    }
    let pi = sma(111) / den;
    let last = *closes.last().ok_or(INDEX_ERROR)?;
    let s200 = sma(200);
    if s200 == 0.0 {
        return Err("float division by zero".into());
    }
    let m = last / s200;
    let (weeks, keys) = weekly(rows);
    let today = {
        let days = (now as i64).div_euclid(86_400);
        let dow = (days % 7 + 7 + 3) % 7;
        days - dow + 3
    };
    let done = if keys.last() == Some(&today) {
        &weeks[..weeks.len() - 1]
    } else {
        &weeks[..]
    };
    let wrsi = rsi(done, 14).unwrap_or(0.0);
    let mut hits: Vec<Json> = Vec::new();
    for (name, hit) in [
        ("pi", pi >= PI_ARM),
        ("mayer", m >= MAYER_ARM),
        ("wrsi", wrsi >= WRSI_ARM),
    ] {
        if hit {
            hits.push(name.into());
        }
    }
    let armed = pi >= PI_ARM;
    Ok(obj(vec![
        ("armed", armed.into()),
        ("fired", Json::Arr(hits)),
        ("pi", round(pi, 4).into()),
        ("mayer", round(m, 4).into()),
        ("wrsi", round(wrsi, 1).into()),
        ("spot", last.into()),
        ("ts", now.into()),
    ]))
}

const MEANING: [(&str, &str); 4] = [
    (
        "hot",
        "  Crowded. Longs paying, sentiment extended, no recent flush. This is the state\n  \
         where adding at spot has historically paid worst. It says nothing about the\n  \
         cycle, only about the next few weeks.",
    ),
    (
        "warm",
        "  Getting crowded. Worth knowing before any decision to add at market.",
    ),
    (
        "elevated",
        "  One signal stretched. Noise on its own; noted so a trend is visible.",
    ),
    (
        "calm",
        "  No crowding signals firing. Froth is not the current risk.",
    ),
];

fn num_or0(arm: &Json, k: &str) -> f64 {
    arm.get(k).and_then(Json::to_float).unwrap_or(0.0)
}

/// `(subject, text)`.
pub fn build_message(
    level: &str,
    prev: Option<&str>,
    sig: &[Signal],
    fired: usize,
    arm: &Json,
    arm_changed: bool,
    now: f64,
) -> (String, String) {
    let prev_up = upper(prev.filter(|p| !p.is_empty()).unwrap_or("unknown"));
    let mut l = vec![
        format!(
            "MARKET FROTH: {prev_up} -> {}  ({fired}/4 signals firing)",
            upper(level)
        ),
        String::new(),
        "SIGNALS".to_string(),
    ];
    for (k, hit, txt) in sig {
        l.push(format!(
            "  {} {} {txt}",
            if *hit { "HOT " } else { "    " },
            ljust(k, 14)
        ));
    }
    l.extend([
        String::new(),
        "WHAT THIS IS".into(),
        "  A daily read of the fast-moving crowding signals. The regime label runs on".into(),
        "  100d/200d averages and a monthly market report is slower, so neither can".into(),
        "  see froth build between runs. This changes NO orders and proposes no trade.".into(),
        String::new(),
        "WHAT IT MEANS".into(),
        MEANING
            .iter()
            .find(|(k, _)| *k == level)
            .map(|(_, v)| v.to_string())
            .unwrap_or_default(),
        String::new(),
    ]);
    if arm.truthy() {
        let effective = arm
            .get("effective")
            .or_else(|| arm.get("armed"))
            .is_some_and(Json::truthy);
        let fired_list = arm.get("fired").filter(|f| f.truthy());
        let mut line = format!(
            "  {} pi-cycle {} (arm >= {})   mayer {} (>= {})   weekly RSI {} (>= {})",
            if effective { "ARMED" } else { "     " },
            fixed(num_or0(arm, "pi"), 3),
            repr(PI_ARM),
            fixed(num_or0(arm, "mayer"), 2),
            repr(MAYER_ARM),
            fixed(num_or0(arm, "wrsi"), 0),
            g6(WRSI_ARM),
        );
        if let Some(f) = fired_list {
            let names: Vec<String> = f.items().iter().map(Json::py_str).collect();
            line.push_str(&format!("   fired: {}", names.join(", ")));
        }
        if let Some(e) = arm.get("error").filter(|e| e.truthy()) {
            line.push_str(&format!("   [{}]", e.py_str()));
        }
        l.extend([
            "BTC BLOW-OFF ARMING (sell policy: BTC trail tightens to the armed give-back while on)"
                .to_string(),
            line,
            String::new(),
        ]);
    }
    l.push(format!("Checked: {}", utc_minute(now)));
    let mut subject = format!(
        "FROTH {}: {fired}/4 signals ({} -> {level})",
        upper(level),
        prev.filter(|p| !p.is_empty()).unwrap_or("?")
    );
    if arm_changed {
        let head = if arm.get("armed").is_some_and(Json::truthy) {
            "BTC BLOW-OFF ARMED"
        } else {
            "BTC blow-off disarmed"
        };
        subject = format!("{head} | {subject}");
    }
    (subject, l.join("\n"))
}

/// What one run decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Same level, same arming: print `line`, write `state` (the refreshed arming read)
    /// unless it is `None`.
    Unchanged { line: String, state: Option<Json> },
    /// Mail it; on delivery write `state`.
    Send {
        subject: String,
        text: String,
        state: Json,
    },
}

/// Decide a run. `st` is the previous state (`{}` when missing or unreadable); `arm` is
/// [`btc_arm`]'s read or its error. Dry runs are the caller's: print, write nothing.
pub fn plan(
    sig: &[Signal],
    st: &Json,
    arm_read: Result<Json, String>,
    force: bool,
    now: f64,
) -> Plan {
    let fired = sig.iter().filter(|s| s.1).count();
    let level = level_for(fired);
    let prev_level = st.get("level");
    let prev_arm = st
        .get("btc_arm")
        .filter(|a| a.truthy())
        .cloned()
        .unwrap_or_else(Json::obj);
    let mut arm = match arm_read {
        Ok(a) => a,
        Err(e) => {
            let mut a = prev_arm.clone();
            if !a.is_obj() {
                a = Json::obj();
            }
            a.set("error", format!("arming unavailable: {e}").into());
            a
        }
    };
    let latch = if arm.get("armed").is_some_and(Json::truthy) {
        now + ARM_LATCH_DAYS * 86400.0
    } else {
        prev_arm.float_or0("armed_until").unwrap_or(0.0)
    };
    arm.set("armed_until", latch.into());
    let eff_now = arm.get("armed").is_some_and(Json::truthy) || now < latch;
    let eff_prev = prev_arm.get("armed").is_some_and(Json::truthy)
        || prev_arm.float_or0("ts").unwrap_or(0.0)
            < prev_arm.float_or0("armed_until").unwrap_or(0.0);
    arm.set("effective", eff_now.into());
    let arm_changed = eff_now != eff_prev;
    let has_error = arm.get("error").is_some_and(Json::truthy);
    let same_level = prev_level.is_some_and(|p| p.as_str() == Some(level));
    if same_level && !arm_changed && !force {
        let line = format!(
            "froth {level} ({fired}/4), BTC arm {} — unchanged, no alert.",
            if arm.get("armed").is_some_and(Json::truthy) {
                "ON"
            } else {
                "off"
            }
        );
        let state = (!has_error).then(|| {
            let mut s = st.clone();
            s.set("btc_arm", arm.clone());
            s
        });
        return Plan::Unchanged { line, state };
    }
    // `prev or 'unknown'`: a missing, null or empty level reads as unknown.
    let prev_text = prev_level.filter(|p| p.truthy()).map(Json::py_str);
    let (subject, text) = build_message(
        level,
        prev_text.as_deref(),
        sig,
        fired,
        &arm,
        arm_changed,
        now,
    );
    let mut state = st.clone();
    state.set("level", level.into());
    state.set("fired", Json::Int(fired as i64));
    state.set("epoch", Json::Int(now as i64));
    state.set(
        "signals",
        Json::Obj(
            sig.iter()
                .map(|(k, _, t)| (k.to_string(), t.as_str().into()))
                .collect(),
        ),
    );
    if !has_error {
        state.set("btc_arm", arm);
    }
    Plan::Send {
        subject,
        text,
        state,
    }
}
