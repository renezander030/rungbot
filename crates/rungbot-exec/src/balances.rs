//! The balance bridge: one read-only balance read per venue, written where the keyless
//! layers pick it up.
//!
//! `balances.json` in `state_dir` is what the watchers read (`watch.balances`):
//! `{"ts": .., "venues": {"gate": {"USDT": {"free": .., "locked": ..}, ..}, ..}}`, and a
//! venue that could not be read carries `{"error": ".."}` instead of its assets.
//!
//! The optional start book (`--book FILE`) is what `rungbot backtest monthly --book`
//! replays: `held` per watchlist coin (free plus locked on every venue), `stable` per
//! venue (free USD, USDC and USDT, the cash the replayed ladder can spend) and `counts`
//! (how many coins each venue's stable bag serves). It is written only when every venue
//! was read, so a failed read never leaves a half book behind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::run::config::RunConfig;
use crate::venue::{Balance, Venue};

/// The quote assets counted as a venue's stable cash.
pub const STABLES: [&str; 3] = ["USD", "USDC", "USDT"];

/// One venue's read: its balances, or why they could not be read.
pub type Read = Vec<(String, Result<BTreeMap<String, Balance>, String>)>;

/// The venues the routing names, in name order.
pub fn venues(cfg: &RunConfig) -> Vec<String> {
    let mut v: Vec<String> = cfg
        .routing
        .iter()
        .map(|(_, r)| r.exch.clone())
        .filter(|e| !e.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Where the watchers read the balances snapshot.
pub fn balances_path(cfg: &RunConfig) -> PathBuf {
    cfg.state_dir.join("balances.json")
}

/// Read every routed venue's balances; read-only on each.
pub fn read<'a>(cfg: &RunConfig, client: impl Fn(&str) -> Result<&'a dyn Venue, String>) -> Read {
    venues(cfg)
        .into_iter()
        .map(|name| {
            let got = client(&name).and_then(|v| v.balances_full().map_err(|e| e.to_string()));
            (name, got)
        })
        .collect()
}

/// The watchers' snapshot.
pub fn snapshot_json(ts: f64, read: &Read) -> Value {
    let mut venues = Map::new();
    for (name, got) in read {
        let body = match got {
            Ok(bals) => Value::Object(
                bals.iter()
                    .map(|(asset, b)| (asset.clone(), json!({"free": b.free, "locked": b.locked})))
                    .collect(),
            ),
            Err(e) => json!({ "error": e }),
        };
        venues.insert(name.clone(), body);
    }
    json!({ "ts": ts, "venues": venues })
}

/// The monthly backtest's start book, `None` when a venue could not be read.
pub fn book_json(cfg: &RunConfig, read: &Read) -> Option<Value> {
    let mut ok: Vec<(&str, &BTreeMap<String, Balance>)> = Vec::new();
    for (name, got) in read {
        ok.push((name.as_str(), got.as_ref().ok()?));
    }
    let held: Map<String, Value> = cfg
        .watchlist
        .iter()
        .map(|(sym, _)| {
            let total: f64 = ok
                .iter()
                .filter_map(|(_, b)| b.get(sym))
                .map(|b| b.free + b.locked)
                .sum();
            (sym.clone(), json!(total))
        })
        .collect();
    let stable: Map<String, Value> = ok
        .iter()
        .map(|(name, b)| {
            let free: f64 = STABLES
                .iter()
                .filter_map(|a| b.get(*a))
                .map(|x| x.free)
                .sum();
            (name.to_string(), json!(free))
        })
        .collect();
    let counts: Map<String, Value> = ok
        .iter()
        .map(|(name, _)| {
            let n = cfg.routing.iter().filter(|(_, r)| r.exch == *name).count();
            (name.to_string(), json!(n))
        })
        .collect();
    Some(json!({ "held": held, "stable": stable, "counts": counts }))
}

/// Write a JSON file atomically, readable by its owner only (it lists every balance).
pub(crate) fn write_private(path: &Path, v: &Value) -> Result<(), String> {
    let body = serde_json::to_string_pretty(v).map_err(|e| e.to_string())? + "\n";
    crate::store::write_atomic(path, &body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot restrict {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Write the snapshot, then the book when asked for and every venue was read. The
/// lines say what was written; `Err` names the venues that failed (after the snapshot,
/// which records their errors, has been written).
pub fn write(
    cfg: &RunConfig,
    read: &Read,
    book: Option<&Path>,
    ts: f64,
) -> Result<Vec<String>, String> {
    let path = balances_path(cfg);
    write_private(&path, &snapshot_json(ts, read))?;
    let mut lines = vec![format!(
        "wrote {} | {}",
        path.display(),
        read.iter()
            .map(|(n, got)| match got {
                Ok(b) => format!("{n} {} assets", b.len()),
                Err(_) => format!("{n} failed"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    )];
    let failed: Vec<String> = read
        .iter()
        .filter_map(|(n, got)| got.as_ref().err().map(|e| format!("{n} ({e})")))
        .collect();
    if !failed.is_empty() {
        return Err(format!("balance reads failed for: {}", failed.join(", ")));
    }
    if let Some(p) = book {
        if let Some(b) = book_json(cfg, read) {
            write_private(p, &b)?;
            lines.push(format!(
                "wrote {} | {} coins",
                p.display(),
                cfg.watchlist.len()
            ));
        }
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::config::CoinRoute;

    fn cfg(dir: &Path) -> RunConfig {
        let route = |exch: &str, pair: &str, quote: &str| CoinRoute {
            exch: exch.into(),
            pair: pair.into(),
            quote: quote.into(),
        };
        RunConfig {
            watchlist: vec![
                ("AAA".into(), "aaa".into()),
                ("BBB".into(), "bbb".into()),
                ("CCC".into(), "ccc".into()),
            ],
            routing: vec![
                ("AAA".into(), route("revx", "AAA/USD", "USD")),
                ("BBB".into(), route("gate", "BBB_USDT", "USDT")),
                ("CCC".into(), route("revx", "CCC/USD", "USD")),
            ],
            state_dir: dir.to_path_buf(),
            ..Default::default()
        }
    }

    fn bal(pairs: &[(&str, f64, f64)]) -> BTreeMap<String, Balance> {
        pairs
            .iter()
            .map(|(a, free, locked)| {
                (
                    a.to_string(),
                    Balance {
                        free: *free,
                        locked: *locked,
                    },
                )
            })
            .collect()
    }

    fn healthy() -> Read {
        vec![
            (
                "gate".into(),
                Ok(bal(&[
                    ("BBB", 100.0, 25.0),
                    ("AAA", 4.0, 1.0),
                    ("USDT", 10.5, 40.0),
                ])),
            ),
            (
                "revx".into(),
                Ok(bal(&[
                    ("AAA", 50.0, 0.0),
                    ("USD", 300.0, 200.0),
                    ("USDC", 2.0, 0.0),
                ])),
            ),
        ]
    }

    #[test]
    fn the_routed_venues_in_name_order() {
        let dir = std::env::temp_dir();
        assert_eq!(venues(&cfg(&dir)), vec!["gate", "revx"]);
    }

    #[test]
    fn the_book_holds_the_whole_bag_and_the_free_cash() {
        let dir = std::env::temp_dir();
        let b = book_json(&cfg(&dir), &healthy()).unwrap();
        // held: free plus locked on every venue; a coin no venue holds reads 0
        assert_eq!(b["held"]["AAA"], json!(55.0));
        assert_eq!(b["held"]["BBB"], json!(125.0));
        assert_eq!(b["held"]["CCC"], json!(0.0));
        // stable: free USD, USDC and USDT only
        assert_eq!(b["stable"]["gate"], json!(10.5));
        assert_eq!(b["stable"]["revx"], json!(302.0));
        assert_eq!(b["counts"]["gate"], json!(1));
        assert_eq!(b["counts"]["revx"], json!(2));
    }

    #[test]
    fn a_failed_venue_is_recorded_and_no_book_is_written() {
        let dir = std::env::temp_dir().join(format!("rungbot-balances-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let c = cfg(&dir);
        let mut r = healthy();
        r[1].1 = Err("revx: 401 unauthorized".into());
        let book = dir.join("book.json");
        let e = write(&c, &r, Some(&book), 1_800_000_000.0).unwrap_err();
        assert_eq!(e, "balance reads failed for: revx (revx: 401 unauthorized)");
        assert!(!book.exists());
        let snap: Value =
            serde_json::from_str(&std::fs::read_to_string(balances_path(&c)).unwrap()).unwrap();
        assert_eq!(
            snap["venues"]["revx"]["error"],
            json!("revx: 401 unauthorized")
        );
        assert_eq!(snap["venues"]["gate"]["USDT"]["locked"], json!(40.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_healthy_read_writes_both_files_for_the_owner_only() {
        let dir = std::env::temp_dir().join(format!("rungbot-balances-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let c = cfg(&dir);
        let book = dir.join("book.json");
        let lines = write(&c, &healthy(), Some(&book), 1_800_000_000.0).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("| gate 3 assets, revx 3 assets"));
        let snap: Value =
            serde_json::from_str(&std::fs::read_to_string(balances_path(&c)).unwrap()).unwrap();
        assert_eq!(snap["ts"], json!(1_800_000_000.0));
        assert_eq!(
            snap["venues"]["revx"]["USD"],
            json!({"free": 300.0, "locked": 200.0})
        );
        let b: Value = serde_json::from_str(&std::fs::read_to_string(&book).unwrap()).unwrap();
        assert_eq!(b["held"]["BBB"], json!(125.0));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&book).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
