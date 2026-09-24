//! Cross-language parity for the backtests.
//!
//! `tests/golden/*` were produced by running the reference implementation (Python) over
//! the frozen price fixtures in `tests/fixtures/`: public daily candles plus seeded
//! synthetic hourly walks, a neutral example book and neutral cost bases. Each test
//! replays the same inputs through this crate and requires the **same bytes** out — the
//! reports are a contract, since the monthly verdict parses them back.
//!
//! Two normalisations were applied to the monthly goldens when they were frozen, both
//! for lines only the reference can print: a failed sweep's full Python traceback is
//! reduced to its last line (with the bare cache file name), and the per-coin advice
//! names the config instead of the reference's cron wrapper.

use std::path::PathBuf;

use rungbot_backtest::monthly::{self, MonthlyOpts};
use rungbot_backtest::py::{loads, Py};
use rungbot_backtest::sweep::{self, SweepOpts};
use rungbot_backtest::window::{self, WindowOpts};
use rungbot_backtest::{invariants, Book, History};
use rungbot_core::regime::RegimeConfig;
use rungbot_core::{Bands, Coin, Config, Settings, Venue};

fn dir(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(sub)
}

fn read(sub: &str, name: &str) -> String {
    std::fs::read_to_string(dir(sub).join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn env_f(env: &Py, k: &str) -> Option<f64> {
    env.get(k).and_then(|v| v.as_f64())
}

/// The reference read its tunables from the environment; map them onto the config.
fn config(coins: &Py, env: &Py) -> Config {
    let mut s = Settings {
        bands: Bands {
            first_pct: env_f(env, "FIRST_PCT").unwrap_or(10.0),
            step_pct: env_f(env, "STEP_PCT").unwrap_or(5.0),
        },
        ..Settings::default()
    };
    macro_rules! knob {
        ($($k:literal => $f:ident),*) => {$( if let Some(v) = env_f(env, $k) { s.$f = v; } )*};
    }
    knob! {
        "MIN_TRADE_PCT" => min_trade_pct, "MIN_CORE_PCT" => min_core_pct,
        "WINDOW_HOURS" => window_hours, "BUY_FLOOR_PCT" => buy_floor_pct,
        "TARGET_PCT" => target_pct, "BREAKER_PCT" => breaker_pct,
        "BREAKER_DAYS" => breaker_days
    };
    let per_coin = env
        .get("BANDS_JSON")
        .and_then(|v| v.as_str())
        .map(|t| loads(t).expect("BANDS_JSON parses"));
    let coins = coins
        .as_list()
        .iter()
        .map(|c| {
            let sym = c.get("sym").and_then(|v| v.as_str()).unwrap().to_string();
            let bands = per_coin.as_ref().and_then(|b| b.get(&sym)).map(|fs| Bands {
                first_pct: fs.as_list()[0].as_f64().unwrap(),
                step_pct: fs.as_list()[1].as_f64().unwrap(),
            });
            Coin {
                venue: Venue::parse(c.get("venue").and_then(|v| v.as_str()).unwrap()).unwrap(),
                pair: c.get("pair").and_then(|v| v.as_str()).unwrap().to_string(),
                name: sym.clone(),
                entry: c.get("entry").and_then(|v| v.as_f64()),
                bands,
                symbol: sym,
            }
        })
        .collect();
    Config::new(coins, s).unwrap()
}

fn regime(env: &Py) -> RegimeConfig {
    let mut r = RegimeConfig::default();
    if let Some(v) = env_f(env, "RUN_MIN_SIGNALS") {
        r.run_min_signals = v as usize;
    }
    r
}

fn show_diff(name: &str, want: &str, got: &str) {
    if want != got {
        for (k, (a, b)) in want.lines().zip(got.lines()).enumerate() {
            if a != b {
                panic!("{name}: first difference at line {k}:\n  want: {a}\n   got: {b}\n\nwant:\n{want}\ngot:\n{got}");
            }
        }
        panic!("{name}: outputs differ in length\nwant:\n{want}\ngot:\n{got}");
    }
}

#[test]
fn invariants_report_matches_the_reference() {
    let (text, ok) = invariants::run();
    assert!(ok);
    show_diff("invariants", &read("golden", "invariants.txt"), &text);
}

#[test]
fn window_and_sweep_reports_match_the_reference() {
    let golden = loads(&read("golden", "window_sweep.json")).unwrap();
    let book = Book::parse(&read("fixtures", "book.json")).unwrap();
    let cases = golden.as_list();
    assert!(cases.len() >= 9, "the golden file lost scenarios");
    for case in cases {
        let sc = case.get("scenario").unwrap();
        let name = sc.get("name").and_then(|v| v.as_str()).unwrap();
        let env = sc.get("env").unwrap();
        let cfg = config(sc.get("coins").unwrap(), env);
        let days = sc.get("days").and_then(|v| v.as_f64()).unwrap() as i64;
        let hist_file = sc
            .get("hist")
            .and_then(|h| h.get(&days.to_string()))
            .and_then(|v| v.as_str())
            .unwrap();
        let hist = History::parse(&read("fixtures", hist_file)).unwrap();
        let opts = WindowOpts {
            days,
            max_order_usd: env_f(env, "MAX_ORDER_USD").unwrap_or(50.0),
            bag: sc.get("bag").and_then(|v| v.as_f64()),
        };
        let run = window::run(&cfg, Ok(&hist), &book, &opts);
        show_diff(
            &format!("{name} window"),
            case.get("window_stdout").and_then(|v| v.as_str()).unwrap(),
            &run.text,
        );
        let written = run.book.expect("a book");
        assert_eq!(
            written.to_py().json(None),
            case.get("book_written").unwrap().json(None),
            "{name}: the start book handed to the sweep"
        );
        let so = SweepOpts {
            days,
            bag: env_f(env, "BT_BAG"),
            regime: regime(env),
        };
        let sw = sweep::run(&cfg, Ok(&hist), Some(&written), &so).unwrap();
        show_diff(
            &format!("{name} sweep"),
            case.get("sweep_stdout").and_then(|v| v.as_str()).unwrap(),
            &sw,
        );
    }
}

#[test]
fn monthly_verdicts_match_the_reference() {
    let golden = loads(&read("golden", "monthly.json")).unwrap();
    let book = Book::parse(&read("fixtures", "book.json")).unwrap();
    let coins = loads(
        r#"[{"sym": "BTC", "venue": "binance", "pair": "BTCUSDT", "entry": 60000},
            {"sym": "FET", "venue": "binance", "pair": "FETUSDT", "entry": 0.5},
            {"sym": "OSMO", "venue": "gate", "pair": "OSMO_USDT", "entry": 0.3}]"#,
    )
    .unwrap();
    let files = [
        (28, "hist_hourly_28d.json"),
        (90, "hist_hourly_90d.json"),
        (180, "hist_daily_180d.json"),
    ];
    for case in golden.as_list() {
        let name = case.get("name").and_then(|v| v.as_str()).unwrap();
        let env = case.get("env").unwrap();
        let cfg = config(&coins, env);
        let windows: Vec<i64> = env
            .get("BT_WINDOWS")
            .and_then(|v| v.as_str())
            .unwrap()
            .split(',')
            .map(|w| w.trim().parse().unwrap())
            .collect();
        let opts = MonthlyOpts {
            windows,
            drift_band_pct: env_f(env, "DRIFT_BAND_PCT").unwrap_or(5.0),
            percoin: env.get("BT_PERCOIN").and_then(|v| v.as_str()) != Some("0"),
            percoin_min_gain: env_f(env, "BT_PERCOIN_MIN_GAIN").unwrap_or(2.0),
            now: 1_790_000_000,
            ..MonthlyOpts::default()
        };
        let history_for = |days: i64| match files.iter().find(|(d, _)| *d == days) {
            Some((_, f)) => History::parse(&read("fixtures", f)),
            None => Err(format!(
                "history BTC failed: offline fixture: no {days}d history for btc-id"
            )),
        };
        let report = monthly::run(&cfg, &book, &history_for, &opts);
        show_diff(
            name,
            case.get("stdout").and_then(|v| v.as_str()).unwrap(),
            &report.dry_run_text(),
        );
        let want_exp = case.get("expectation").and_then(|v| v.as_str());
        assert_eq!(
            want_exp,
            report.expectation.as_deref(),
            "{name}: expectation"
        );
    }
}
