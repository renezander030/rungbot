//! Path precedence exercised through the shipped CLI with isolated environment and files.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rungbot-paths-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn command(&self, home: bool, xdg_key: &str, xdg: Option<&str>) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rungbot"));
        cmd.env_clear()
            .current_dir(&self.0)
            .env("RUNGBOT_OFFLINE", "1");
        // Windows needs its system directory to resolve system libraries.
        if let Some(root) = std::env::var_os("SystemRoot") {
            cmd.env("SystemRoot", root);
        }
        if home {
            cmd.env("HOME", self.0.join("home"));
        }
        if let Some(value) = xdg {
            cmd.env(xdg_key, value);
        }
        cmd
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cases() -> Vec<(bool, Option<&'static str>, Option<&'static str>)> {
    let mut cases = vec![
        (true, None, None),
        (true, Some(""), None),
        (true, Some("   \t"), None),
        (true, Some("custom root"), None),
        (false, None, None),
        (false, Some(" \t"), None),
        (true, Some(" \t"), Some("explicit root")),
        (true, Some("custom root"), Some("explicit root")),
    ];
    if cfg!(unix) {
        // Meaningful paths retain their leading and trailing spaces verbatim.
        cases.push((true, Some(" custom "), None));
    }
    cases
}

fn expected_root(f: &Fixture, home: bool, xdg: Option<&str>, fallback: &str) -> PathBuf {
    match xdg {
        Some(v) if v.chars().any(|c| !c.is_whitespace()) => PathBuf::from(v),
        _ if home => f.0.join("home").join(fallback),
        _ => Path::new(".").join(fallback),
    }
}

#[test]
fn replay_cache_path_obeys_xdg_fallback_and_explicit_override() {
    for (home, xdg, explicit) in cases() {
        let f = Fixture::new();
        std::fs::write(
            f.0.join("study.json"),
            r#"{"btc_source":"binance:AAA/USD"}"#,
        )
        .unwrap();
        let mut cmd = f.command(home, "XDG_CACHE_HOME", xdg);
        cmd.args(["backtest", "replay", "btc-confirm", "--study", "study.json"]);
        let expected = if let Some(path) = explicit {
            cmd.args(["--cache-dir", path]);
            PathBuf::from(path)
        } else {
            expected_root(&f, home, xdg, ".cache").join("rungbot/backtest")
        }
        .join("candles/binance_AAA_USD.json");
        let out = cmd.output().unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(!out.status.success());
        assert!(
            stderr.contains(&format!("at {} -- run", expected.display())),
            "home={home}, xdg={xdg:?}, explicit={explicit:?}: {stderr}"
        );
        assert!(!f.0.join(" \t").exists());
    }
}

#[test]
fn monthly_expectation_path_obeys_xdg_fallback_and_explicit_override() {
    for (home, xdg, explicit) in cases() {
        let f = Fixture::new();
        std::fs::write(
            f.0.join("config.yaml"),
            "coins:\n  AAA:\n    venue: binance\n    pair: AAA/USD\n    entry: 100\n",
        )
        .unwrap();
        std::fs::write(
            f.0.join("book.json"),
            r#"{"held":{"AAA":1},"stable":{"binance":100},"min_notional":{"AAA":1}}"#,
        )
        .unwrap();
        let points: Vec<_> = (0..30).map(|i| (i * 86400, 100 + i % 4)).collect();
        std::fs::write(
            f.0.join("history.json"),
            serde_json::to_vec(&serde_json::json!({"AAA": points})).unwrap(),
        )
        .unwrap();
        let mut cmd = f.command(home, "XDG_STATE_HOME", xdg);
        cmd.args([
            "backtest",
            "monthly",
            "--config",
            "config.yaml",
            "--book",
            "book.json",
            "--history",
            "history.json",
            "--windows",
            "28",
            "--no-percoin",
            "--dry-run",
            "--now",
            "2505600",
        ]);
        let expected = if let Some(path) = explicit {
            let path = Path::new(path).join("expectation.json");
            cmd.arg("--expect").arg(&path);
            path
        } else {
            expected_root(&f, home, xdg, ".local/state").join("rungbot/backtest-expectation.json")
        };
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let actual = f.0.join(&expected);
        assert!(
            actual.is_file(),
            "home={home}, xdg={xdg:?}, explicit={explicit:?}: {}",
            actual.display()
        );
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(actual).unwrap()).unwrap();
        assert!(doc.is_object());
        assert!(!f.0.join(" \t").exists());
    }
}
