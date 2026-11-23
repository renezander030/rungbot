//! `pairs` compares the host clock with each routed venue's unsigned server time.
//!
//! Real Binance and Gate clients over a scripted transport and a stepping clock: what
//! the time requests ask, how the offset is measured across the round trip, and which
//! offsets warn or fail against each venue's signed-request window.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use rungbot_exec::health::Level;
use rungbot_exec::http::{Http, Request, Response, SendError, Transport};
use rungbot_exec::keys::Credentials;
use rungbot_exec::pairs::{self, clock_verdict, clock_window, ClockRow};
use rungbot_exec::run::config::RunConfig;
use rungbot_exec::venue::Venue;
use rungbot_exec::{Binance, Gate};

struct Scripted {
    queue: RefCell<VecDeque<Result<Response, SendError>>>,
    seen: RefCell<Vec<String>>,
}

impl Scripted {
    fn new(replies: Vec<Result<Response, SendError>>) -> Rc<Scripted> {
        Rc::new(Scripted {
            queue: RefCell::new(replies.into()),
            seen: RefCell::new(Vec::new()),
        })
    }
}

impl Transport for Scripted {
    fn send(&self, req: &Request) -> Result<Response, SendError> {
        self.seen
            .borrow_mut()
            .push(format!("{} {}", req.method, req.url));
        self.queue
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(SendError::Network("unscripted request".into())))
    }
}

fn ok(body: &str) -> Result<Response, SendError> {
    Ok(Response {
        status: 200,
        body: body.into(),
    })
}

fn creds() -> Credentials {
    Credentials {
        key: String::new(),
        secret: String::new(),
    }
}

fn http(t: &Rc<Scripted>) -> Http {
    Http::new(t.clone(), Rc::new(|| 0.0))
}

/// A host clock that reads `start`, then advances `step` seconds per read.
fn stepping(start: f64, step: f64) -> impl Fn() -> f64 {
    let t = Cell::new(start - step);
    move || {
        t.set(t.get() + step);
        t.get()
    }
}

fn config(routing: &str) -> RunConfig {
    let watch: String = routing
        .lines()
        .filter_map(|l| l.trim().split(':').next())
        .map(|sym| format!("  {sym}: {}\n", sym.to_lowercase()))
        .collect();
    let yaml = format!("watchlist:\n{watch}routing:\n{routing}");
    RunConfig::from_yaml(&yaml, &|_| None).expect("neutral config")
}

#[test]
fn each_venue_serves_its_clock_from_the_unsigned_time_endpoint() {
    let t = Scripted::new(vec![ok(r#"{"serverTime": 1700000000250}"#)]);
    let b = Binance::with_http(creds(), http(&t));
    assert_eq!(b.server_time().unwrap().unwrap(), 1_700_000_000.25);
    assert_eq!(
        t.seen.borrow().as_slice(),
        ["GET https://api.binance.com/api/v3/time"]
    );

    let t = Scripted::new(vec![ok(r#"{"server_time": 1700000000500}"#)]);
    let g = Gate::with_http(creds(), http(&t));
    assert_eq!(g.server_time().unwrap().unwrap(), 1_700_000_000.5);
    assert_eq!(
        t.seen.borrow().as_slice(),
        ["GET https://api.gateio.ws/api/v4/spot/time"]
    );

    let t = Scripted::new(vec![ok(r#"{"unexpected": 1}"#)]);
    let g = Gate::with_http(creds(), http(&t));
    let e = g.server_time().unwrap().unwrap_err().to_string();
    assert!(e.contains("server_time"), "{e}");
}

#[test]
fn the_windows_follow_each_venues_signed_request_rules() {
    assert_eq!(clock_window("binance"), Some((1.0, 5.0)));
    assert_eq!(clock_window("gate"), Some((60.0, 60.0)));
    assert_eq!(clock_window("revx"), None);

    let binance = clock_window("binance").unwrap();
    assert_eq!(clock_verdict(binance, 0.2, 0.05).0, Level::Ok);
    assert_eq!(clock_verdict(binance, 0.6, 0.05).0, Level::Warn);
    assert_eq!(clock_verdict(binance, 1.5, 0.05).0, Level::Fail);
    // Behind is allowed up to the 5 s recvWindow.
    assert_eq!(clock_verdict(binance, -1.5, 0.05).0, Level::Ok);
    assert_eq!(clock_verdict(binance, -3.0, 0.05).0, Level::Warn);
    assert_eq!(clock_verdict(binance, -5.5, 0.05).0, Level::Fail);
    // Fail only when outside the window whatever the round-trip uncertainty.
    assert_eq!(clock_verdict(binance, 1.2, 0.3).0, Level::Warn);

    let (level, message) = clock_verdict(clock_window("gate").unwrap(), -75.0, 0.1);
    assert_eq!(level, Level::Fail);
    assert_eq!(
        message,
        "host clock 75.00 s behind the venue (±0.10 s), accepted up to 60 s: \
         signed requests are refused, sync the host clock"
    );
}

#[test]
fn the_offset_is_measured_at_the_round_trips_midpoint_once_per_venue() {
    let cfg = config(
        "  AAA: binance AAAUSDC USDC\n  BBB: gate BBB_USDT USDT\n  CCC: binance CCCUSDC USDC\n",
    );
    // Binance's clock is 2 s behind the host's midpoint 1000.1 (reads 1000.0, 1000.2);
    // Gate's matches the next midpoint 1000.5 (reads 1000.4, 1000.6).
    let bt = Scripted::new(vec![ok(r#"{"serverTime": 998100}"#)]);
    let gt = Scripted::new(vec![ok(r#"{"server_time": 1000500}"#)]);
    let b = Binance::with_http(creds(), http(&bt));
    let g = Gate::with_http(creds(), http(&gt));
    let venue = |name: &str| -> Result<&dyn Venue, String> {
        match name {
            "binance" => Ok(&b),
            "gate" => Ok(&g),
            other => Err(format!("unknown venue {other}")),
        }
    };
    let rows = pairs::clock_check(&cfg, venue, stepping(1000.0, 0.2));
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(bt.seen.borrow().len(), 1, "one time request per venue");
    assert_eq!(rows[0].exch, "binance");
    assert!((rows[0].offset_s.unwrap() - 2.0).abs() < 1e-9);
    assert_eq!(rows[0].level, Level::Fail);
    assert!(rows[0].message.contains("±0.10 s"), "{}", rows[0].message);
    assert_eq!(rows[1].exch, "gate");
    assert!(rows[1].offset_s.unwrap().abs() < 1e-9);
    assert_eq!(rows[1].level, Level::Ok);
}

#[test]
fn an_unread_clock_warns_and_a_venue_without_a_window_is_skipped() {
    let cfg = config("  AAA: gate AAA_USDT USDT\n  BBB: revx BBB-USD USD\n");
    let gt = Scripted::new(vec![Err(SendError::Network("timed out".into()))]);
    let g = Gate::with_http(creds(), http(&gt));
    let venue = |name: &str| -> Result<&dyn Venue, String> {
        match name {
            "gate" => Ok(&g),
            other => Err(format!("no client for {other}")),
        }
    };
    let rows = pairs::clock_check(&cfg, venue, stepping(0.0, 0.1));
    assert_eq!(rows.len(), 1, "revx has no known window: {rows:?}");
    assert_eq!(rows[0].level, Level::Warn);
    assert_eq!(rows[0].offset_s, None);
    assert!(rows[0]
        .message
        .starts_with("could not read the venue clock"));
}

#[test]
fn json_carries_the_clock_and_a_clock_failure_clears_ok() {
    let row = |level| ClockRow {
        exch: "gate".into(),
        level,
        message: "m".into(),
        offset_s: Some(0.5),
    };
    let fine = pairs::report_json(&[], &[row(Level::Warn)]);
    assert_eq!(fine["ok"], serde_json::json!(true));
    assert_eq!(fine["pairs"], serde_json::json!([]));
    assert_eq!(
        fine["clock"],
        serde_json::json!([{"venue": "gate", "level": "warn", "message": "m", "offset_s": 0.5}])
    );
    assert_eq!(
        pairs::report_json(&[], &[row(Level::Fail)])["ok"],
        serde_json::json!(false)
    );
}
