//! `rungbot-exec pairs`: is every routed pair still trading on its venue?
//!
//! Read-only and keyless: every venue serves pair status unsigned. A venue that delists a coin first stops quoting it or announces a date, and
//! the ladder only notices once its orders start failing. This asks each routed venue
//! for the pair's trading status and any delisting it announced. Exit 1 when a pair is
//! no longer listed, does not trade, or has a delisting date, so a timer can alert.
//!
//! It also compares the host clock with each routed venue's unsigned server time. A
//! signed request carries the host's timestamp, and the venue refuses it outside its
//! window, so a drifting clock fails every order with a timestamp error; exit 1 when the
//! offset is outside the window, a warning past half of it.

use rungbot_core::time::utc_day;
use serde_json::{json, Value};

use crate::health::Level;
use crate::run::config::RunConfig;
use crate::venue::Venue;

/// What a venue says about one pair.
#[derive(Debug, Clone, PartialEq)]
pub struct PairStatus {
    /// The venue knows the pair at all.
    pub listed: bool,
    /// `Some(false)` when the venue says it does not trade; `None` when it does not say.
    pub tradable: Option<bool>,
    /// The venue's own word for it (`tradable`, `TRADING`, `BREAK`, ...).
    pub state: String,
    /// An announced delisting, epoch seconds.
    pub delisting_ts: Option<f64>,
}

impl PairStatus {
    pub fn unlisted(state: impl Into<String>) -> PairStatus {
        PairStatus {
            listed: false,
            tradable: Some(false),
            state: state.into(),
            delisting_ts: None,
        }
    }
}

/// Gate's `GET /spot/currency_pairs/{pair}`: `trade_status` and `delisting_time`. An
/// unknown pair is a 400 `INVALID_CURRENCY_PAIR`; a delisted coin is a 400
/// `INVALID_CURRENCY` whose message says so.
pub fn gate_status(status: Option<u16>, body: &Value) -> Result<PairStatus, String> {
    match status {
        Some(200) => {
            let state = body
                .get("trade_status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let delisting_ts = body
                .get("delisting_time")
                .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
                .filter(|t| *t > 0.0);
            Ok(PairStatus {
                listed: true,
                tradable: (!state.is_empty()).then(|| state == "tradable"),
                state,
                delisting_ts,
            })
        }
        Some(400) | Some(404)
            if status == Some(404)
                || matches!(
                    body.get("label").and_then(Value::as_str),
                    Some("INVALID_CURRENCY_PAIR" | "INVALID_CURRENCY")
                ) =>
        {
            let said = body.get("message").and_then(Value::as_str).unwrap_or("");
            Ok(PairStatus::unlisted(if said.contains("delisted") {
                "delisted"
            } else {
                "not listed"
            }))
        }
        _ => Err(format!(
            "currency_pairs -> {} {body}",
            status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "None".into())
        )),
    }
}

/// Binance's `GET /api/v3/exchangeInfo?symbol=`: the symbol's `status`. An unknown symbol
/// is a 400 with code -1121.
pub fn binance_status(status: Option<u16>, body: &Value) -> Result<PairStatus, String> {
    match status {
        Some(200) => {
            let state = body
                .get("symbols")
                .and_then(|s| s.get(0))
                .and_then(|s| s.get("status"))
                .and_then(Value::as_str)
                .ok_or("exchangeInfo: no symbol status")?
                .to_string();
            Ok(PairStatus {
                listed: true,
                tradable: Some(state == "TRADING"),
                state,
                delisting_ts: None,
            })
        }
        Some(400) if body.get("code").and_then(Value::as_i64) == Some(-1121) => {
            Ok(PairStatus::unlisted("not listed"))
        }
        _ => Err(format!(
            "exchangeInfo -> {} {body}",
            status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "None".into())
        )),
    }
}

/// One routed coin's verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub sym: String,
    pub exch: String,
    pub pair: String,
    pub level: Level,
    pub message: String,
}

/// What `status` (or the read error) means for the ladder.
pub fn verdict(read: Result<PairStatus, String>) -> (Level, String) {
    match read {
        Err(e) => (Level::Warn, format!("could not read: {e}")),
        Ok(p) if !p.listed => (Level::Fail, p.state),
        Ok(p) => {
            if let Some(t) = p.delisting_ts {
                return (
                    Level::Fail,
                    format!("{}, delisting announced for {}", p.state, utc_day(t)),
                );
            }
            match p.tradable {
                Some(false) => (Level::Fail, format!("{}: not trading", p.state)),
                Some(true) => (Level::Ok, p.state),
                None if p.state.is_empty() => (Level::Ok, "listed".into()),
                None => (Level::Ok, format!("listed ({})", p.state)),
            }
        }
    }
}

/// Every routed coin, in the config's order. `venue` gives the client for a venue name.
pub fn check<'a>(
    cfg: &RunConfig,
    venue: impl Fn(&str) -> Result<&'a dyn Venue, String>,
) -> Vec<Row> {
    cfg.routing
        .iter()
        .map(|(sym, r)| {
            let read =
                venue(&r.exch).and_then(|v| v.pair_status(&r.pair).map_err(|e| e.to_string()));
            let (level, message) = verdict(read);
            Row {
                sym: sym.clone(),
                exch: r.exch.clone(),
                pair: r.pair.clone(),
                level,
                message,
            }
        })
        .collect()
}

/// One routed venue's clock verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct ClockRow {
    pub exch: String,
    pub level: Level,
    pub message: String,
    /// Host clock minus venue clock, seconds; `None` when it could not be read.
    pub offset_s: Option<f64>,
}

/// How far, in seconds, the host clock may run `(ahead, behind)` the venue's before the
/// venue refuses a signed request. Binance accepts a timestamp below server time plus
/// 1 s and no older than the 5 s `recvWindow` the client signs; Gate accepts 60 s
/// either way. `None` for a venue without a known window.
pub fn clock_window(exch: &str) -> Option<(f64, f64)> {
    match exch {
        "binance" => Some((1.0, crate::binance::RECV_WINDOW_MS as f64 / 1000.0)),
        "gate" => Some((60.0, 60.0)),
        _ => None,
    }
}

/// The verdict on `offset_s` (host minus venue) measured to within `uncertainty_s`
/// (half the round trip), against `window` from [`clock_window`]. Fail only when the
/// offset is outside the window whatever the uncertainty.
pub fn clock_verdict(window: (f64, f64), offset_s: f64, uncertainty_s: f64) -> (Level, String) {
    let (limit, way) = if offset_s >= 0.0 {
        (window.0, "ahead of")
    } else {
        (window.1, "behind")
    };
    let off = offset_s.abs();
    let said = format!(
        "host clock {off:.2} s {way} the venue (±{uncertainty_s:.2} s), accepted up to {limit} s"
    );
    if off - uncertainty_s > limit {
        (
            Level::Fail,
            format!("{said}: signed requests are refused, sync the host clock"),
        )
    } else if off + uncertainty_s > limit / 2.0 {
        (Level::Warn, format!("{said}: close to the limit"))
    } else {
        (Level::Ok, said)
    }
}

/// Every routed venue with a known window, once, in the config's order. `now` is the
/// host clock in epoch seconds, read before and after each venue's time request.
pub fn clock_check<'a>(
    cfg: &RunConfig,
    venue: impl Fn(&str) -> Result<&'a dyn Venue, String>,
    now: impl Fn() -> f64,
) -> Vec<ClockRow> {
    let mut seen: Vec<&str> = Vec::new();
    let mut rows = Vec::new();
    for (_, r) in &cfg.routing {
        let exch = r.exch.as_str();
        if seen.contains(&exch) {
            continue;
        }
        seen.push(exch);
        let Some(window) = clock_window(exch) else {
            continue;
        };
        let v = match venue(exch) {
            Ok(v) => v,
            Err(e) => {
                rows.push(unread(exch, e));
                continue;
            }
        };
        let sent = now();
        let Some(read) = v.server_time() else {
            continue;
        };
        let received = now();
        rows.push(match read {
            Ok(server) => {
                let offset = (sent + received) / 2.0 - server;
                let (level, message) = clock_verdict(window, offset, (received - sent) / 2.0);
                ClockRow {
                    exch: exch.into(),
                    level,
                    message,
                    offset_s: Some(offset),
                }
            }
            Err(e) => unread(exch, e.to_string()),
        });
    }
    rows
}

fn unread(exch: &str, e: String) -> ClockRow {
    ClockRow {
        exch: exch.into(),
        level: Level::Warn,
        message: format!("could not read the venue clock: {e}"),
        offset_s: None,
    }
}

/// No pair and no venue clock failed: what `ok` says and the exit code follows.
pub fn all_ok(rows: &[Row], clock: &[ClockRow]) -> bool {
    rows.iter().all(|r| r.level != Level::Fail) && clock.iter().all(|c| c.level != Level::Fail)
}

/// [`to_json`] with the clock rows under `clock`; `ok` is [`all_ok`].
pub fn report_json(rows: &[Row], clock: &[ClockRow]) -> Value {
    let mut out = to_json(rows);
    out["ok"] = json!(all_ok(rows, clock));
    out["clock"] = clock
        .iter()
        .map(|c| {
            json!({
                "venue": c.exch, "level": c.level.as_str(),
                "message": c.message, "offset_s": c.offset_s,
            })
        })
        .collect();
    out
}

pub fn to_json(rows: &[Row]) -> Value {
    json!({
        "ok": rows.iter().all(|r| r.level != Level::Fail),
        "pairs": rows
            .iter()
            .map(|r| json!({
                "sym": r.sym, "venue": r.exch, "pair": r.pair,
                "level": r.level.as_str(), "message": r.message,
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_reads_trade_status_and_an_announced_delisting() {
        let ok = gate_status(
            Some(200),
            &json!({"id": "AAA_USDT", "trade_status": "tradable"}),
        );
        assert_eq!(verdict(ok), (Level::Ok, "tradable".into()));

        let sell_only = gate_status(Some(200), &json!({"trade_status": "sellable"}));
        assert_eq!(
            verdict(sell_only),
            (Level::Fail, "sellable: not trading".into())
        );

        let dated = gate_status(
            Some(200),
            &json!({"trade_status": "tradable", "delisting_time": 1_700_006_400}),
        );
        assert_eq!(
            verdict(dated),
            (
                Level::Fail,
                "tradable, delisting announced for 2023-11-15".into()
            )
        );

        let zero = gate_status(
            Some(200),
            &json!({"trade_status": "tradable", "delisting_time": 0}),
        );
        assert_eq!(zero.unwrap().delisting_ts, None);
    }

    #[test]
    fn an_unknown_pair_is_unlisted_and_other_errors_stay_errors() {
        let gone = gate_status(
            Some(400),
            &json!({"label": "INVALID_CURRENCY_PAIR", "message": "x"}),
        );
        assert_eq!(verdict(gone), (Level::Fail, "not listed".into()));
        let delisted = gate_status(
            Some(400),
            &json!({"label": "INVALID_CURRENCY", "message": "currency AAA is delisted"}),
        );
        assert_eq!(verdict(delisted), (Level::Fail, "delisted".into()));
        assert!(gate_status(Some(400), &json!({"label": "OTHER"})).is_err());
        assert!(gate_status(None, &Value::Null).is_err());

        let gone = binance_status(Some(400), &json!({"code": -1121, "msg": "Invalid symbol."}));
        assert_eq!(verdict(gone), (Level::Fail, "not listed".into()));
        assert!(binance_status(Some(429), &json!({"code": -1003})).is_err());
    }

    #[test]
    fn binance_trades_only_in_trading() {
        let body = |s: &str| json!({"symbols": [{"symbol": "AAAUSDT", "status": s}]});
        assert_eq!(
            verdict(binance_status(Some(200), &body("TRADING"))),
            (Level::Ok, "TRADING".into())
        );
        assert_eq!(
            verdict(binance_status(Some(200), &body("BREAK"))),
            (Level::Fail, "BREAK: not trading".into())
        );
    }

    #[test]
    fn a_read_error_warns_and_an_unreported_status_is_listed() {
        assert_eq!(verdict(Err("timeout".into())).0, Level::Warn);
        let quiet = PairStatus {
            listed: true,
            tradable: None,
            state: String::new(),
            delisting_ts: None,
        };
        assert_eq!(verdict(Ok(quiet)), (Level::Ok, "listed".into()));
    }

    #[test]
    fn json_is_ok_only_without_a_failure() {
        let row = |level| Row {
            sym: "AAA".into(),
            exch: "gate".into(),
            pair: "AAA_USDT".into(),
            level,
            message: String::new(),
        };
        assert_eq!(
            to_json(&[row(Level::Ok), row(Level::Warn)])["ok"],
            json!(true)
        );
        assert_eq!(to_json(&[row(Level::Fail)])["ok"], json!(false));
    }
}
