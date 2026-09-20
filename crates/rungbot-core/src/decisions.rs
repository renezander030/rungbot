//! The decision log — so the tool can answer **"why did nothing happen?"**
//!
//! A ladder is quiet most of the time. Without a record of *why* it was quiet, a
//! correctly-disciplined run and a broken one look identical: both print nothing. This
//! module turns every run into one line per coin, including the coins that did nothing
//! and the reason they did nothing.
//!
//! Pure. Appending the lines to a file is the CLI's job.

use serde::{Deserialize, Serialize};

use crate::analyze::{Outcome, Side, SkipReason};
use crate::fmt::g;
use crate::time::iso8601;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Buy,
    Sell,
    Hold,
    Error,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Buy => "BUY",
            Kind::Sell => "SELL",
            Kind::Hold => "HOLD",
            Kind::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub ts: f64,
    pub at: String,
    pub sym: String,
    pub kind: Kind,
    pub detail: String,
}

impl Decision {
    /// The one-line log form: `2026-09-20T17:00:00+00:00 BTC HOLD no new rung crossed`.
    pub fn line(&self) -> String {
        format!(
            "{} {} {} {}",
            self.at,
            self.sym,
            self.kind.as_str(),
            self.detail
        )
    }
}

fn skip_detail(r: &SkipReason) -> String {
    match r {
        SkipReason::NoNewRung => "no new rung crossed".into(),
        SkipReason::NoData => "no 24h change from the venue; ladder held".into(),
        SkipReason::NoCostBasis => "no cost basis, so the sell side cannot be judged".into(),
        SkipReason::KnifeFloor { chg, floor } => {
            format!(
                "past the knife floor: {chg:+.1}% in 24h is beyond -{}%",
                g(*floor)
            )
        }
        SkipReason::WindowLocked { dir } => {
            format!("{dir}-locked: the directional window is still open")
        }
        SkipReason::Breaker { pnl, days } => {
            format!(
                "circuit breaker: {pnl:+.1}% vs entry for over {}d, dip-buys frozen",
                g(*days)
            )
        }
        SkipReason::CapReached { deployed, cap } => {
            format!("budget spent: {deployed:.0}% deployed of a {cap:.0}% cap")
        }
        SkipReason::BelowMinTrade { want, min } => {
            format!(
                "too small to bother: {want:.1}% is under the {}% minimum",
                g(*min)
            )
        }
        SkipReason::CoreProtected { sold, core } => {
            format!(
                "core protected: {sold:.0}% sold, and {}% is never sold",
                g(*core)
            )
        }
        SkipReason::PolicyGoverns => {
            "bull policy governs this coin; the sell ladder is dormant".into()
        }
    }
}

/// One decision per coin in this run, plus one per error.
pub fn from_outcome(out: &Outcome, now: f64) -> Vec<Decision> {
    let at = iso8601(now);
    let mut decisions = Vec::new();

    for t in out.buys.iter().chain(out.sells.iter()) {
        let (verb, unit) = match t.side {
            Side::Buy => (Kind::Buy, "of base"),
            Side::Sell => (Kind::Sell, "of position"),
        };
        decisions.push(Decision {
            ts: now,
            at: at.clone(),
            sym: t.row.sym.clone(),
            kind: verb,
            detail: format!(
                "rung {} at {}% -> {:.0}% {unit}{}",
                t.rung,
                g(t.threshold),
                t.pct,
                if t.capped { " (capped)" } else { "" }
            ),
        });
    }

    let acted: Vec<&str> = out
        .buys
        .iter()
        .chain(out.sells.iter())
        .map(|t| t.row.sym.as_str())
        .collect();

    for skip in &out.skips {
        if acted.contains(&skip.sym.as_str()) {
            continue; // it did something; that is the more interesting line
        }
        decisions.push(Decision {
            ts: now,
            at: at.clone(),
            sym: skip.sym.clone(),
            kind: Kind::Hold,
            detail: skip_detail(&skip.reason),
        });
    }

    for e in &out.errors {
        let sym = e.split([':', ' ']).next().unwrap_or("-").to_string();
        decisions.push(Decision {
            ts: now,
            at: at.clone(),
            sym,
            kind: Kind::Error,
            detail: e.clone(),
        });
    }

    decisions
}

/// `{kind: count}` over a slice of decisions, for a one-line summary.
pub fn counts(decisions: &[Decision]) -> (usize, usize, usize, usize) {
    let c = |k: Kind| decisions.iter().filter(|d| d.kind == k).count();
    (c(Kind::Buy), c(Kind::Sell), c(Kind::Hold), c(Kind::Error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::{analyze, Price};
    use crate::config::{Coin, Config, Settings, Venue};
    use std::collections::BTreeMap;

    fn cfg() -> Config {
        Config::new(
            vec![Coin {
                symbol: "AAA".into(),
                venue: Venue::Binance,
                pair: "AAAUSDT".into(),
                name: String::new(),
                entry: Some(100.0),
                bands: None,
            }],
            Settings::default(),
        )
        .unwrap()
    }

    fn px(price: f64, chg: f64) -> BTreeMap<String, Price> {
        let mut m = BTreeMap::new();
        m.insert(
            "AAA".to_string(),
            Price {
                price,
                chg_24h: Some(chg),
            },
        );
        m
    }

    #[test]
    fn a_quiet_run_still_explains_itself() {
        let out = analyze(&cfg(), &px(100.0, 0.5), &BTreeMap::new(), 1_700_000_000.0);
        assert!(out.buys.is_empty() && out.sells.is_empty());
        let d = from_outcome(&out, 1_700_000_000.0);
        assert_eq!(d.len(), 1, "silence is still a decision");
        assert_eq!(d[0].kind, Kind::Hold);
        assert!(d[0].detail.contains("no new rung"), "{}", d[0].detail);
    }

    #[test]
    fn a_knife_floor_hold_names_the_floor() {
        let out = analyze(&cfg(), &px(40.0, -60.0), &BTreeMap::new(), 1_700_000_000.0);
        let d = from_outcome(&out, 1_700_000_000.0);
        let hold = d.iter().find(|x| x.kind == Kind::Hold).expect("a hold");
        assert!(hold.detail.contains("knife floor"), "{}", hold.detail);
        assert!(
            hold.detail.contains("-60.0%"),
            "it quotes the actual move: {}",
            hold.detail
        );
    }

    #[test]
    fn an_action_is_logged_instead_of_a_hold() {
        let out = analyze(&cfg(), &px(88.0, -12.0), &BTreeMap::new(), 1_700_000_000.0);
        let d = from_outcome(&out, 1_700_000_000.0);
        assert_eq!(d.len(), 1, "one line for the coin");
        assert_eq!(d[0].kind, Kind::Buy);
        assert!(d[0].detail.contains("rung 1"), "{}", d[0].detail);
        assert!(d[0].line().starts_with("2023-11-14T"), "{}", d[0].line());
    }

    #[test]
    fn errors_become_their_own_lines() {
        let out = analyze(&cfg(), &BTreeMap::new(), &BTreeMap::new(), 1_700_000_000.0);
        let d = from_outcome(&out, 1_700_000_000.0);
        assert!(d.iter().any(|x| x.kind == Kind::Error));
        assert_eq!(counts(&d).3, 1, "one error counted");
    }
}
