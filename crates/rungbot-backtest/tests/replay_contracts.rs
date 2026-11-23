//! Replay contracts exercised through the public simulation interfaces.
use rungbot_backtest::py::Py;
use rungbot_backtest::replay::alt_top::{analyse, BtcTop, Ladder, Series};
use rungbot_backtest::replay::data::{day_num, Bar};
use rungbot_backtest::window::{self, WindowOpts};
use rungbot_backtest::{Book, History};
use rungbot_core::{Coin, Config, Settings, Venue};

fn window_report(points: Vec<(i64, f64)>) -> String {
    let cfg = Config::new(
        vec![Coin {
            symbol: "AAA".into(),
            venue: Venue::Binance,
            pair: "AAA/USD".into(),
            name: "AAA".into(),
            entry: Some(100.0),
            bands: None,
        }],
        Settings::default(),
    )
    .unwrap();
    let book = Book {
        held: vec![("AAA".into(), 1.0)],
        stable: vec![("binance".into(), 100.0)],
        min_notional: vec![("AAA".into(), 1.0)],
        counts: vec![("binance".into(), 1)],
    };
    let history = History(vec![("AAA".into(), points)]);
    history.check().unwrap();
    window::run(&cfg, Ok(&history), &book, &WindowOpts::default()).text
}

#[test]
fn window_only_trades_a_dip_present_in_the_supplied_samples() {
    let flat = window_report(vec![(0, 100.0), (86_400, 100.0), (172_800, 100.0)]);
    let dip = window_report(vec![(0, 100.0), (86_400, 80.0), (172_800, 100.0)]);
    assert!(flat.contains("TRADES: 0 buys"), "{flat}");
    assert!(dip.contains("TRADES: 1 buys"), "{dip}");
}

#[test]
fn window_uses_the_supplied_hourly_or_daily_cadence() {
    let daily = window_report(vec![(0, 100.0), (86_400, 80.0), (172_800, 100.0)]);
    let hourly = window_report(
        (0..=48)
            .map(|i| (i * 3_600, if i == 24 { 80.0 } else { 100.0 }))
            .collect(),
    );
    assert!(daily.contains("3 daily points"), "{daily}");
    assert!(hourly.contains("49 hourly points"), "{hourly}");
    assert!(daily.contains("TRADES: 1 buys"), "{daily}");
    assert!(hourly.contains("TRADES: 1 buys"), "{hourly}");
}

fn stop(stops: &Py, percentage: i64) -> &Py {
    stops
        .items()
        .iter()
        .find(|(key, _)| matches!(key, Py::Int(value) if *value == percentage))
        .map(|(_, value)| value)
        .expect("requested trailing-stop percentage")
}

#[test]
fn trailing_stop_reports_the_absolute_date_at_the_inclusive_window_end() {
    let bars = [
        ("2024-08-01", 100.0),
        ("2024-08-02", 90.0),
        ("2024-10-01", 200.0),
        ("2024-10-02", 180.0),
    ]
    .into_iter()
    .map(|(date, close)| Bar {
        date: date.into(),
        open: close,
        high: close,
        low: close,
        close,
    })
    .collect();
    let series = Series::merge(&[("synthetic".into(), bars)]);
    let reference_top = BtcTop {
        date: day_num("2024-10-01"),
        close: 200.0,
        signals: vec![],
        last_before_top: vec![],
        mayer_at_top: 0.0,
        ret30_at_top: 0.0,
    };
    let reports = analyse(
        &Ladder::default(),
        "AAA",
        &series,
        &[("2025".into(), reference_top)],
    );
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert_eq!(
        report.get("cost_date").and_then(Py::as_str),
        Some("2024-08-02")
    );
    let stops = report.get("trail_stops").unwrap();
    let leg_stops = report
        .get("final_leg")
        .unwrap()
        .get("trail_stops_from_leg_low")
        .unwrap();
    for stops in [stops, leg_stops] {
        assert_eq!(
            stop(stops, 10).get("exit").and_then(Py::as_str),
            Some("2024-10-02")
        );
    }
    assert!(stop(stops, 50).get("exit").unwrap().is_none());
}
