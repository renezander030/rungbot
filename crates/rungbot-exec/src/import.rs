//! Importing an order journal written by another implementation of this journal format.
//!
//! The source directory holds `orders-journal.json` (`{client_id: row}`) and, optionally,
//! `orders-archive.jsonl` and the live run state: `ladder-state.live.json`,
//! `pnl-ledger.json`, `ttl-warned.json`, the decision log `decisions.jsonl`, the
//! signal-mail dedupe `signal-notices.json`, the level alert's `btc-alert-state.json`, the
//! deploy layer's `deploy-state.live.json` (written as `deploy-state.json`) and the last
//! book audit `audit-state.json`. The run state is written beside the imported
//! journal under the names [`crate::store`] uses. Every row is read into an [`Order`]; fields this crate does not
//! model are carried along unchanged. The import then checks itself: each row is written
//! back out and compared with the source, where `null` and an absent field count as the
//! same and `5` and `5.0` are the same number. Anything else that differs is reported.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::Value;

use crate::housekeeping::{LadderState, PnlLedger, TtlWarned};
use crate::journal::{Journal, Order};
use crate::store;

pub const JOURNAL_FILE: &str = "orders-journal.json";
pub const ARCHIVE_FILE: &str = "orders-archive.jsonl";
pub const LADDER_SOURCE: &str = "ladder-state.live.json";
pub const PNL_SOURCE: &str = "pnl-ledger.json";
pub const TTL_SOURCE: &str = "ttl-warned.json";
pub const DECISIONS_FILE: &str = "decisions.jsonl";
pub const NOTICES_FILE: &str = "signal-notices.json";
pub const BTC_ALERT_FILE: &str = "btc-alert-state.json";
/// The deploy layer's state in the source directory, and its name beside the journal.
pub const DEPLOY_SOURCE: &str = "deploy-state.live.json";
pub const DEPLOY_FILE: &str = "deploy-state.json";
/// The last book audit (the dashboard banner reads it).
pub const AUDIT_FILE: &str = "audit-state.json";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportReport {
    pub rows: usize,
    pub open: usize,
    pub by_status: BTreeMap<String, usize>,
    pub by_kind: BTreeMap<String, usize>,
    pub by_venue: BTreeMap<String, usize>,
    /// Fields kept verbatim because no [`Order`] field models them, with their counts.
    pub unmodelled: BTreeMap<String, usize>,
    pub archive_rows: usize,
    /// Coins in the ladder state, and whether it carries a balance snapshot; `None`
    /// without a ladder state file.
    pub ladder_coins: Option<(usize, bool)>,
    pub pnl_records: Option<usize>,
    pub ttl_flags: Option<usize>,
    pub decision_lines: Option<usize>,
    pub notices: Option<usize>,
    pub btc_band: Option<String>,
    /// Venues with a deploy baseline, and whether a top-up is marked in flight.
    pub deploy_venues: Option<(usize, bool)>,
    /// Whether the last audit was clean, and its finding count.
    pub audit: Option<(bool, usize)>,
    /// Rows whose re-serialised form differs from the source beyond null/absent and
    /// int/float, with the first difference.
    pub mismatches: Vec<String>,
}

impl ImportReport {
    pub fn lines(&self) -> Vec<String> {
        let fmt = |m: &BTreeMap<String, usize>| {
            m.iter()
                .map(|(k, v)| format!("{} {v}", if k.is_empty() { "(none)" } else { k }))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut out = vec![
            format!("rows: {} ({} open)", self.rows, self.open),
            format!("status: {}", fmt(&self.by_status)),
            format!("kind: {}", fmt(&self.by_kind)),
            format!("venue: {}", fmt(&self.by_venue)),
            format!("archive rows: {}", self.archive_rows),
        ];
        if let Some((n, snap)) = self.ladder_coins {
            out.push(format!(
                "ladder state: {n} coin(s), {}",
                if snap {
                    "with a balance snapshot"
                } else {
                    "no balance snapshot (the first run writes one)"
                }
            ));
        }
        if let Some(n) = self.pnl_records {
            out.push(format!("pnl ledger: {n} record(s)"));
        }
        if let Some(n) = self.ttl_flags {
            out.push(format!("stale-order flags: {n}"));
        }
        if let Some(n) = self.decision_lines {
            out.push(format!("decision log: {n} line(s)"));
        }
        if let Some(n) = self.notices {
            out.push(format!("signal notices: {n}"));
        }
        if let Some(b) = &self.btc_band {
            out.push(format!("BTC level state: {b}"));
        }
        if let Some((n, inflight)) = self.deploy_venues {
            out.push(format!(
                "deploy state: {n} venue baseline(s){}",
                if inflight { ", a top-up in flight" } else { "" }
            ));
        }
        if let Some((ok, n)) = self.audit {
            out.push(format!(
                "book audit: {}",
                if ok {
                    "clean".to_string()
                } else {
                    format!("{n} finding(s)")
                }
            ));
        }
        if !self.unmodelled.is_empty() {
            out.push(format!("kept verbatim: {}", fmt(&self.unmodelled)));
        }
        if self.mismatches.is_empty() {
            out.push("round trip: every row reads back as written".into());
        } else {
            out.push(format!(
                "round trip: {} row(s) differ",
                self.mismatches.len()
            ));
            out.extend(self.mismatches.iter().map(|m| format!("  {m}")));
        }
        out
    }
}

/// What an import read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Imported {
    pub journal: Journal,
    pub archive: Vec<Order>,
    pub ladder: Option<LadderState>,
    pub pnl: Option<PnlLedger>,
    pub ttl: Option<TtlWarned>,
    /// The decision log, verbatim (every line checked to be JSON).
    pub decisions: Option<String>,
    pub notices: Option<rungbot_notify::signal_notices::NoticeState>,
    pub btc_alert: Option<Value>,
    pub deploy: Option<serde_json::Map<String, Value>>,
    pub audit: Option<Value>,
    pub report: ImportReport,
}

/// Read `dir`'s journal and archive. A missing journal is an error here: importing
/// nothing is never what was meant.
pub fn read_dir(dir: &Path) -> Result<Imported, String> {
    let jpath = dir.join(JOURNAL_FILE);
    if !jpath.exists() {
        return Err(format!("no {JOURNAL_FILE} in {}", dir.display()));
    }
    let ordered = store::read_state_ordered(&jpath)?;
    let source: serde_json::Map<String, Value> = ordered.clone().into_iter().collect();
    let journal =
        store::journal_from_rows(ordered).map_err(|e| format!("{}: {e}", jpath.display()))?;
    let archive = store::read_archive(&dir.join(ARCHIVE_FILE))?;
    let (ladder, pnl, ttl) = read_run_state(dir)?;
    let (decisions, notices, btc_alert) = read_run_logs(dir)?;
    let deploy = if dir.join(DEPLOY_SOURCE).exists() {
        Some(store::read_state(&dir.join(DEPLOY_SOURCE))?)
    } else {
        None
    };
    let audit = if dir.join(AUDIT_FILE).exists() {
        Some(Value::Object(store::read_state(&dir.join(AUDIT_FILE))?))
    } else {
        None
    };

    let mut r = ImportReport {
        rows: journal.orders.len(),
        open: journal.open_orders(None).len(),
        archive_rows: archive.len(),
        ladder_coins: ladder.as_ref().map(|l| {
            (
                l.keys().filter(|k| !k.starts_with('_')).count(),
                l.get("_bal").is_some_and(|b| b.get("booked").is_some()),
            )
        }),
        pnl_records: pnl.as_ref().map(Vec::len),
        ttl_flags: ttl.as_ref().map(IndexMap::len),
        decision_lines: decisions.as_ref().map(|d| d.lines().count()),
        notices: notices.as_ref().map(|n| n.len()),
        btc_band: btc_alert.as_ref().map(|b| {
            b.get("band")
                .and_then(Value::as_str)
                .unwrap_or("ok")
                .to_string()
        }),
        deploy_venues: deploy.as_ref().map(|d| {
            (
                d.get("stable")
                    .and_then(Value::as_object)
                    .map_or(0, |m| m.len()),
                d.get("inflight")
                    .is_some_and(|v| crate::pyfmt::truthy(Some(v))),
            )
        }),
        audit: audit.as_ref().map(|a| {
            (
                a.get("ok").and_then(Value::as_bool).unwrap_or(false),
                a.get("findings")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len),
            )
        }),
        ..Default::default()
    };
    for o in journal.orders.values() {
        *r.by_status.entry(o.status.clone()).or_default() += 1;
        *r.by_kind.entry(o.kind.clone()).or_default() += 1;
        *r.by_venue.entry(o.exch.clone()).or_default() += 1;
        for k in o.extra.keys() {
            *r.unmodelled.entry(k.clone()).or_default() += 1;
        }
    }
    for (cid, src) in &source {
        let ours = journal
            .get(cid)
            .map(|o| serde_json::to_value(o).expect("an order serialises"))
            .unwrap_or(Value::Null);
        if let Some(d) = first_difference(src, &ours, cid) {
            r.mismatches.push(d);
        }
    }
    Ok(Imported {
        journal,
        archive,
        ladder,
        pnl,
        ttl,
        decisions,
        notices,
        btc_alert,
        deploy,
        audit,
        report: r,
    })
}

type RunLogs = (
    Option<String>,
    Option<rungbot_notify::signal_notices::NoticeState>,
    Option<Value>,
);

/// The decision log, the notice dedupe and the level-alert state, when present. Each
/// must read cleanly.
fn read_run_logs(dir: &Path) -> Result<RunLogs, String> {
    let read = |name: &str| -> Result<Option<(PathBuf, String)>, String> {
        let p = dir.join(name);
        if !p.exists() {
            return Ok(None);
        }
        let t = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(Some((p, t)))
    };
    let decisions = match read(DECISIONS_FILE)? {
        Some((p, t)) => {
            for (i, line) in t.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
                serde_json::from_str::<Value>(line)
                    .map_err(|e| format!("{} line {}: {e}", p.display(), i + 1))?;
            }
            Some(t)
        }
        None => None,
    };
    let notices = match read(NOTICES_FILE)? {
        Some((p, t)) => Some(
            serde_json::from_str::<rungbot_notify::signal_notices::NoticeState>(&t)
                .map_err(|e| format!("{}: {e}", p.display()))?,
        ),
        None => None,
    };
    let btc_alert = match read(BTC_ALERT_FILE)? {
        Some((p, t)) => match serde_json::from_str::<Value>(&t) {
            Ok(v @ Value::Object(_)) => Some(v),
            Ok(_) => return Err(format!("{}: expected an object", p.display())),
            Err(e) => return Err(format!("{}: {e}", p.display())),
        },
        None => None,
    };
    Ok((decisions, notices, btc_alert))
}

type RunState = (Option<LadderState>, Option<PnlLedger>, Option<TtlWarned>);

/// The run-state files that exist in `dir`. Each must read cleanly: an import that
/// silently drops a ladder state or a ledger would start the new runtime from nothing.
fn read_run_state(dir: &Path) -> Result<RunState, String> {
    let exists = |name: &str| dir.join(name).exists();
    let ladder = if exists(LADDER_SOURCE) {
        Some(store::load_ladder(&dir.join(LADDER_SOURCE))?)
    } else {
        None
    };
    let pnl = if exists(PNL_SOURCE) {
        let p = dir.join(PNL_SOURCE);
        let raw = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Array(v)) => Some(v),
            Ok(_) => return Err(format!("{}: expected a list of records", p.display())),
            Err(e) => return Err(format!("{}: {e}", p.display())),
        }
    } else {
        None
    };
    let ttl = if exists(TTL_SOURCE) {
        let p = dir.join(TTL_SOURCE);
        let raw = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let m: IndexMap<String, Value> =
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", p.display()))?;
        let mut t = TtlWarned::new();
        for (k, v) in m {
            let f = v
                .as_f64()
                .ok_or_else(|| format!("{}: {k} is not a timestamp", p.display()))?;
            t.insert(k, f);
        }
        Some(t)
    } else {
        None
    };
    Ok((ladder, pnl, ttl))
}

/// Write an import to `journal_path` and its archive. Refuses to replace a journal or
/// archive that already holds rows unless `force`.
pub fn write(imported: &Imported, journal_path: &Path, force: bool) -> Result<PathBuf, String> {
    let existing = store::load_journal(journal_path)?;
    if !existing.orders.is_empty() && !force {
        return Err(format!(
            "{} already holds {} row(s); pass --force to replace it",
            journal_path.display(),
            existing.orders.len()
        ));
    }
    let apath = store::archive_path(journal_path);
    let has_archive = std::fs::metadata(&apath).is_ok_and(|m| m.len() > 0);
    if has_archive && !force {
        return Err(format!(
            "{} already exists; pass --force to replace it",
            apath.display()
        ));
    }
    let mut text = String::new();
    for o in &imported.archive {
        text.push_str(&store::archive_line(o));
        text.push('\n');
    }
    let targets = [
        (store::LADDER_FILE, imported.ladder.is_some()),
        (store::PNL_FILE, imported.pnl.is_some()),
        (store::TTL_FILE, imported.ttl.is_some()),
        (DECISIONS_FILE, imported.decisions.is_some()),
        (NOTICES_FILE, imported.notices.is_some()),
        (BTC_ALERT_FILE, imported.btc_alert.is_some()),
        (DEPLOY_FILE, imported.deploy.is_some()),
        (AUDIT_FILE, imported.audit.is_some()),
    ];
    for (name, present) in targets {
        let p = store::sibling(journal_path, name);
        if present && !force && std::fs::metadata(&p).is_ok_and(|m| m.len() > 0) {
            return Err(format!(
                "{} already exists; pass --force to replace it",
                p.display()
            ));
        }
    }
    if !imported.archive.is_empty() || has_archive {
        store::write_atomic(&apath, &text)?;
    }
    if let Some(l) = &imported.ladder {
        store::save_ladder(&store::sibling(journal_path, store::LADDER_FILE), l)?;
    }
    if let Some(p) = &imported.pnl {
        store::save_pnl(&store::sibling(journal_path, store::PNL_FILE), p)?;
    }
    if let Some(t) = &imported.ttl {
        store::save_ttl_warned(&store::sibling(journal_path, store::TTL_FILE), t)?;
    }
    if let Some(d) = &imported.decisions {
        store::write_atomic(&store::sibling(journal_path, DECISIONS_FILE), d)?;
    }
    if let Some(n) = &imported.notices {
        store::write_atomic(
            &store::sibling(journal_path, NOTICES_FILE),
            &rungbot_notify::signal_notices::to_json(n),
        )?;
    }
    if let Some(b) = &imported.btc_alert {
        store::write_atomic(
            &store::sibling(journal_path, BTC_ALERT_FILE),
            &crate::pyfmt::dumps(b, None),
        )?;
    }
    if let Some(d) = &imported.deploy {
        crate::deploy::save_state(&store::sibling(journal_path, DEPLOY_FILE), d)?;
    }
    if let Some(a) = &imported.audit {
        store::write_atomic(
            &store::sibling(journal_path, AUDIT_FILE),
            &crate::pyfmt::dumps(a, Some(1)),
        )?;
    }
    store::save_journal(journal_path, &imported.journal)?;
    Ok(apath)
}

/// The first way `ours` differs from `src`, ignoring null-vs-absent and int-vs-float.
pub fn first_difference(src: &Value, ours: &Value, at: &str) -> Option<String> {
    match (src, ours) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a
                .iter()
                .chain(b.iter())
                .filter(|(_, v)| !v.is_null())
                .map(|(k, _)| k)
                .collect();
            for k in keys {
                let (x, y) = (
                    a.get(k).unwrap_or(&Value::Null),
                    b.get(k).unwrap_or(&Value::Null),
                );
                if let Some(d) = first_difference(x, y, &format!("{at}.{k}")) {
                    return Some(d);
                }
            }
            None
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(format!("{at}: {} items, read back {}", a.len(), b.len()));
            }
            a.iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (x, y))| first_difference(x, y, &format!("{at}[{i}]")))
        }
        (Value::Number(a), Value::Number(b)) if a.as_f64() == b.as_f64() => None,
        (a, b) if a == b => None,
        (a, b) => Some(format!("{at}: {a} read back as {b}")),
    }
}
