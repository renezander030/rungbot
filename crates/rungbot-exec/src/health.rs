//! `rungbot-exec health`: is the scheduled run still running, and can it trade?
//!
//! Read-only. It reads the run config, the decision log's last `run` line, the order
//! journal, the halt file and the balances snapshot, and calls no venue. Exit 1 when a
//! check fails, so a timer or a monitor can alert on it.

use std::path::Path;

use serde_json::{json, Value};

use crate::run::config::RunConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub level: Level,
    pub message: String,
}

fn check(level: Level, message: impl Into<String>) -> Check {
    Check {
        level,
        message: message.into(),
    }
}

/// The time and trade mode of the newest `run` line in a decision log.
pub fn last_run(log: &str) -> Option<(f64, String)> {
    log.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("kind")?.as_str()? != "run" {
            return None;
        }
        let ts = v.get("ts")?.as_f64()?;
        let mode = v
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Some((ts, mode))
    })
}

fn minutes(s: f64) -> String {
    format!("{:.0} min", s / 60.0)
}

/// Every check, in the order it prints.
pub fn report(cfg: &RunConfig, now: f64, max_age_min: f64) -> Vec<Check> {
    let mut out = Vec::new();

    let log = cfg.decisions_path();
    match std::fs::read_to_string(&log) {
        Err(_) => out.push(check(
            Level::Fail,
            format!(
                "no decision log at {}: no run has been recorded",
                log.display()
            ),
        )),
        Ok(text) => match last_run(&text) {
            None => out.push(check(
                Level::Fail,
                format!("{} holds no completed run", log.display()),
            )),
            Some((ts, mode)) => {
                let age = now - ts;
                let mode = if mode.is_empty() {
                    String::new()
                } else {
                    format!(" (mode {mode})")
                };
                if max_age_min > 0.0 && age > max_age_min * 60.0 {
                    out.push(check(
                        Level::Fail,
                        format!(
                            "last run {} ago{mode}, over --max-age {max_age_min} min",
                            minutes(age)
                        ),
                    ));
                } else {
                    out.push(check(
                        Level::Ok,
                        format!("last run {} ago{mode}", minutes(age)),
                    ));
                }
            }
        },
    }

    let jpath = cfg.journal_path();
    if jpath.exists() {
        match crate::store::load_journal(&jpath) {
            Ok(j) => {
                let open = j.open_orders(None);
                let failing = open.iter().filter(|o| o.last_error.is_some()).count();
                out.push(check(
                    Level::Ok,
                    format!("journal reads: {} open order(s)", open.len()),
                ));
                if failing > 0 {
                    out.push(check(
                        Level::Warn,
                        format!(
                            "{failing} open order(s) whose last poll failed (rungbot-exec status)"
                        ),
                    ));
                }
            }
            Err(e) => out.push(check(Level::Fail, format!("journal does not read: {e}"))),
        }
    } else {
        out.push(check(
            Level::Ok,
            format!("no journal yet at {}", jpath.display()),
        ));
    }

    let live = cfg.trade_mode == "live" && cfg.live_trading_enabled;
    out.push(check(
        Level::Ok,
        format!(
            "trade mode {}{}",
            cfg.trade_mode,
            if cfg.trade_mode == "live" && !cfg.live_trading_enabled {
                " (live_trading_enabled is off: nothing is placed)"
            } else {
                ""
            }
        ),
    ));
    if live && cfg.halt_file.exists() {
        out.push(check(
            Level::Warn,
            format!(
                "halt file {} is present: nothing is placed",
                cfg.halt_file.display()
            ),
        ));
    }

    let bal = crate::balances::balances_path(cfg);
    if let Some(ts) = snapshot_ts(&bal) {
        let age = now - ts;
        if age > 6.0 * 3600.0 {
            out.push(check(
                Level::Warn,
                format!("balances snapshot is {} old", minutes(age)),
            ));
        }
    }
    out
}

fn snapshot_ts(path: &Path) -> Option<f64> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("ts")?.as_f64()
}

pub fn to_json(checks: &[Check]) -> Value {
    json!({
        "ok": checks.iter().all(|c| c.level != Level::Fail),
        "checks": checks
            .iter()
            .map(|c| json!({ "level": c.level.as_str(), "message": c.message }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_run_line_wins_and_other_lines_are_skipped() {
        let log = concat!(
            "{\"ts\": 100.0, \"kind\": \"run\", \"mode\": \"dry\"}\n",
            "{\"ts\": 200.0, \"kind\": \"run\", \"mode\": \"live\"}\n",
            "{\"ts\": 250.0, \"kind\": \"skip\", \"src\": \"ladder\"}\n",
            "not json\n",
        );
        assert_eq!(last_run(log), Some((200.0, "live".into())));
        assert_eq!(last_run("{\"ts\": 1, \"kind\": \"done\"}\n"), None);
    }

    fn config(dir: &Path) -> RunConfig {
        let yaml = dir.join("run.yaml");
        std::fs::write(
            &yaml,
            format!(
                "watchlist:\n  AAA: a\nrouting:\n  AAA: gate AAA_USDT USDT\nstate_dir: \"{}\"\nhalt_file: \"{}\"\n",
                dir.display(),
                dir.join("HALT").display()
            ),
        )
        .unwrap();
        RunConfig::load(&yaml).unwrap()
    }

    #[test]
    fn a_run_older_than_the_limit_fails_and_a_fresh_one_passes() {
        let d = std::env::temp_dir().join(format!("rungbot-health-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let cfg = config(&d);

        let none = report(&cfg, 10_000.0, 60.0);
        assert_eq!(none[0].level, Level::Fail, "{none:?}");

        std::fs::write(
            cfg.decisions_path(),
            "{\"ts\": 1000.0, \"kind\": \"run\", \"mode\": \"live\"}\n",
        )
        .unwrap();
        let fresh = report(&cfg, 1000.0 + 30.0 * 60.0, 60.0);
        assert_eq!(fresh[0].level, Level::Ok, "{fresh:?}");
        assert!(fresh.iter().all(|c| c.level != Level::Fail), "{fresh:?}");

        let stale = report(&cfg, 1000.0 + 90.0 * 60.0, 60.0);
        assert_eq!(stale[0].level, Level::Fail);
        assert!(
            stale[0].message.contains("90 min ago"),
            "{}",
            stale[0].message
        );
        assert_eq!(to_json(&stale)["ok"], json!(false));

        std::fs::write(cfg.journal_path(), "{ not json").unwrap();
        let broken = report(&cfg, 1000.0, 60.0);
        assert!(broken
            .iter()
            .any(|c| c.level == Level::Fail && c.message.starts_with("journal does not read")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
