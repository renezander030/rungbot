//! Ladder state on disk: the high-water rungs and ledgers that make a rung fire once.
//!
//! The write is atomic on purpose. A kill mid-write would otherwise leave half-written
//! JSON that reads as empty on the next run, and an empty state re-fires every rung.

use std::path::{Path, PathBuf};

use rungbot_core::State;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct OnDisk {
    version: u32,
    coins: State,
}

/// `$RUNGBOT_STATE`, else `$XDG_STATE_HOME/rungbot/state.json`, else `~/.local/state/...`.
pub fn default_path() -> PathBuf {
    if let Ok(p) = std::env::var("RUNGBOT_STATE") {
        return PathBuf::from(p);
    }
    base_dir("XDG_STATE_HOME", ".local/state")
        .join("rungbot")
        .join("state.json")
}

/// `$RUNGBOT_CONFIG`, else `$XDG_CONFIG_HOME/rungbot/watchlist.yaml`, else `~/.config/...`.
pub fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("RUNGBOT_CONFIG") {
        return PathBuf::from(p);
    }
    base_dir("XDG_CONFIG_HOME", ".config")
        .join("rungbot")
        .join("watchlist.yaml")
}

fn base_dir(env_key: &str, fallback: &str) -> PathBuf {
    match std::env::var(env_key) {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Prior state, or an empty map on a cold start or an unreadable file.
pub fn load(path: &Path) -> State {
    if !path.exists() {
        return State::new();
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "WARN: unreadable state at {} ({e}); starting cold",
                path.display()
            );
            return State::new();
        }
    };
    match serde_json::from_str::<OnDisk>(&raw) {
        Ok(d) => d.coins,
        // Tolerate a bare map, which is what a hand-edited file usually looks like.
        Err(_) => serde_json::from_str::<State>(&raw).unwrap_or_else(|e| {
            eprintln!(
                "WARN: unparseable state at {} ({e}); starting cold",
                path.display()
            );
            State::new()
        }),
    }
}

/// Atomic write. Failure warns and continues: a report is still worth printing.
pub fn save(path: &Path, state: &State) {
    let body = match serde_json::to_string_pretty(&OnDisk {
        version: VERSION,
        coins: state.clone(),
    }) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("WARN: could not serialise state: {e}");
            return;
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("WARN: could not create {}: {e}", parent.display());
            return;
        }
    }
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, body) {
        eprintln!("WARN: could not write state to {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        eprintln!("WARN: could not replace {}: {e}", path.display());
    }
}
