//! The regime as a file: the cached reading and the day-by-day label history.
//!
//! [`crate::regime`] answers "what market is this" for one set of candles. The watchers
//! need two more things, and both have to survive between runs:
//!
//! * **The cached reading** ([`compute_state`]): the label, the BTC trend numbers,
//!   breadth and every coin's RUN gate, in the shape the state file has always had. A coin
//!   whose candles fail is kept with its error rather than dropped, and still counts in
//!   the breadth denominator.
//! * **The label history** ([`label_history`]): the label replayed over every day of
//!   history, so "how long has this held" is answerable on the first run, plus which
//!   stage of which label was already announced ([`pending_shift`], [`mark_notified`]).
//!
//! One deliberate change from the original: announcements were remembered per label
//! forever, so a label confirmed once stayed silent every later time it came back (a
//! bull confirmed in spring suppressed the autumn bull). Here, once a label is
//! **confirmed**, the announcements of every other label are forgotten: the next time
//! one of them returns it is a new cycle and is announced again. A provisional flip-flop
//! between two labels is still announced only once, as before.
//!
//! Pure: candles in, JSON out. Fetching and the file I/O are the CLI's job.

use super::json::{obj, Json};
use super::pyfmt;
use crate::regime::{market_label, running_from_series, sma, RegimeConfig};

/// Days of daily candles replayed for the history.
pub const HIST_DAYS: usize = 420;
/// Held days after which a label counts as confirmed.
pub const CONFIRM_DAYS: i64 = 14;
/// Hours a cached history is reused before it is replayed again.
pub const HIST_TTL_H: f64 = 12.0;
/// Hours a cached regime reading is reused.
pub const TTL_H: f64 = 6.0;

fn signals_json(closes: &[f64], cfg: RegimeConfig) -> (bool, Json) {
    let (running, sig) = running_from_series(closes, 1, cfg);
    let j = if sig.insufficient_history {
        obj(vec![("insufficient_history", Json::Bool(true))])
    } else {
        obj(vec![
            ("above_sma30", sig.above_sma30.into()),
            ("ret30_strong", sig.ret30_strong.into()),
            ("fresh_30d_high", sig.fresh_30d_high.into()),
            ("higher_lows", sig.higher_lows.into()),
        ])
    };
    (running, j)
}

const INDEX_ERROR: &str = "list index out of range";

fn mean(v: &[f64]) -> f64 {
    sma(v).unwrap_or(0.0)
}

fn tail(v: &[f64], n: usize) -> &[f64] {
    &v[v.len().saturating_sub(n)..]
}

/// The regime reading as the state file holds it.
///
/// `coins` is `(symbol, closes or the fetch error)` in routing order; `btc` is the BTC
/// market proxy's daily closes. `epoch` is the reading's time, whole seconds.
pub fn compute_state(
    coins: &[(String, Result<Vec<f64>, String>)],
    btc: &Result<Vec<f64>, String>,
    cfg: RegimeConfig,
    epoch: i64,
) -> Json {
    let mut out = Json::obj();
    let mut above30 = 0usize;
    for (sym, closes) in coins {
        let entry = match closes {
            Err(e) => obj(vec![
                ("running", false.into()),
                ("error", e.as_str().into()),
            ]),
            Ok(c) if c.is_empty() => obj(vec![
                ("running", false.into()),
                ("error", INDEX_ERROR.into()),
            ]),
            Ok(c) => {
                let (running, sig) = signals_json(c, cfg);
                let last = c[c.len() - 1];
                let sma30 = (c.len() >= 30).then(|| mean(tail(c, 30)));
                if sma30.is_some_and(|s| last > s) {
                    above30 += 1;
                }
                obj(vec![
                    ("running", running.into()),
                    ("signals", sig),
                    ("px", last.into()),
                    ("sma30", sma30.map(|s| pyfmt::round(s, 8)).into()),
                ])
            }
        };
        out.set(sym, entry);
    }
    let n = coins.len();
    let (label, btc_info) = match btc {
        Ok(b) if !b.is_empty() => (
            market_label(b, above30, n).as_str(),
            obj(vec![
                ("px", b[b.len() - 1].into()),
                ("sma100", pyfmt::round(mean(tail(b, 100)), 2).into()),
                ("sma200", pyfmt::round(mean(tail(b, 200)), 2).into()),
            ]),
        ),
        Ok(_) => ("unknown", obj(vec![("error", INDEX_ERROR.into())])),
        Err(e) => ("unknown", obj(vec![("error", e.as_str().into())])),
    };
    obj(vec![
        ("epoch", Json::Int(epoch)),
        ("market", label.into()),
        ("btc", btc_info),
        ("breadth_above_sma30", format!("{above30}/{n}").into()),
        ("coins", out),
    ])
}

/// Is a cached file younger than `ttl_h` hours? `epoch` missing or not a number is 0.
pub fn is_fresh(cached: &Json, now: f64, ttl_h: f64) -> bool {
    let epoch = cached.get("epoch").and_then(Json::num).unwrap_or(0.0);
    now - epoch < ttl_h * 3600.0
}

/// The market label for every day of history: day `i` (from 200) is labelled from the
/// BTC closes up to it and the breadth of `series` on that day.
///
/// `series` are the coins whose candles came back, `btc_all` the BTC closes. Both are
/// right-aligned to the shortest coin series; with 200 days or fewer, or a BTC series
/// that does not cover them, there is no history.
pub fn history_labels(series: &[Vec<f64>], btc_all: &[f64]) -> Vec<&'static str> {
    let n = series.iter().map(Vec::len).min().unwrap_or(0);
    let aligned: Vec<&[f64]> = series.iter().map(|v| tail(v, n)).collect();
    let btc: &[f64] = if n > 0 { tail(btc_all, n) } else { &[] };
    let mut labels = Vec::new();
    if n > 200 && btc.len() == n {
        for i in 200..=n {
            let breadth = aligned
                .iter()
                .filter(|c| i >= 30 && c[i - 1] > mean(&c[i - 30..i]))
                .count();
            labels.push(market_label(&btc[..i], breadth, aligned.len()).as_str());
        }
    }
    labels
}

/// What the history file held before this run.
#[derive(Debug, Clone, PartialEq)]
pub enum Prior {
    Missing,
    /// It exists but is not valid JSON.
    Unreadable,
    Parsed(Json),
}

impl Prior {
    fn object(&self) -> Option<&Json> {
        match self {
            Prior::Parsed(j) if j.is_obj() => Some(j),
            _ => None,
        }
    }
}

/// The result of one [`label_history`] call.
#[derive(Debug, Clone, PartialEq)]
pub struct History {
    pub json: Json,
    /// Write `json` back: false when the cache was served as it was.
    pub save: bool,
}

/// The label history, from the cache when it is younger than `ttl_h`, else replayed.
///
/// `fetch` is called only on a replay and returns `(coin series that came back, BTC)`.
/// When a replay yields no labels (feeds down, too little history) the previous history
/// is served with `stale: true` instead, if it had labels: an `unknown` pinned for half a
/// day would flip the sell policy back to the ladder.
pub fn label_history<F>(
    prior: &Prior,
    now: f64,
    ttl_h: f64,
    confirm_days: i64,
    force: bool,
    fetch: F,
) -> History
where
    F: FnOnce() -> (Vec<Vec<f64>>, Result<Vec<f64>, String>),
{
    if let (false, Some(cached)) = (force, prior.object()) {
        if is_fresh(cached, now, ttl_h) {
            return History {
                json: cached.clone(),
                save: false,
            };
        }
    }
    let (series, btc) = fetch();
    let btc_all = btc.unwrap_or_default();
    let labels = history_labels(&series, &btc_all);
    if labels.is_empty() {
        if let Some(p) = prior.object() {
            if p.get("labels").is_some_and(Json::truthy) {
                let mut stale = p.clone();
                stale.set("stale", true.into());
                return History {
                    json: stale,
                    save: false,
                };
            }
        }
    }
    let last = labels.last().copied();
    let held = match last {
        Some(l) => labels.iter().rev().take_while(|x| **x == l).count() as i64,
        None => 0,
    };
    let prev = last.and_then(|l| labels.iter().rev().find(|x| **x != l).copied());
    let label = last.unwrap_or("unknown");
    let confirmed = held >= confirm_days;
    let mut notified = match prior.object().and_then(|p| p.get("notified")) {
        Some(n) if n.is_obj() => n.clone(),
        _ => Json::obj(),
    };
    if confirmed && label != "unknown" {
        // A confirmed label closes every other label's cycle: forget their notices.
        if let Json::Obj(e) = &mut notified {
            e.retain(|(k, _)| k == label);
        }
    }
    let json = obj(vec![
        ("epoch", Json::Int(now as i64)),
        (
            "labels",
            Json::Arr(labels.iter().map(|l| (*l).into()).collect()),
        ),
        ("label", label.into()),
        ("held_days", Json::Int(held)),
        ("prev_label", prev.map(Json::from).unwrap_or(Json::Null)),
        ("confirmed", confirmed.into()),
        ("confirm_days", Json::Int(confirm_days)),
        ("days_covered", Json::Int(labels.len() as i64)),
        ("notified", notified),
    ]);
    History { json, save: true }
}

/// A label change that has not been announced at its current stage.
#[derive(Debug, Clone, PartialEq)]
pub struct Shift {
    pub label: String,
    pub prev_label: Option<String>,
    /// `provisional` (the label just changed) or `confirmed` (held `confirm_days`).
    pub stage: String,
    pub held_days: i64,
    pub confirm_days: i64,
    pub days_covered: i64,
}

impl Shift {
    /// The current stage of whatever the history says, announced or not.
    pub fn current(h: &Json) -> Shift {
        let int = |k: &str| h.get(k).and_then(Json::num).unwrap_or(0.0) as i64;
        Shift {
            label: h
                .get("label")
                .and_then(Json::as_str)
                .unwrap_or("unknown")
                .to_string(),
            prev_label: h.get("prev_label").and_then(Json::as_str).map(String::from),
            stage: if h.get("confirmed").is_some_and(Json::truthy) {
                "confirmed"
            } else {
                "provisional"
            }
            .into(),
            held_days: int("held_days"),
            confirm_days: int("confirm_days"),
            days_covered: int("days_covered"),
        }
    }

    pub fn to_json(&self) -> Json {
        obj(vec![
            ("label", self.label.as_str().into()),
            ("prev_label", self.prev_label.clone().into()),
            ("stage", self.stage.as_str().into()),
            ("held_days", Json::Int(self.held_days)),
            ("confirm_days", Json::Int(self.confirm_days)),
            ("days_covered", Json::Int(self.days_covered)),
        ])
    }
}

/// The shift to announce, or `None` when the label is unknown or its current stage (or
/// its confirmation) was already announced.
pub fn pending_shift(h: &Json) -> Option<Shift> {
    let s = Shift::current(h);
    if s.label == "unknown" {
        return None;
    }
    let done = h
        .get("notified")
        .and_then(|n| n.get(&s.label))
        .and_then(Json::as_str);
    if done == Some("confirmed") || done == Some(s.stage.as_str()) {
        return None;
    }
    Some(s)
}

/// Record that `label` was announced at `stage`.
pub fn mark_notified(h: &mut Json, label: &str, stage: &str) {
    if !h.get("notified").is_some_and(Json::is_obj) {
        h.set("notified", Json::obj());
    }
    if let Some(n) = h.get_mut("notified") {
        n.set(label, stage.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(labels: &[&str], notified: Json) -> Json {
        let last = labels.last().copied().unwrap_or("unknown");
        let held = labels.iter().rev().take_while(|x| **x == last).count() as i64;
        obj(vec![
            ("label", last.into()),
            ("prev_label", Json::Null),
            ("held_days", Json::Int(held)),
            ("confirmed", (held >= 3).into()),
            ("confirm_days", Json::Int(3)),
            ("days_covered", Json::Int(labels.len() as i64)),
            ("notified", notified),
        ])
    }

    #[test]
    fn a_stage_is_announced_once() {
        let mut h = hist(&["chop", "bull"], Json::obj());
        let s = pending_shift(&h).expect("a fresh flip is pending");
        assert_eq!(
            (s.label.as_str(), s.stage.as_str()),
            ("bull", "provisional")
        );
        mark_notified(&mut h, "bull", "provisional");
        assert_eq!(pending_shift(&h), None);
    }

    #[test]
    fn a_confirmation_suppresses_the_provisional_stage_too() {
        let h = hist(&["chop", "bull"], obj(vec![("bull", "confirmed".into())]));
        assert_eq!(pending_shift(&h), None);
    }
}
