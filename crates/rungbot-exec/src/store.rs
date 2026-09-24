//! Where the journal lives on disk, and who may touch it.
//!
//! Format: one JSON object `{client_id: row}`, two-space indented, written to a `.tmp`
//! sibling and renamed over the old file, so a crash leaves either the old journal or
//! the new one and never half of each. Finished rows move to an append-only JSONL
//! archive next to it (`orders-archive.jsonl`, one sorted-key row per line).
//!
//! Journals written by rungbot-exec 0.5, `{"orders": {...}}` with `venue`,
//! `venue_order_id` and `placed_ts`, are read too and written back in this format.
//!
//! The run state beside it follows the same write rule: the ladder state
//! (`ladder-state.json`), the realized-P&L ledger (`pnl-ledger.json`) and the stale-order
//! flags (`ttl-warned.json`).
//!
//! Two rules, both about not losing the one record that stops a double trade:
//!
//! * A state file that exists but does not parse is an **error**, never an empty state.
//!   Read as empty, every order in it is forgotten: the next save overwrites the file,
//!   and an intent that was already placed no longer looks placed.
//! * One writer at a time. [`RunLock`] makes a second writer wait, then give up naming
//!   the holder. It is an OS file lock, so a crashed process never leaves it behind.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::housekeeping::{LadderState, PnlLedger, TtlWarned};
use crate::journal::{Journal, Order};
use crate::pyfmt;

/// `path` with its extension replaced by `tmp`: `orders-journal.json` → `orders-journal.tmp`.
pub fn tmp_path(path: &Path) -> PathBuf {
    path.with_extension("tmp")
}

/// Read a machine-written state file. A missing file is a fresh start (an empty
/// object); one that exists but cannot be read, does not parse, or is not an object is
/// an error that names the file and how to recover.
pub fn read_state(path: &Path) -> Result<Map<String, Value>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(unreadable(path, &e.to_string())),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(other) => Err(format!(
            "{} holds {}, expected an object",
            path.display(),
            match other {
                Value::Array(_) => "list",
                Value::String(_) => "str",
                Value::Number(_) => "number",
                Value::Bool(_) => "bool",
                _ => "NoneType",
            }
        )),
        Err(e) => Err(unreadable(path, &e.to_string())),
    }
}

fn unreadable(path: &Path, e: &str) -> String {
    format!(
        "{} exists but cannot be read ({e}); refusing to run on an empty state -- restore \
         it from {} or a backup, or move it aside to start fresh",
        path.display(),
        tmp_path(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    )
}

/// [`read_state`], keeping the file's key order. The journal's row order is behaviour:
/// reconcile hands fills out in it, and the order fills are booked in decides which cost
/// basis a sell is measured against.
pub fn read_state_ordered(path: &Path) -> Result<IndexMap<String, Value>, String> {
    if read_state(path)?.is_empty() {
        return Ok(IndexMap::new());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| unreadable(path, &e.to_string()))?;
    serde_json::from_str(&raw).map_err(|e| unreadable(path, &e.to_string()))
}

/// Is this object a 0.5-era journal, `{"orders": {cid: row}}`?
fn is_legacy(m: &IndexMap<String, Value>) -> bool {
    m.len() == 1
        && m.get("orders").is_some_and(|o| {
            o.as_object()
                .is_some_and(|rows| rows.values().all(|r| r.get("placed_ts").is_some()))
                && o.get("client_id").is_none()
        })
}

/// Turn a parsed journal object into a [`Journal`], migrating the 0.5 layout. A
/// `serde_json` map is sorted by id; use [`journal_from_rows`] to keep the file's order.
pub fn journal_from_map(m: Map<String, Value>) -> Result<Journal, String> {
    journal_from_rows(m.into_iter().collect())
}

/// Turn journal rows, in file order, into a [`Journal`], migrating the 0.5 layout.
pub fn journal_from_rows(m: IndexMap<String, Value>) -> Result<Journal, String> {
    let (rows, legacy): (Vec<(String, Value)>, bool) = if is_legacy(&m) {
        match m.into_iter().next() {
            Some((_, Value::Object(rows))) => (rows.into_iter().collect(), true),
            _ => unreachable!("is_legacy checked the shape"),
        }
    } else {
        (m.into_iter().collect(), false)
    };
    let mut j = Journal::default();
    for (cid, row) in rows {
        let mut o: Order = serde_json::from_value(row)
            .map_err(|e| format!("journal row {cid:?} does not read: {e}"))?;
        if o.client_id.is_empty() {
            o.client_id = cid.clone();
        }
        if legacy {
            migrate_legacy(&mut o);
        }
        j.orders.insert(cid, o);
    }
    Ok(j)
}

/// 0.5 spelled a venue cancel `venue_cancelled` and our own cancel `cancelled` with the
/// note `manual cancel`. The note is what tells the two apart now.
fn migrate_legacy(o: &mut Order) {
    match o.status.as_str() {
        "venue_cancelled" => o.status = "cancelled".into(),
        "cancelled" => {
            o.status = "canceled".into();
            let note = o.note.take().unwrap_or_default();
            o.note = Some(if note.is_empty() || note == "manual cancel" {
                "manual --cancel".into()
            } else {
                format!("{note} (manual --cancel)")
            });
        }
        _ => {}
    }
    if o.swept == Some(false) {
        o.swept = None;
    }
}

/// Load the journal: see [`read_state`] for the failure rules.
pub fn load_journal(path: &Path) -> Result<Journal, String> {
    let m = read_state_ordered(path)?;
    journal_from_rows(m).map_err(|e| format!("{}: {e}", path.display()))
}

/// The journal file's exact bytes.
pub fn journal_text(j: &Journal) -> String {
    pyfmt::dumps(j, Some(2))
}

/// Write any text atomically: a temp sibling, then a rename over the target.
pub fn write_atomic(path: &Path, body: &str) -> Result<(), String> {
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(p).map_err(|e| format!("cannot create {}: {e}", p.display()))?;
    }
    let tmp = tmp_path(path);
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

pub fn save_journal(path: &Path, j: &Journal) -> Result<(), String> {
    write_atomic(path, &journal_text(j))
}

/// The archive that sits next to a journal. `RUNGBOT_ORDER_ARCHIVE` overrides it.
pub fn archive_path(journal: &Path) -> PathBuf {
    match std::env::var("RUNGBOT_ORDER_ARCHIVE") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => journal.with_file_name("orders-archive.jsonl"),
    }
}

/// One archive line: the row with sorted keys, Python's default separators.
pub fn archive_line(o: &Order) -> String {
    // A Value's map is sorted, which is the order the archive wants.
    let v = serde_json::to_value(o).expect("an order always serialises");
    pyfmt::dumps(&v, None)
}

/// Append rows to the archive, one line each.
pub fn append_archive(path: &Path, rows: &[Order]) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(p).map_err(|e| format!("cannot create {}: {e}", p.display()))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut text = String::new();
    for o in rows {
        text.push_str(&archive_line(o));
        text.push('\n');
    }
    f.write_all(text.as_bytes())
        .map_err(|e| format!("cannot append to {}: {e}", path.display()))
}

/// Read an archive back. Blank lines are skipped; a line that does not parse is an
/// error naming its number.
pub fn read_archive(path: &Path) -> Result<Vec<Order>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    raw.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(n, l)| {
            serde_json::from_str(l).map_err(|e| format!("{} line {}: {e}", path.display(), n + 1))
        })
        .collect()
}

// ------------------------------------------------------------------ run state files

/// The ladder state, beside the journal.
pub const LADDER_FILE: &str = "ladder-state.json";
/// The realized-P&L ledger, beside the journal.
pub const PNL_FILE: &str = "pnl-ledger.json";
/// Which stale orders were flagged when, beside the journal.
pub const TTL_FILE: &str = "ttl-warned.json";

/// A run-state file that sits next to the journal.
pub fn sibling(journal: &Path, name: &str) -> PathBuf {
    journal.with_file_name(name)
}

/// The ladder state. The same rules as the journal: missing is a fresh start, unreadable
/// is an error, because an empty ladder state forgets every rung already taken.
pub fn load_ladder(path: &Path) -> Result<LadderState, String> {
    read_state(path)
}

pub fn save_ladder(path: &Path, state: &LadderState) -> Result<(), String> {
    write_atomic(path, &pyfmt::dumps(state, Some(2)))
}

/// The stale-order flags. They only suppress a repeated warning, so a file that is
/// missing or unreadable starts empty; entries that are not a number are dropped.
pub fn load_ttl_warned(path: &Path) -> TtlWarned {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<IndexMap<String, Value>>(&raw).ok())
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k, f)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn save_ttl_warned(path: &Path, ttl: &TtlWarned) -> Result<(), String> {
    write_atomic(path, &pyfmt::dumps(ttl, Some(2)))
}

/// The P&L ledger. Missing, or not a list, starts empty; a file that does not parse is an
/// error, so an append never overwrites records it could not read.
pub fn load_pnl(path: &Path) -> Result<PnlLedger, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Array(v)) => Ok(v),
        Ok(_) => Ok(Vec::new()),
        Err(e) => Err(unreadable(path, &e.to_string())),
    }
}

pub fn save_pnl(path: &Path, ledger: &PnlLedger) -> Result<(), String> {
    write_atomic(path, &pyfmt::dumps(ledger, Some(2)))
}

/// The lock file for a journal: `RUNGBOT_LOCK` if set, else `<journal>.lock`.
pub fn lock_path(journal: &Path) -> PathBuf {
    match std::env::var("RUNGBOT_LOCK") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => journal.with_extension("lock"),
    }
}

/// An exclusive hold on the journal and the venues. Released when dropped, or by the
/// OS if the process dies.
#[derive(Debug)]
pub struct RunLock {
    file: File,
}

impl RunLock {
    /// Take the lock for `who`, waiting up to `wait` for another holder to finish.
    pub fn acquire(path: &Path, who: &str, wait: Duration) -> Result<RunLock, String> {
        if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(p)
                .map_err(|e| format!("cannot create {}: {e}", p.display()))?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("cannot open the lock {}: {e}", path.display()))?;
        let deadline = Instant::now() + wait;
        loop {
            // Fully qualified: newer toolchains also have an inherent File::try_lock.
            match fs4::FileExt::try_lock(&file) {
                Ok(()) => break,
                Err(fs4::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(fs4::TryLockError::WouldBlock) => {
                    let mut holder = String::new();
                    let _ = file.seek(SeekFrom::Start(0));
                    let _ = file.read_to_string(&mut holder);
                    let holder = holder.trim();
                    return Err(format!(
                        "{who}: another run holds {} ({}); waited {}s",
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string()),
                        if holder.is_empty() {
                            "unknown holder"
                        } else {
                            holder
                        },
                        wait.as_secs()
                    ));
                }
                Err(fs4::TryLockError::Error(e)) => {
                    return Err(format!("cannot lock {}: {e}", path.display()))
                }
            }
        }
        // Best effort: who holds it, for the next process's refusal message.
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as f64)
            .unwrap_or(0.0);
        let iso = rungbot_core::time::iso8601(since);
        let stamp = format!("{} {}Z", &iso[..10], &iso[11..19]);
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "{who}, pid {}, since {stamp}", std::process::id());
        let _ = file.flush();
        Ok(RunLock { file })
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that is removed when the test ends.
    struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir(tag: &str) -> Scratch {
        let d = std::env::temp_dir().join(format!("rungbot-store-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Scratch(d)
    }

    fn order(cid: &str) -> Order {
        Order {
            client_id: cid.into(),
            sym: "AAA".into(),
            exch: "gate".into(),
            pair: "AAA_USDT".into(),
            side: "buy".into(),
            kind: "ladder_buy".into(),
            status: "open".into(),
            price: Some(1.0),
            base: Some(10.0),
            quote: Some(10.0),
            ts: Some(1.0),
            order_id: Some("1".into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_missing_journal_is_a_fresh_start() {
        let d = dir("missing");
        assert!(load_journal(&d.join("orders-journal.json"))
            .unwrap()
            .orders
            .is_empty());
    }

    #[test]
    fn a_journal_round_trips_and_is_a_flat_object() {
        let d = dir("roundtrip");
        let p = d.join("orders-journal.json");
        let mut j = Journal::default();
        j.record(order("csAAAb1r1"));
        save_journal(&p, &j).unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["csAAAb1r1"]["exch"], "gate", "keyed by client id");
        assert_eq!(load_journal(&p).unwrap(), j);
    }

    #[test]
    fn a_corrupt_journal_is_an_error_and_is_left_as_it_was() {
        let d = dir("corrupt");
        let p = d.join("orders-journal.json");
        let mut j = Journal::default();
        j.record(order("csAAAb1r1"));
        save_journal(&p, &j).unwrap();
        let good = std::fs::read_to_string(&p).unwrap();
        let cut = &good[..good.len() / 2];
        std::fs::write(&p, cut).unwrap();
        let err = load_journal(&p).unwrap_err();
        assert!(err.contains("cannot be read"), "{err}");
        assert!(err.contains("orders-journal.tmp"), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), cut);
        std::fs::write(&p, "[1, 2]").unwrap();
        assert!(load_journal(&p).unwrap_err().contains("holds list"));
    }

    #[test]
    fn a_journal_reads_back_in_file_order() {
        let d = dir("order");
        let p = d.join("orders-journal.json");
        std::fs::write(
            &p,
            r#"{"zz": {"client_id": "zz"}, "aa": {"client_id": "aa"}, "mm": {"client_id": "mm"}}"#,
        )
        .unwrap();
        let j = load_journal(&p).unwrap();
        let ids: Vec<&str> = j.orders.keys().map(String::as_str).collect();
        assert_eq!(ids, ["zz", "aa", "mm"]);
    }

    #[test]
    fn a_journal_from_the_previous_release_is_migrated() {
        let d = dir("legacy");
        let p = d.join("orders-journal.json");
        std::fs::write(
            &p,
            r#"{"orders": {
                "a": {"client_id": "a", "sym": "AAA", "pair": "AAA_USDT", "venue": "gate",
                      "side": "buy", "kind": "ladder_buy", "price": 1.0, "base": 2.0,
                      "quote": 2.0, "status": "venue_cancelled", "placed_ts": 5.0,
                      "venue_order_id": "9", "status_ts": 6.0, "swept": false},
                "b": {"client_id": "b", "sym": "AAA", "pair": "AAA_USDT", "venue": "gate",
                      "side": "buy", "kind": "ladder_buy", "price": 1.0, "base": 2.0,
                      "quote": 2.0, "status": "cancelled", "placed_ts": 5.0,
                      "note": "manual cancel", "swept": false}}}"#,
        )
        .unwrap();
        let j = load_journal(&p).unwrap();
        let a = j.get("a").unwrap();
        assert_eq!(
            (a.exch.as_str(), a.order_id.as_deref()),
            ("gate", Some("9"))
        );
        assert_eq!((a.status.as_str(), a.ts), ("cancelled", Some(5.0)));
        assert_eq!(j.venue_cancelled_unswept(None).len(), 1, "a is the venue's");
        let b = j.get("b").unwrap();
        assert_eq!(b.note.as_deref(), Some("manual --cancel"), "b is ours");
    }

    #[test]
    fn archive_lines_are_sorted_and_read_back() {
        let d = dir("archive");
        let p = d.join("orders-archive.jsonl");
        append_archive(&p, &[order("x"), order("y")]).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with(r#"{"base": 10.0, "client_id": "x", "exch": "gate""#),
            "{text}"
        );
        let back = read_archive(&p).unwrap();
        assert_eq!(back, vec![order("x"), order("y")]);
    }

    #[test]
    fn a_second_writer_is_refused_and_told_who_holds_it() {
        let d = dir("lock");
        let p = d.join("orders-journal.lock");
        let first = RunLock::acquire(&p, "sync", Duration::ZERO).unwrap();
        let err = RunLock::acquire(&p, "cancel", Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("cancel: another run holds"), "{err}");
        if cfg!(unix) {
            // Windows locks the bytes too, so the holder's name is only readable on unix.
            assert!(err.contains("sync, pid"), "{err}");
            assert!(err.contains("since 20"), "{err}");
        }
        drop(first);
        RunLock::acquire(&p, "cancel", Duration::ZERO).expect("free again once released");
    }
}
