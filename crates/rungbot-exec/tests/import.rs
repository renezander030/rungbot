//! `import-cex` over a synthetic source directory holding every state file the
//! reference kept: where each one goes, that the copies are byte-identical, that a dry
//! run writes nothing, that a second import is a no-op and that state which moved on is
//! only replaced with `--force`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rungbot_exec::import::{self, Body, Status, Targets};
use rungbot_exec::run::config::RunConfig;

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("rungbot-import-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const JOURNAL: &str = r#"{
  "csAAAb1": {"client_id": "csAAAb1", "exch": "gate", "sym": "AAA", "pair": "AAA_USDT",
              "side": "buy", "kind": "market_buy", "status": "filled", "quote": 10.0,
              "ts": 1700000000.0, "filled_ts": 1700000001.0, "filled_base": 5.0,
              "filled_price": 2.0}
}"#;

/// Every source file, synthetic values only.
const COPIED: [(&str, &str); 22] = [
    ("ladder-state.dry.json", r#"{"AAA": {"buy": 1, "sell": 0}}"#),
    ("ladder-state.json", r#"{"AAA": {"buy": 0, "sell": 0}}"#),
    (
        "regime-state.json",
        r#"{"ts": 1700000000, "market": "chop"}"#,
    ),
    (
        "regime-history.json",
        r#"{"labels": [["2023-11-14", "chop"]]}"#,
    ),
    ("froth-state.json", r#"{"armed": false, "ts": 1700000000}"#),
    ("zone-state.json", r#"{"zone": "mid"}"#),
    ("divergence-state.json", r#"{"baseline": 100.0}"#),
    ("market-verdict.json", r#"{"verdict": "neutral"}"#),
    (
        "backtest-expectation.json",
        r#"{"epoch": 1700000000, "alpha_pct": 1.5}"#,
    ),
    ("opportunity-ledger.json", r#"{"candidates": []}"#),
    ("value-index.json", r#"{"AAA": {"tvl": 1}}"#),
    ("unlock-index.json", r#"{"AAA": {"slug": "aaa"}}"#),
    ("theses.yaml", "bull:\n  screen: momentum\n"),
    (
        "dashboard/manual-fills.json",
        r#"[{"sym": "AAA", "qty": 1.0}]"#,
    ),
    ("dashboard/wallet-targets.json", r#"{"AAA": 1.2}"#),
    ("dashboard/.deploy-status.json", r#"{"fails": 0}"#),
    ("dashboard/.revx-cache.json", r#"{"ts": 1700000000}"#),
    ("dashboard/.wallets-alert-state.json", "{}"),
    ("dashboard/.wallets-last-good.json", r#"{"ts": 1700000000}"#),
    (
        "replay/cache/binance_AAAUSDT.json",
        "[[1700000000000, 2.0]]",
    ),
    ("replay/cache/gate_BBB_USDT.json", "[[1700000000, 3.0]]"),
    ("deploy-state.live.json", r#"{"stable": {"gate": 100.0}}"#),
];

const IGNORED: [&str; 4] = [
    "revx-journal.jsonl",
    "revx-state.json",
    ".run.lock",
    "divergence-state.json.bak-1",
];

const MARKERS: [&str; 2] = ["audit-state.json.last", "orders-archive.jsonl.last"];

fn source(dir: &Path) -> SystemTime {
    std::fs::write(dir.join("orders-journal.json"), JOURNAL).unwrap();
    for (rel, body) in COPIED {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    for n in IGNORED {
        std::fs::write(dir.join(n), "{}").unwrap();
    }
    std::fs::create_dir_all(dir.join("dashboard/public")).unwrap();
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for m in MARKERS {
        let f = std::fs::File::create(dir.join(m)).unwrap();
        f.set_modified(t).unwrap();
    }
    t
}

/// Where each source file lands under [`Targets::beside`].
fn expected(out: &Path, rel: &str) -> PathBuf {
    let name = Path::new(rel).file_name().unwrap().to_str().unwrap();
    match rel {
        "ladder-state.dry.json" => out.join("ladder-state.dry.json"),
        "ladder-state.json" => out.join("ladder-state.off.json"),
        "deploy-state.live.json" => out.join("deploy-state.json"),
        r if r.starts_with("dashboard/") => out.join("dashboard").join(name),
        r if r.starts_with("replay/cache/") => out.join("replay").join("cache").join(name),
        "opportunity-ledger.json" | "value-index.json" | "unlock-index.json" | "theses.yaml" => {
            out.join("research").join(name)
        }
        _ => out.join(name),
    }
}

#[test]
fn every_state_file_is_mapped_copied_and_checked() {
    let src = Scratch::new("src");
    let out = Scratch::new("out");
    let t0 = source(&src.0);
    let imp = import::read_dir(&src.0).unwrap();
    let target = out.0.join("state").join("orders-journal.json");
    let dest = out.0.join("state");
    let targets = Targets::beside(&target);

    // The dry run: a mapping line per file, a reason per file left out, nothing on disk.
    let map = import::mapping_lines(&imp, &targets).join("\n");
    for (rel, _) in COPIED {
        let want = format!("{rel} -> {}: new", expected(&dest, rel).display());
        assert!(map.contains(&want), "{want}\n{map}");
    }
    for m in MARKERS {
        assert!(map.contains(&format!("{m} -> ")), "{m}\n{map}");
    }
    for n in [
        "revx-journal.jsonl: not imported",
        "revx-state.json: not imported",
        ".run.lock: not imported",
        "divergence-state.json.bak-1: not imported",
        "dashboard/public: not imported",
        "halt and sell-arm files: not copied",
    ] {
        assert!(map.contains(n), "{n}\n{map}");
    }
    assert!(!dest.exists(), "a dry run wrote something");

    // The write: copies byte-identical, markers keep their time, the journal last.
    let written = import::write_to(&imp, &targets, false).unwrap();
    assert_eq!(written.last().unwrap().source, "orders-journal.json");
    for (rel, body) in COPIED {
        if rel == "deploy-state.live.json" {
            continue; // re-serialised in the deploy layer's own format
        }
        assert_eq!(
            std::fs::read_to_string(expected(&dest, rel)).unwrap(),
            body,
            "{rel}"
        );
    }
    for (m, target) in [
        ("audit-state.json.last", dest.join("audit-state.json.last")),
        (
            "orders-archive.jsonl.last",
            dest.join("orders-archive.jsonl.last"),
        ),
    ] {
        let got = std::fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(got, t0, "{m}");
    }
    assert!(!dest.join("revx-state.json").exists());

    // Again: a no-op.
    let again = import::write_to(&imp, &targets, false).unwrap();
    assert!(
        again.is_empty(),
        "{:?}",
        again.iter().map(|s| &s.source).collect::<Vec<_>>()
    );
    assert!(import::plan(&imp, &targets)
        .iter()
        .all(|s| s.status() == Status::Same));

    // State that moved on is refused, by name, and nothing is written; --force replaces.
    std::fs::write(dest.join("zone-state.json"), r#"{"zone": "low"}"#).unwrap();
    std::fs::write(dest.join("research/value-index.json"), "{}").unwrap();
    let e = import::write_to(&imp, &targets, false).unwrap_err();
    assert!(
        e.contains("zone-state.json") && e.contains("--force"),
        "{e}"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("research/value-index.json")).unwrap(),
        "{}",
        "a refused import wrote"
    );
    import::write_to(&imp, &targets, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join("zone-state.json")).unwrap(),
        r#"{"zone": "mid"}"#
    );
}

#[test]
fn a_copied_file_that_does_not_parse_stops_the_import() {
    let src = Scratch::new("bad");
    source(&src.0);
    std::fs::write(src.0.join("froth-state.json"), "{half").unwrap();
    let e = import::read_dir(&src.0).unwrap_err();
    assert!(e.contains("froth-state.json"), "{e}");
}

#[test]
fn the_run_config_decides_where_state_lands() {
    let y = "watchlist:\n  AAA: aaa\nrouting:\n  AAA: gate AAA_USDT USDT\n\
             state_dir: /s\nregime_state: /r/regime.json\nfroth_state: /f/froth.json\n\
             deploy_state: /d/deploy.json\n";
    let cfg = RunConfig::from_yaml(y, &|_| None).unwrap();
    let t = Targets::from_config(&cfg);
    assert_eq!(t.journal, Path::new("/s/orders-journal.json"));
    assert_eq!(t.regime, Path::new("/r/regime.json"));
    assert_eq!(t.regime_history, Path::new("/r/regime-history.json"));
    assert_eq!(t.froth, Path::new("/f/froth.json"));
    assert_eq!(t.deploy, Path::new("/d/deploy.json"));
    assert_eq!(t.ladder_live, Path::new("/s/ladder-state.json"));
    assert_eq!(t.ladder_dry, Path::new("/s/ladder-state.dry.json"));
    assert_eq!(t.ladder_off, Path::new("/s/ladder-state.off.json"));
    assert_eq!(t.candles_dir, Path::new("/s/replay/cache"));
    assert_eq!(t.research_dir, Path::new("/s/research"));
}

#[test]
fn the_cli_dry_run_prints_the_mapping_and_writes_nothing() {
    let src = Scratch::new("cli-src");
    let out = Scratch::new("cli-out");
    source(&src.0);
    let journal = out.0.join("st").join("orders-journal.json");
    let run = |extra: &[&str]| {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_rungbot-exec"));
        c.arg("import-cex")
            .arg(&src.0)
            .args(["--journal", journal.to_str().unwrap()])
            .args(extra)
            .env("RUNGBOT_OFFLINE", "1")
            .env("RUNGBOT_RUN_CONFIG", "")
            .env("XDG_CONFIG_HOME", out.0.join("cfg"))
            .env_remove("RUNGBOT_LOCK")
            .env_remove("RUNGBOT_ORDER_ARCHIVE");
        c.output().unwrap()
    };
    let o = run(&["--dry-run"]);
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.contains("mapping (source -> target: status):"),
        "{text}"
    );
    assert!(text.contains("dry run: nothing written"), "{text}");
    assert!(!out.0.join("st").exists());
    let o = run(&["--write"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = run(&["--write"]);
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("nothing to write"),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    // Body of a marker is not text.
    assert!(import::plan(
        &import::read_dir(&src.0).unwrap(),
        &Targets::beside(&journal)
    )
    .iter()
    .any(|s| matches!(s.body, Body::Marker(_))));
}
