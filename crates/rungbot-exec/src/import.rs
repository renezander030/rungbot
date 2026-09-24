//! Importing an order journal written by another implementation of this journal format.
//!
//! The source directory holds `orders-journal.json` (`{client_id: row}`) and, optionally,
//! `orders-archive.jsonl` and the live run state: `ladder-state.live.json`,
//! `pnl-ledger.json` and `ttl-warned.json`. The run state is written beside the imported
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
        report: r,
    })
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
