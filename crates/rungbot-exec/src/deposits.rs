//! The deposit check: money that arrives from outside the bot starts a run within a
//! minute, not at the next half-hour tick.
//!
//! Neither Revolut X nor Gate pushes balance changes (both APIs only answer requests), so
//! the event is the difference between two reads. Each check reads every routed venue's
//! balances once, read-only, and keeps two things per venue in `deposits-state.json` in
//! `state_dir`: the free amount of each cash asset ([`CASH`]) and the total (free plus
//! locked) of every other asset. A cash asset whose free amount rose by `deploy_min_usd`
//! or more since the previous check is an inflow: a deposit, or a transfer between the
//! venues.
//!
//! Cash the bot moves itself is not an inflow:
//! - when the order journal or the decision log changed since the previous check, a run
//!   (or another writer) cancelled, placed, converted or staged in between, and the check
//!   records the new amounts as they are;
//! - when a coin's total on a venue fell since the previous check, a sell filled there,
//!   and the cash that venue gained is its proceeds.
//!
//! Either way the next run, at :00 or :30 at the latest, picks up what changed.
//!
//! The first check of a venue only records it. A venue that could not be read keeps its
//! previous amounts, so a deposit that lands while it is unreachable is still seen once
//! it answers. A check that finds a run holding the run lock reads nothing and leaves
//! the state as it was.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::balances::{self, Read};
use crate::run::config::RunConfig;
use crate::store::RunLock;
use crate::venue::Venue;

/// The cash assets: the onramp's EUR and the quote assets the ladders spend.
pub const CASH: [&str; 4] = ["EUR", "USD", "USDC", "USDT"];

/// Amounts per venue, then per asset.
pub type Amounts = BTreeMap<String, BTreeMap<String, f64>>;

/// What one check saw: the free cash, and the total of every other asset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Seen {
    pub cash: Amounts,
    pub coins: Amounts,
}

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

/// What this read shows; a venue that failed is left out.
pub fn seen(read: &Read) -> Seen {
    let mut s = Seen::default();
    for (name, got) in read {
        let Ok(b) = got else {
            continue;
        };
        let cash = CASH
            .iter()
            .map(|a| (a.to_string(), b.get(*a).map_or(0.0, |x| x.free)))
            .collect();
        let coins = b
            .iter()
            .filter(|(a, _)| !CASH.contains(&a.as_str()))
            .map(|(a, x)| (a.clone(), x.free + x.locked))
            .filter(|(_, total)| *total > 0.0)
            .collect();
        s.cash.insert(name.clone(), cash);
        s.coins.insert(name.clone(), coins);
    }
    s
}

/// Whether a coin's total fell between two reads of one venue: a sell filled.
fn sold(before: &BTreeMap<String, f64>, after: &BTreeMap<String, f64>) -> bool {
    before
        .iter()
        .any(|(a, b)| after.get(a).copied().unwrap_or(0.0) < b - b.abs() * 1e-9)
}

/// The inflows between the previous check and this one, and the state to keep: this
/// check's amounts for every venue it read, the previous ones for a venue it did not.
/// With `bot_acted` (a run or another writer moved cash in between) nothing is compared.
pub fn compare(prev: &Seen, now: &Seen, min: f64, bot_acted: bool) -> (Vec<Inflow>, Seen) {
    let mut found = Vec::new();
    if !bot_acted {
        for (venue, assets) in &now.cash {
            let (Some(before), Some(coins_before)) = (prev.cash.get(venue), prev.coins.get(venue))
            else {
                continue;
            };
            if now.coins.get(venue).is_some_and(|c| sold(coins_before, c)) {
                continue;
            }
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
    }
    let mut keep = prev.clone();
    keep.cash.extend(now.cash.clone());
    keep.coins.extend(now.coins.clone());
    (found, keep)
}

/// The previous check: what it saw and when; nothing before the first check.
pub fn load(path: &Path) -> Result<(Seen, f64), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Seen::default(), 0.0)),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let amounts = |key: &str| -> Amounts {
        v.get(key)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(venue, assets)| {
                let a = assets
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(a, x)| Some((a.clone(), x.as_f64()?)))
                    .collect();
                (venue.clone(), a)
            })
            .collect()
    };
    let seen = Seen {
        cash: amounts("cash"),
        coins: amounts("coins"),
    };
    Ok((seen, v.get("ts").and_then(Value::as_f64).unwrap_or(0.0)))
}

/// When the order journal or the decision log was last written: the last time a run, or
/// another writer, cancelled, placed, converted or staged. 0 when neither exists.
pub fn bot_wrote(cfg: &RunConfig) -> f64 {
    [cfg.journal_path(), cfg.decisions_path()]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .fold(0.0, f64::max)
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
    let (prev, prev_ts) = load(&path)?;
    let bot_acted = bot_wrote(cfg) >= prev_ts;
    let (inflows, keep) = compare(&prev, &seen(&read), cfg.deploy_min_usd.max(1.0), bot_acted);
    let state = json!({ "ts": ts, "cash": keep.cash, "coins": keep.coins });
    balances::write_private(&path, &state)?;
    drop(lock);
    Ok(Outcome::Checked { inflows, failed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venue::Balance;

    fn amounts(rows: &[(&str, &str, f64)]) -> Amounts {
        let mut c = Amounts::new();
        for (venue, asset, x) in rows {
            c.entry(venue.to_string())
                .or_default()
                .insert(asset.to_string(), *x);
        }
        c
    }

    /// A read: cash rows, coin rows; every venue with cash has a (maybe empty) coin map.
    fn read_of(cash: &[(&str, &str, f64)], coins: &[(&str, &str, f64)]) -> Seen {
        let mut s = Seen {
            cash: amounts(cash),
            coins: amounts(coins),
        };
        for v in s.cash.keys() {
            s.coins.entry(v.clone()).or_default();
        }
        s
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rungbot-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const AKT: (&str, &str, f64) = ("revx", "AKT", 4472.0);

    #[test]
    fn the_first_check_of_a_venue_only_records_it() {
        let now = read_of(&[("revx", "EUR", 707.0), ("revx", "USD", 3.56)], &[AKT]);
        let (found, keep) = compare(&Seen::default(), &now, 25.0, false);
        assert!(found.is_empty());
        assert_eq!(keep, now);
    }

    #[test]
    fn a_eur_deposit_on_revx_is_an_inflow() {
        let prev = read_of(&[("revx", "EUR", 0.0), ("revx", "USD", 3.56)], &[AKT]);
        let now = read_of(&[("revx", "EUR", 707.0), ("revx", "USD", 3.56)], &[AKT]);
        let (found, keep) = compare(&prev, &now, 25.0, false);
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
        let prev = read_of(&[("revx", "EUR", 700.0), ("revx", "USD", 100.0)], &[AKT]);
        let now = read_of(&[("revx", "EUR", 0.0), ("revx", "USD", 124.99)], &[AKT]);
        let (found, keep) = compare(&prev, &now, 25.0, false);
        assert!(found.is_empty());
        assert_eq!(keep, now);
    }

    #[test]
    fn an_asset_the_last_check_did_not_hold_counts_from_zero() {
        let prev = read_of(&[("gate", "USDT", 1.0)], &[]);
        let now = read_of(&[("gate", "USDC", 166.4), ("gate", "USDT", 1.0)], &[]);
        let (found, _) = compare(&prev, &now, 25.0, false);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].asset.as_str(), found[0].before), ("USDC", 0.0));
    }

    #[test]
    fn an_unread_venue_keeps_its_amounts_until_it_answers_again() {
        let prev = read_of(&[("gate", "USDT", 10.0), ("revx", "EUR", 0.0)], &[AKT]);
        // revx did not answer: gate alone is compared, revx keeps its last amounts
        let (found, keep) = compare(&prev, &read_of(&[("gate", "USDT", 10.0)], &[]), 25.0, false);
        assert!(found.is_empty());
        assert_eq!(keep, prev);
        // the deposit that landed meanwhile is seen once revx answers
        let back = read_of(&[("gate", "USDT", 10.0), ("revx", "EUR", 707.0)], &[AKT]);
        let (found, _) = compare(&keep, &back, 25.0, false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].venue, "revx");
    }

    #[test]
    fn cash_a_run_freed_or_staged_is_no_inflow() {
        // 2026-10-07 02:42: the run before rolled $7482.58 of zones, then staged USDC
        let prev = read_of(&[("revx", "USD", 3.66), ("revx", "USDC", 0.0)], &[AKT]);
        let now = read_of(&[("revx", "USD", 7486.24), ("revx", "USDC", 166.4)], &[AKT]);
        let (found, keep) = compare(&prev, &now, 25.0, true);
        assert!(found.is_empty());
        assert_eq!(keep, now);
        // the next check compares from what the run left
        let (found, _) = compare(&keep, &now, 25.0, false);
        assert!(found.is_empty());
    }

    #[test]
    fn a_filled_sell_frees_proceeds_not_an_inflow_but_another_venue_still_counts() {
        let prev = read_of(
            &[("gate", "USDC", 0.0), ("revx", "USD", 2.34)],
            &[("gate", "MED", 900.0), AKT],
        );
        // an AKT sell filled on revx (+$60 USD) while the Gate top-up arrived
        let now = read_of(
            &[("gate", "USDC", 166.4), ("revx", "USD", 62.34)],
            &[("gate", "MED", 900.0), ("revx", "AKT", 4392.0)],
        );
        let (found, _) = compare(&prev, &now, 25.0, false);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].venue.as_str(), found[0].asset.as_str()),
            ("gate", "USDC")
        );
    }

    #[test]
    fn a_state_from_before_coins_were_kept_records_first() {
        let prev = Seen {
            cash: amounts(&[("revx", "EUR", 0.0)]),
            coins: Amounts::new(),
        };
        let now = read_of(&[("revx", "EUR", 707.0)], &[AKT]);
        let (found, keep) = compare(&prev, &now, 25.0, false);
        assert!(found.is_empty());
        assert_eq!(keep, now);
    }

    #[test]
    fn seen_keeps_free_cash_and_coin_totals_and_leaves_a_failed_venue_out() {
        let bal = |free: f64, locked: f64| Balance { free, locked };
        let read: Read = vec![
            ("gate".into(), Err("gate: timeout".into())),
            (
                "revx".into(),
                Ok([
                    ("AKT".to_string(), bal(4400.0, 72.0)),
                    ("BTC".to_string(), bal(0.0, 0.0)),
                    ("EUR".to_string(), bal(707.0, 0.0)),
                    ("USD".to_string(), bal(3.56, 7244.05)),
                ]
                .into_iter()
                .collect()),
            ),
        ];
        assert_eq!(
            seen(&read),
            read_of(
                &[
                    ("revx", "EUR", 707.0),
                    ("revx", "USD", 3.56),
                    ("revx", "USDC", 0.0),
                    ("revx", "USDT", 0.0),
                ],
                &[AKT],
            )
        );
    }

    #[test]
    fn the_state_survives_a_round_trip_and_a_missing_file_reads_empty() {
        let dir = temp("deposits-state");
        let path = dir.join("deposits-state.json");
        assert_eq!(load(&path).unwrap(), (Seen::default(), 0.0));
        let s = read_of(&[("gate", "USDT", 0.5), ("revx", "EUR", 707.0)], &[AKT]);
        let state = json!({ "ts": 1.5, "cash": s.cash, "coins": s.coins });
        balances::write_private(&path, &state).unwrap();
        assert_eq!(load(&path).unwrap(), (s, 1.5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bot_wrote_follows_the_journal() {
        let dir = temp("deposits-wrote");
        let cfg = RunConfig {
            state_dir: dir.clone(),
            ..Default::default()
        };
        assert_eq!(bot_wrote(&cfg), 0.0);
        std::fs::write(cfg.journal_path(), "{}").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert!((bot_wrote(&cfg) - now).abs() < 60.0);
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
