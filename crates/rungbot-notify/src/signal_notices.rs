//! Signal-mail dedupe: one mail per **new** signal state, one reminder a day, never a
//! 30-minute loop.
//!
//! When a ladder signal cannot execute (no free stable coin, below the venue minimum,
//! live trading blocked) the run rolls that coin's rung back on purpose, so the rung
//! fires again once budget appears. Mailed naively, that is the same DIP-BUY mail every
//! run. This module remembers what was announced, per coin and side:
//!
//! ```json
//! {"AKT|buy": {"count": 3, "first_ts": 1758000000.0, "last_sent_ts": 1758003700.0,
//!              "reason": "$0.00 USD < min $3.00", "rung": 2}}
//! ```
//!
//! A signal is mailed when (a) it is new (no entry, or a different rung), (b) it
//! executed, errored or was planned this run (that is news, and clears the entry so the
//! next occurrence is new again), or (c) the last mail for it is at least `remind_s`
//! old. A signal that stops firing is forgotten, so its return is news.
//!
//! The state file is byte-compatible with the Python bot's `signal-notices.json`
//! (`indent=1`, sorted keys, ASCII escapes) and is written with tmp-and-rename.
//! [`filter_signals`] itself is pure: state in, state out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::http::head;
use crate::pyjson;

/// Overrides for the state path and the reminder interval, as the Python bot read them.
pub const PATH_ENV: &str = "SIGNAL_NOTICES";
pub const REMIND_ENV: &str = "SIGNAL_REMIND_S";
pub const DEFAULT_REMIND_S: f64 = 24.0 * 3600.0;
pub const DEFAULT_FILE: &str = "signal-notices.json";

/// One remembered signal. Fields are optional because the file is a plain JSON map an
/// operator may edit; a missing field is read the way the Python bot's `.get` read it.
/// Declared in sorted order, so the file comes out with sorted keys.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NoticeEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_ts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent_ts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rung: Option<i64>,
}

/// `"SYM|side"` → entry.
pub type NoticeState = BTreeMap<String, NoticeEntry>;

/// A ladder signal as far as the dedupe cares: which coin, which rung.
pub trait Signal {
    fn sym(&self) -> &str;
    fn rung(&self) -> i64;
}

/// One execution result of this run for a coin and side. The first of `done`, `err`,
/// `skip`, `plan` that is non-empty is the outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    pub sym: String,
    pub side: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Done,
    Err,
    Skip,
    Plan,
}

/// This run's outcome for a signal, and its text.
pub fn outcome_for(
    sym: &str,
    side: &str,
    execution: &[ExecResult],
) -> Option<(OutcomeKind, String)> {
    for res in execution {
        if res.sym != sym || res.side != side {
            continue;
        }
        let kinds = [
            (OutcomeKind::Done, &res.done),
            (OutcomeKind::Err, &res.err),
            (OutcomeKind::Skip, &res.skip),
            (OutcomeKind::Plan, &res.plan),
        ];
        for (kind, text) in kinds {
            if let Some(t) = text.as_deref().filter(|t| !t.is_empty()) {
                return Some((kind, t.to_string()));
            }
        }
    }
    None
}

/// The signals worth mailing this run, and a log line for each one held back.
#[derive(Debug, PartialEq)]
pub struct Filtered<'a, T> {
    pub buys: Vec<&'a T>,
    pub sells: Vec<&'a T>,
    pub suppressed: Vec<String>,
}

/// Split this run's signals into (mail_buys, mail_sells, suppressed), updating `state`.
/// The caller persists `state` unless it is a dry run.
pub fn filter_signals<'a, T: Signal>(
    state: &mut NoticeState,
    buys: &'a [T],
    sells: &'a [T],
    execution: &[ExecResult],
    now: f64,
    remind_s: f64,
) -> Filtered<'a, T> {
    let mut keep_buys = Vec::new();
    let mut keep_sells = Vec::new();
    let mut suppressed = Vec::new();
    for (side, sigs) in [("buy", buys), ("sell", sells)] {
        let keep = if side == "buy" {
            &mut keep_buys
        } else {
            &mut keep_sells
        };
        for r in sigs {
            let (sym, rung) = (r.sym(), r.rung());
            let key = format!("{sym}|{side}");
            let outcome = outcome_for(sym, side, execution);
            let text = outcome.as_ref().map(|(_, t)| t.clone()).unwrap_or_default();
            if matches!(
                outcome,
                Some((OutcomeKind::Done | OutcomeKind::Err | OutcomeKind::Plan, _))
            ) {
                keep.push(r); // news: it executed, or failed trying
                state.remove(&key);
                continue;
            }
            let ent = state.get(&key).cloned();
            let fresh = ent.as_ref().is_none_or(|e| e.rung != Some(rung));
            let due = ent
                .as_ref()
                .is_some_and(|e| now - e.last_sent_ts.unwrap_or(0.0) >= remind_s);
            if fresh || due {
                keep.push(r);
                let prev = ent.unwrap_or_default();
                state.insert(
                    key,
                    NoticeEntry {
                        rung: Some(rung),
                        first_ts: Some(prev.first_ts.unwrap_or(now)),
                        last_sent_ts: Some(now),
                        count: Some(prev.count.unwrap_or(0) + 1),
                        reason: Some(head(&text, 160)),
                    },
                );
            } else if let Some(e) = state.get_mut(&key) {
                let count = e.count.unwrap_or(0) + 1;
                e.count = Some(count);
                let cut = head(&text, 160);
                e.reason = Some(if cut.is_empty() {
                    e.reason.clone().unwrap_or_default()
                } else {
                    cut
                });
                let ago = (now - e.last_sent_ts.unwrap_or(now)) / 3600.0;
                suppressed.push(format!(
                    "{sym} {side} rung {rung} (mailed {ago:.1}h ago, seen {count}x)"
                ));
            }
        }
    }
    // Entries for signals that stopped firing are stale: drop them so a return is news.
    let live: Vec<String> = buys
        .iter()
        .map(|r| format!("{}|buy", r.sym()))
        .chain(sells.iter().map(|r| format!("{}|sell", r.sym())))
        .collect();
    state.retain(|k, _| live.contains(k));
    Filtered {
        buys: keep_buys,
        sells: keep_sells,
        suppressed,
    }
}

/// `" — not executed: <reason>"` for the signal a subject line names (the first buy,
/// else the first sell) when it was skipped, else `""`.
pub fn subject_suffix<T: Signal>(buys: &[T], sells: &[T], execution: &[ExecResult]) -> String {
    let (top, side) = match (buys.first(), sells.first()) {
        (Some(b), _) => (b, "buy"),
        (None, Some(s)) => (s, "sell"),
        (None, None) => return String::new(),
    };
    match outcome_for(top.sym(), side, execution) {
        Some((OutcomeKind::Skip, text)) => format!(" \u{2014} not executed: {}", head(&text, 80)),
        _ => String::new(),
    }
}

/// The state file's exact text.
pub fn to_json(state: &NoticeState) -> String {
    pyjson::dumps_indent1(state)
}

/// Read the state; a missing or unreadable file is an empty state.
pub fn load(path: &Path) -> NoticeState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write the state through `<name>.tmp` and a rename, so a crash never leaves half a file.
pub fn save(path: &Path, state: &NoticeState) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, to_json(state))?;
    std::fs::rename(&tmp, path)
}

/// `SIGNAL_NOTICES` if set, else `<dir>/signal-notices.json`.
pub fn path_from_env(dir: &Path) -> PathBuf {
    std::env::var_os(PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(DEFAULT_FILE))
}

/// `SIGNAL_REMIND_S` if set and numeric, else a day.
pub fn remind_s_from_env() -> f64 {
    std::env::var(REMIND_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_REMIND_S)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Sig(&'static str, i64);
    impl Signal for Sig {
        fn sym(&self) -> &str {
            self.0
        }
        fn rung(&self) -> i64 {
            self.1
        }
    }

    fn skip(sym: &str, side: &str, why: &str) -> ExecResult {
        ExecResult {
            sym: sym.into(),
            side: side.into(),
            skip: Some(why.into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_standing_signal_is_mailed_once_then_held() {
        let mut st = NoticeState::new();
        let b = [Sig("AKT", 2)];
        let ex = [skip("AKT", "buy", "no budget")];
        let f = filter_signals(&mut st, &b, &[], &ex, 1000.0, 3600.0);
        assert_eq!(f.buys.len(), 1);
        let f = filter_signals(&mut st, &b, &[], &ex, 2800.0, 3600.0);
        assert!(f.buys.is_empty());
        assert_eq!(f.suppressed, ["AKT buy rung 2 (mailed 0.5h ago, seen 2x)"]);
    }

    #[test]
    fn an_empty_outcome_text_is_no_outcome() {
        let ex = [ExecResult {
            sym: "A".into(),
            side: "buy".into(),
            done: Some(String::new()),
            skip: Some("why".into()),
            ..Default::default()
        }];
        assert_eq!(
            outcome_for("A", "buy", &ex),
            Some((OutcomeKind::Skip, "why".into()))
        );
    }

    #[test]
    fn save_goes_through_a_temp_file_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("rungbot-notices-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(DEFAULT_FILE);
        let mut st = NoticeState::new();
        st.insert(
            "A|buy".into(),
            NoticeEntry {
                rung: Some(1),
                ..Default::default()
            },
        );
        save(&p, &st).unwrap();
        assert!(!dir.join("signal-notices.tmp").exists());
        assert_eq!(load(&p), st);
        std::fs::write(&p, "not json").unwrap();
        assert!(load(&p).is_empty(), "an unreadable file is an empty state");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
