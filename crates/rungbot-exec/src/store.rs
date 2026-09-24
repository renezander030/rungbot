//! Where the journal lives on disk, and who may touch it.
//!
//! Two rules, both about not losing the one record that stops a double trade:
//!
//! * A journal that exists but does not parse is an **error**, never an empty journal.
//!   Read as empty, every order in it is forgotten: the next save overwrites the file,
//!   and an intent that was already placed no longer looks placed.
//! * One writer at a time. `sync` and `cancel` load the journal, call the venue and save
//!   it back. Two of them at once (a schedule and a hand-run command) can both decide an
//!   id is not journaled yet and both place it, and the later save drops what the other
//!   one wrote. [`RunLock`] makes the second one wait, then give up naming the holder.
//!   The lock is an OS file lock, so a crashed process never leaves it behind.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::journal::Journal;

/// Load the journal. A missing file is a fresh start; anything else that cannot be read
/// or parsed is an error that names the file.
pub fn load_journal(path: &Path) -> Result<Journal, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Journal::default()),
        Err(e) => return Err(format!("cannot read the journal {}: {e}", path.display())),
    };
    serde_json::from_str(&raw).map_err(|e| {
        format!(
            "the journal {} exists but does not parse ({e}); refusing to run on an empty \
             journal. Restore it from {} or a backup, or move it aside to start fresh",
            path.display(),
            path.with_extension("tmp").display()
        )
    })
}

/// Write the journal atomically: a temp file, then a rename over the old one.
pub fn save_journal(path: &Path, j: &Journal) -> Result<(), String> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("cannot create {}: {e}", p.display()))?;
    }
    let body = serde_json::to_string_pretty(j).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// The lock file that sits next to a journal.
pub fn lock_path(journal: &Path) -> PathBuf {
    journal.with_extension("lock")
}

/// An exclusive hold on a journal. Released when dropped, or by the OS if the process dies.
#[derive(Debug)]
pub struct RunLock {
    file: File,
}

impl RunLock {
    /// Take the lock for `who`, waiting up to `wait` for another holder to finish.
    pub fn acquire(path: &Path, who: &str, wait: Duration) -> Result<RunLock, String> {
        if let Some(p) = path.parent() {
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
                    let _ = file.read_to_string(&mut holder);
                    let holder = holder.trim();
                    return Err(format!(
                        "another rungbot-exec run holds {} ({}); waited {}s",
                        path.display(),
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
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "{who}, pid {}", std::process::id());
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
    use crate::journal::{Order, Side, Status};

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rungbot-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn order(cid: &str) -> Order {
        Order {
            client_id: cid.into(),
            sym: "AAA".into(),
            pair: "AAA_USDT".into(),
            venue: "gate".into(),
            side: Side::Buy,
            kind: "ladder_buy".into(),
            price: 1.0,
            base: 10.0,
            quote: 10.0,
            status: Status::Open,
            placed_ts: 1.0,
            venue_order_id: Some("1".into()),
            filled_ts: None,
            filled_base: None,
            filled_quote: None,
            avg_price: None,
            status_ts: None,
            note: None,
            swept: false,
        }
    }

    #[test]
    fn a_missing_journal_is_a_fresh_start() {
        let d = dir();
        let j = load_journal(&d.join("orders-journal.json")).unwrap();
        assert!(!j.exists("anything"));
    }

    #[test]
    fn a_journal_round_trips() {
        let d = dir();
        let p = d.join("orders-journal.json");
        let mut j = Journal::default();
        j.record(order("csAAAb1r1"));
        save_journal(&p, &j).unwrap();
        assert!(load_journal(&p).unwrap().exists("csAAAb1r1"));
    }

    #[test]
    fn a_corrupt_journal_is_an_error_and_is_left_as_it_was() {
        let d = dir();
        let p = d.join("orders-journal.json");
        let mut j = Journal::default();
        j.record(order("csAAAb1r1"));
        save_journal(&p, &j).unwrap();
        let good = std::fs::read_to_string(&p).unwrap();
        let cut = &good[..good.len() / 2];
        std::fs::write(&p, cut).unwrap();
        let err = load_journal(&p).unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
        assert!(err.contains("orders-journal.json"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            cut,
            "nothing was overwritten"
        );
    }

    #[test]
    fn a_second_writer_is_refused_and_told_who_holds_it() {
        let d = dir();
        let p = lock_path(&d.join("orders-journal.json"));
        let first = RunLock::acquire(&p, "sync", Duration::ZERO).unwrap();
        let err = RunLock::acquire(&p, "cancel", Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("another rungbot-exec run"), "{err}");
        if cfg!(unix) {
            // Windows locks the bytes too, so the holder's name is only readable on unix.
            assert!(err.contains("sync, pid"), "{err}");
        }
        drop(first);
        RunLock::acquire(&p, "cancel", Duration::ZERO).expect("free again once released");
    }
}
