//! The deposit check: new cash on a venue starts a run within a minute, not at the next
//! half-hour tick.
//!
//! Neither Revolut X nor Gate pushes balance changes (both APIs only answer requests), so
//! the event is the difference between two reads. Each check reads every routed venue's
//! balances once, read-only, keeps the free amount of each cash asset ([`CASH`]) and
//! compares it with the previous check's, kept in `deposits-state.json` in `state_dir`.
//! A cash asset whose free amount rose by `deploy_min_usd` or more is an inflow: a
//! deposit, a transfer between the venues, or cash a filled sell freed.
//!
//! The first check of a venue only records it. A venue that could not be read keeps its
//! previous amounts, so a deposit that lands while it is unreachable is still seen once
//! it answers. A check that finds a run holding the run lock reads nothing and leaves
//! the state as it was: the run spends and frees cash while it works.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::balances::{self, Read};
use crate::run::config::RunConfig;
use crate::store::RunLock;
use crate::venue::Venue;

/// The cash assets: the onramp's EUR and the quote assets the ladders spend.
pub const CASH: [&str; 4] = ["EUR", "USD", "USDC", "USDT"];

/// Free cash per venue, then per asset.
pub type Cash = BTreeMap<String, BTreeMap<String, f64>>;

/// A cash asset whose free amount rose by at least the threshold since the last check.
#[derive(Debug, Clone, PartialEq)]
pub struct Inflow {
    pub venue: String,
    pub asset: String,
    pub before: f64,
    pub after: f64,
}

impl Inflow {
    /// `INFLOW revx EUR: 0.00 -> 707.00 (+707.00)`
    pub fn line(&self) -> String {
        format!(
            "INFLOW {} {}: {:.2} -> {:.2} (+{:.2})",
            self.venue,
            self.asset,
            self.before,
            self.after,
            self.after - self.before
        )
    }
}

/// What one check found.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Another holder has the run lock; nothing was read.
    Skipped(String),
    /// The inflows since the last check (empty when none) and the venues not read.
    Checked {
        inflows: Vec<Inflow>,
        failed: Vec<(String, String)>,
    },
}

/// Where the check keeps the previous read.
pub fn state_path(cfg: &RunConfig) -> PathBuf {
    cfg.state_dir.join("deposits-state.json")
}

/// The free cash of every venue that was read; a venue that failed is left out.
pub fn cash(read: &Read) -> Cash {
    read.iter()
        .filter_map(|(name, got)| {
            let b = got.as_ref().ok()?;
            let assets = CASH
                .iter()
                .map(|a| (a.to_string(), b.get(*a).map_or(0.0, |x| x.free)))
                .collect();
            Some((name.clone(), assets))
        })
        .collect()
}

/// The inflows between the previous check and this one, and the state to keep: this
/// check's amounts for every venue it read, the previous ones for a venue it did not.
pub fn compare(prev: &Cash, now: &Cash, min: f64) -> (Vec<Inflow>, Cash) {
    let mut found = Vec::new();
    for (venue, assets) in now {
        let Some(before) = prev.get(venue) else {
            continue;
        };
        for (asset, after) in assets {
            let b = before.get(asset).copied().unwrap_or(0.0);
            if after - b >= min {
                found.push(Inflow {
                    venue: venue.clone(),
                    asset: asset.clone(),
                    before: b,
                    after: *after,
                });
            }
        }
    }
    let mut keep = prev.clone();
    keep.extend(now.iter().map(|(v, a)| (v.clone(), a.clone())));
    (found, keep)
}

/// The previous check's cash; empty before the first check.
pub fn load(path: &Path) -> Result<Cash, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Cash::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Cash::new();
    for (venue, assets) in v
        .get("cash")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let amounts = assets
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(a, x)| Some((a.clone(), x.as_f64()?)))
            .collect();
        out.insert(venue.clone(), amounts);
    }
    Ok(out)
}

/// One check: read every routed venue, compare with the last check, keep this one.
pub fn check<'a>(
    cfg: &RunConfig,
    client: impl Fn(&str) -> Result<&'a dyn Venue, String>,
    ts: f64,
) -> Result<Outcome, String> {
    let lock = match RunLock::acquire(&cfg.lock_path(), "rungbot-exec deposits", Duration::ZERO) {
        Ok(l) => l,
        Err(e) => return Ok(Outcome::Skipped(e)),
    };
    let read = balances::read(cfg, client);
    let failed = read
        .iter()
        .filter_map(|(n, got)| got.as_ref().err().map(|e| (n.clone(), e.clone())))
        .collect();
    let path = state_path(cfg);
    let prev = load(&path)?;
    let (inflows, keep) = compare(&prev, &cash(&read), cfg.deploy_min_usd.max(1.0));
    balances::write_private(&path, &json!({ "ts": ts, "cash": keep }))?;
    drop(lock);
    Ok(Outcome::Checked { inflows, failed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venue::Balance;

    fn amounts(rows: &[(&str, &str, f64)]) -> Cash {
        let mut c = Cash::new();
        for (venue, asset, x) in rows {
            c.entry(venue.to_string())
                .or_default()
                .insert(asset.to_string(), *x);
        }
        c
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rungbot-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_first_check_of_a_venue_only_records_it() {
        let now = amounts(&[("revx", "EUR", 707.0), ("revx", "USD", 3.56)]);
        let (found, keep) = compare(&Cash::new(), &now, 25.0);
        assert!(found.is_empty());
        assert_eq!(keep, now);
    }

    #[test]
    fn a_eur_deposit_on_revx_is_an_inflow() {
        let prev = amounts(&[
            ("gate", "USDT", 0.0),
            ("revx", "EUR", 0.0),
            ("revx", "USD", 3.56),
        ]);
        let now = amounts(&[
            ("gate", "USDT", 0.0),
            ("revx", "EUR", 707.0),
            ("revx", "USD", 3.56),
        ]);
        let (found, keep) = compare(&prev, &now, 25.0);
        assert_eq!(
            found,
            vec![Inflow {
                venue: "revx".into(),
                asset: "EUR".into(),
                before: 0.0,
                after: 707.0,
            }]
        );
        assert_eq!(found[0].line(), "INFLOW revx EUR: 0.00 -> 707.00 (+707.00)");
        assert_eq!(keep, now);
    }

    #[test]
    fn a_rise_below_the_threshold_or_a_fall_is_no_inflow() {
        let prev = amounts(&[("revx", "EUR", 700.0), ("revx", "USD", 100.0)]);
        let now = amounts(&[("revx", "EUR", 0.0), ("revx", "USD", 124.99)]);
        let (found, keep) = compare(&prev, &now, 25.0);
        assert!(found.is_empty());
        assert_eq!(keep, now);
    }

    #[test]
    fn an_asset_the_last_check_did_not_hold_counts_from_zero() {
        let prev = amounts(&[("gate", "USDT", 1.0)]);
        let now = amounts(&[("gate", "USDC", 500.0), ("gate", "USDT", 1.0)]);
        let (found, _) = compare(&prev, &now, 25.0);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].asset.as_str(), found[0].before), ("USDC", 0.0));
    }

    #[test]
    fn an_unread_venue_keeps_its_amounts_until_it_answers_again() {
        let prev = amounts(&[("gate", "USDT", 10.0), ("revx", "EUR", 0.0)]);
        // revx did not answer: gate alone is compared, revx keeps its last amounts
        let (found, keep) = compare(&prev, &amounts(&[("gate", "USDT", 10.0)]), 25.0);
        assert!(found.is_empty());
        assert_eq!(keep, prev);
        // the deposit that landed meanwhile is seen once revx answers
        let back = amounts(&[("gate", "USDT", 10.0), ("revx", "EUR", 707.0)]);
        let (found, _) = compare(&keep, &back, 25.0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].venue, "revx");
    }

    #[test]
    fn cash_counts_free_amounts_and_leaves_a_failed_venue_out() {
        let bal = |free: f64, locked: f64| Balance { free, locked };
        let read: Read = vec![
            ("gate".into(), Err("gate: timeout".into())),
            (
                "revx".into(),
                Ok([
                    ("AKT".to_string(), bal(4472.0, 0.0)),
                    ("EUR".to_string(), bal(707.0, 0.0)),
                    ("USD".to_string(), bal(3.56, 7244.05)),
                ]
                .into_iter()
                .collect()),
            ),
        ];
        assert_eq!(
            cash(&read),
            amounts(&[
                ("revx", "EUR", 707.0),
                ("revx", "USD", 3.56),
                ("revx", "USDC", 0.0),
                ("revx", "USDT", 0.0),
            ])
        );
    }

    #[test]
    fn the_state_survives_a_round_trip_and_a_missing_file_reads_empty() {
        let dir = temp("deposits-state");
        let path = dir.join("deposits-state.json");
        assert_eq!(load(&path).unwrap(), Cash::new());
        let c = amounts(&[("gate", "USDT", 0.5), ("revx", "EUR", 707.0)]);
        balances::write_private(&path, &json!({ "ts": 1.0, "cash": c })).unwrap();
        assert_eq!(load(&path).unwrap(), c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_check_stands_aside_while_a_run_holds_the_lock() {
        let dir = temp("deposits-lock");
        let cfg = RunConfig {
            state_dir: dir.clone(),
            run_lock: Some(dir.join("run.lock")),
            ..Default::default()
        };
        let _run = RunLock::acquire(&cfg.lock_path(), "test run", Duration::ZERO).unwrap();
        let out = check(
            &cfg,
            |_: &str| Err::<&dyn Venue, _>("no venue".to_string()),
            1.0,
        );
        assert!(matches!(out, Ok(Outcome::Skipped(_))));
        assert!(!state_path(&cfg).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
