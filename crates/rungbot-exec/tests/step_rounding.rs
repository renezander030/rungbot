//! Amounts and prices are rounded down to the venue's step before an order is sent.
//!
//! A value that already sits on the step (0.3 with a 0.1 step, 1.15 at two decimals)
//! must keep it: binary floating point makes `0.3 / 0.1` read 2.9999…, and a plain
//! floor would then drop one whole step or tick. Values between steps still round down,
//! never up.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use rungbot_exec::http::{Http, Request, Response, SendError, Transport};
use rungbot_exec::keys::Credentials;
use rungbot_exec::venue::Venue;
use rungbot_exec::{Binance, PairInfo};

struct Scripted(RefCell<VecDeque<Result<Response, SendError>>>);

impl Transport for Scripted {
    fn send(&self, _req: &Request) -> Result<Response, SendError> {
        self.0
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(SendError::Network("unscripted request".into())))
    }
}

/// A Binance client whose one exchangeInfo reply gives AAAUSDT a 0.1 lot step and a
/// 0.01 price tick (cached for every later call).
fn binance() -> Binance {
    let info = r#"{"symbols":[{"symbol":"AAAUSDT","filters":[
        {"filterType":"PRICE_FILTER","tickSize":"0.01"},
        {"filterType":"LOT_SIZE","stepSize":"0.1","minQty":"0.1"},
        {"filterType":"NOTIONAL","minNotional":"5"}]}]}"#;
    let t = Rc::new(Scripted(RefCell::new(
        vec![Ok(Response {
            status: 200,
            body: info.into(),
        })]
        .into(),
    )));
    let creds = Credentials {
        key: String::new(),
        secret: String::new(),
    };
    Binance::with_http(creds, Http::new(t, Rc::new(|| 0.0)))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}

#[test]
fn binance_keeps_an_amount_or_price_that_sits_on_the_step() {
    let b = binance();
    for x in [0.3, 0.7, 2.3, 4.1] {
        let r = b.round_amount("AAAUSDT", x).unwrap();
        assert!(close(r, x), "amount {x} rounded to {r}");
    }
    for p in [4.35, 0.29, 1.15, 0.07] {
        let r = b.round_price("AAAUSDT", p).unwrap();
        assert!(close(r, p), "price {p} rounded to {r}");
    }
}

#[test]
fn binance_still_rounds_down_between_steps() {
    let b = binance();
    assert!(close(b.round_amount("AAAUSDT", 0.39).unwrap(), 0.3));
    assert!(close(b.round_amount("AAAUSDT", 0.2999).unwrap(), 0.2));
    assert!(close(b.round_price("AAAUSDT", 4.359).unwrap(), 4.35));
    assert!(close(b.round_price("AAAUSDT", 4.3499).unwrap(), 4.34));
}

#[test]
fn gate_keeps_an_amount_or_price_that_sits_on_its_precision() {
    let p = PairInfo {
        amount_precision: 2,
        price_precision: 3,
        min_base: 0.0,
        min_quote: 0.0,
    };
    for x in [1.15, 0.29, 4.35, 8.2] {
        assert!(
            close(p.round_amount(x), x),
            "amount {x} -> {}",
            p.round_amount(x)
        );
    }
    for x in [1.005, 0.291, 2.675] {
        assert!(
            close(p.round_price(x), x),
            "price {x} -> {}",
            p.round_price(x)
        );
    }
    assert!(close(p.round_amount(1.159), 1.15));
    assert!(close(p.round_amount(0.2899), 0.28));
    assert!(close(p.round_price(1.0049), 1.004));
    // A large count between steps rounds down too.
    let wide = PairInfo {
        price_precision: 5,
        ..p
    };
    assert!(close(wide.round_price(123456.123456), 123456.12345));
}
