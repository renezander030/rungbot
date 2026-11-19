//! Incomplete price caches are input errors, not replay panics.
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rungbot-history-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "coins:\n  AAA:\n    venue: binance\n    pair: AAA/USD\n    entry: 100\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("book.json"),
            r#"{"held":{"AAA":1},"stable":{"binance":100},"min_notional":{"AAA":1}}"#,
        )
        .unwrap();
        Self(dir)
    }

    fn history(&self, symbol: &str) {
        let points: Vec<_> = (0..30).map(|i| (i * 86400, 100 + i % 4)).collect();
        let text = serde_json::to_vec(&serde_json::json!({symbol: points})).unwrap();
        std::fs::write(self.0.join("history.json"), &text).unwrap();
        std::fs::write(self.0.join("cache/bt-hist-28d.json"), text).unwrap();
    }

    fn run(&self, subcommand: &str, explicit: bool) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rungbot"));
        cmd.env_clear()
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("RUNGBOT_OFFLINE", "1")
            .args([
                "backtest",
                subcommand,
                "--config",
                "config.yaml",
                "--book",
                "book.json",
                "--cache-dir",
                "cache",
            ]);
        if let Some(root) = std::env::var_os("SystemRoot") {
            cmd.env("SystemRoot", root);
        }
        if explicit {
            cmd.args(["--history", "history.json"]);
        }
        if subcommand == "monthly" {
            cmd.args([
                "--windows",
                "28",
                "--dry-run",
                "--expect",
                "expect.json",
                "--now",
                "1700000000",
            ]);
        }
        cmd.output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn window_rejects_missing_configured_history_from_file_and_cache() {
    for explicit in [true, false] {
        let f = Fixture::new();
        f.history("BBB");
        let out = f.run("window", explicit);
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("history missing configured coin AAA"),
            "{stderr}"
        );
        assert!(!stderr.contains("panicked"), "{stderr}");
        assert!(!f.0.join("cache/bt-book.json").exists());
    }
}

#[test]
fn monthly_reports_missing_history_without_panicking_or_writing_expectation() {
    for explicit in [true, false] {
        let f = Fixture::new();
        f.history("BBB");
        let out = f.run("monthly", explicit);
        let stdout = String::from_utf8(out.stdout).unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(out.status.success(), "{stderr}");
        assert!(
            stdout.contains("history missing configured coin AAA"),
            "{stdout}"
        );
        assert!(!stderr.contains("panicked"), "{stderr}");
        assert!(!f.0.join("expect.json").exists());
    }
}

#[test]
fn complete_history_keeps_file_and_cache_reports_identical() {
    let f = Fixture::new();
    f.history("AAA");
    for command in ["window", "monthly"] {
        let explicit = f.run(command, true);
        let cached = f.run(command, false);
        assert!(explicit.status.success());
        assert!(cached.status.success());
        assert_eq!(explicit.stdout, cached.stdout);
        assert_eq!(explicit.stderr, cached.stderr);
    }
}

#[test]
fn sweep_retains_its_missing_history_exclusion() {
    let f = Fixture::new();
    f.history("BBB");
    let out = f.run("sweep", true);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success());
    assert!(
        stdout.contains("no cached history for AAA -- excluded from the sweep"),
        "{stdout}"
    );
}
