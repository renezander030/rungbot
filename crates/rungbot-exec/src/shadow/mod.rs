//! The shadow run: the whole live cycle against a copy of the state, with every venue
//! write simulated and none sent.
//!
//! [`ShadowVenues`] wraps the real venue clients. Reads (balances, open orders, order
//! status, prices, pair rules, deposits) go to the venue as usual, so the run sees the
//! book it would trade against. Writes never leave the process: a market order is
//! answered as filled at the venue's current price, a limit order as resting, a cancel
//! as done, each with a `shadow-N` order id. Every one is recorded as an [`Intent`].
//! Later reads in the same run see the simulated book: a cancelled order reads as
//! cancelled and a simulated resting order is listed on its pair.
//!
//! Housekeeping (paired sells, retries, reprices, cover restores), the ladder's market
//! orders and the deploy layer (tranches, rolls, sweeps, the onramp) all reach the venue
//! through [`crate::reconcile::VenueSource`], so wrapping it covers each of them.
//!
//! [`diff`] compares a shadow run's decision log with the reference implementation's.

pub mod diff;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use serde_json::{json, Value};

use crate::clients::VENUES;
use crate::http::VenueError;
use crate::journal::is_open_status;
use crate::reconcile::VenueSource;
use crate::venue::{Balance, Limits, ParsedOrder, Venue};

/// The key a simulated venue answer carries, so [`Venue::parse_order`] hands it back as
/// it was built instead of running the venue's own parser on it.
const SIM_KEY: &str = "__shadow__";

/// One write the run would have sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Intent {
    pub venue: String,
    /// `market_buy`, `market_sell`, `limit_buy`, `limit_sell` or `cancel`.
    pub call: String,
    pub pair: String,
    /// Quote for a market buy, base for the others; 0 for a cancel.
    pub amount: f64,
    pub price: Option<f64>,
    pub client_id: Option<String>,
    /// The simulated order id, or the cancelled one.
    pub order_id: String,
}

impl Intent {
    pub fn line(&self) -> String {
        let mut s = format!("SHADOW {} {} {}", self.venue, self.call, self.pair);
        if self.call != "cancel" {
            s.push_str(&format!(" amount={}", crate::pyfmt::g(self.amount, 8)));
        }
        if let Some(p) = self.price {
            s.push_str(&format!(" price={}", crate::pyfmt::g(p, 8)));
        }
        if let Some(c) = &self.client_id {
            s.push_str(&format!(" cid={c}"));
        }
        s.push_str(&format!(" id={}", self.order_id));
        s
    }

    pub fn to_json(&self) -> Value {
        json!({"venue": self.venue, "call": self.call, "pair": self.pair,
               "amount": self.amount, "price": self.price,
               "client_id": self.client_id, "order_id": self.order_id})
    }
}

#[derive(Default)]
struct Book {
    seq: Cell<u64>,
    intents: RefCell<Vec<Intent>>,
    /// Simulated orders by id, with their pair.
    sim: RefCell<BTreeMap<String, (String, ParsedOrder)>>,
    /// Real order ids the run cancelled, per venue.
    cancelled: RefCell<BTreeSet<(String, String)>>,
}

/// The venue set of a shadow run. See the module docs.
pub struct ShadowVenues<'a> {
    venues: Vec<ShadowVenue<'a>>,
    inner: &'a dyn VenueSource,
    book: Rc<Book>,
}

impl<'a> ShadowVenues<'a> {
    pub fn new(inner: &'a dyn VenueSource) -> ShadowVenues<'a> {
        let book = Rc::new(Book::default());
        ShadowVenues {
            venues: VENUES
                .iter()
                .map(|exch| ShadowVenue {
                    exch,
                    inner,
                    book: book.clone(),
                })
                .collect(),
            inner,
            book,
        }
    }

    /// Every write the run would have sent, in order.
    pub fn intents(&self) -> Vec<Intent> {
        self.book.intents.borrow().clone()
    }
}

impl VenueSource for ShadowVenues<'_> {
    fn venue(&self, exch: &str) -> Result<&dyn Venue, String> {
        // The real client first: a venue without keys fails here as it would live.
        self.inner.venue(exch)?;
        self.venues
            .iter()
            .find(|v| v.exch == exch)
            .map(|v| v as &dyn Venue)
            .ok_or_else(|| format!("'{exch}'"))
    }
}

struct ShadowVenue<'a> {
    exch: &'static str,
    inner: &'a dyn VenueSource,
    book: Rc<Book>,
}

/// A venue's word for a resting order, as its own parser reports it.
fn open_word(exch: &str) -> &'static str {
    match exch {
        "revx" => "new",
        "binance" => "NEW",
        _ => "open",
    }
}

impl ShadowVenue<'_> {
    fn real(&self) -> Result<&dyn Venue, VenueError> {
        self.inner
            .venue(self.exch)
            .map_err(|e| VenueError::new(self.exch, None, e))
    }

    fn next_id(&self) -> String {
        let n = self.book.seq.get() + 1;
        self.book.seq.set(n);
        format!("shadow-{n}")
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        call: &str,
        pair: &str,
        amount: f64,
        price: Option<f64>,
        cid: Option<&str>,
        order: ParsedOrder,
    ) -> Value {
        self.book.intents.borrow_mut().push(Intent {
            venue: self.exch.to_string(),
            call: call.to_string(),
            pair: pair.to_string(),
            amount,
            price,
            client_id: cid.map(str::to_string),
            order_id: order.order_id.clone(),
        });
        self.book
            .sim
            .borrow_mut()
            .insert(order.order_id.clone(), (pair.to_string(), order.clone()));
        json!({ SIM_KEY: serde_json::to_value(&order).expect("an order serialises") })
    }

    /// A market order, filled at the venue's price now. Without a price it rests
    /// unfilled, and the run treats it as awaiting its fill.
    fn market(&self, side: &str, pair: &str, amount: f64, cid: Option<&str>) -> Value {
        let px = self.real().and_then(|v| v.price(pair)).ok();
        let id = self.next_id();
        let mut o = ParsedOrder {
            order_id: id,
            side: side.into(),
            client_id: cid.unwrap_or_default().into(),
            ..Default::default()
        };
        match px.filter(|p| *p > 0.0 && p.is_finite()) {
            Some(p) => {
                let (base, quote) = if side == "buy" {
                    (amount / p, amount)
                } else {
                    (amount, amount * p)
                };
                o.status = "filled".into();
                o.filled = true;
                o.base_qty = Some(base);
                o.qty = base;
                o.quote = quote;
                o.avg_price = Some(p);
                o.price = Some(p);
            }
            None => o.status = open_word(self.exch).into(),
        }
        self.record(&format!("market_{side}"), pair, amount, px, cid, o)
    }

    fn limit(&self, side: &str, pair: &str, base: f64, price: f64, cid: Option<&str>) -> Value {
        let o = ParsedOrder {
            order_id: self.next_id(),
            status: open_word(self.exch).into(),
            side: side.into(),
            qty: base,
            price: Some(price),
            client_id: cid.unwrap_or_default().into(),
            ..Default::default()
        };
        self.record(&format!("limit_{side}"), pair, base, Some(price), cid, o)
    }

    fn is_cancelled(&self, id: &str) -> bool {
        self.book
            .cancelled
            .borrow()
            .contains(&(self.exch.to_string(), id.to_string()))
    }
}

impl Venue for ShadowVenue<'_> {
    fn name(&self) -> &'static str {
        self.real().map(|v| v.name()).unwrap_or(self.exch)
    }
    fn parse_order(&self, raw: &Value) -> Result<ParsedOrder, VenueError> {
        if let Some(o) = raw.get(SIM_KEY) {
            return serde_json::from_value(o.clone())
                .map_err(|e| VenueError::new(self.exch, None, e.to_string()));
        }
        self.real()?.parse_order(raw)
    }
    fn order_status(&self, pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        if let Some((_, o)) = self.book.sim.borrow().get(order_id) {
            return Ok(o.clone());
        }
        let mut st = self.real()?.order_status(pair, order_id)?;
        if self.is_cancelled(order_id) && !st.filled && is_open_status(&st.status) {
            st.status = "cancelled".into();
        }
        Ok(st)
    }
    fn open_orders(&self, pair: &str) -> Result<Vec<ParsedOrder>, VenueError> {
        let mut rows: Vec<ParsedOrder> = self
            .real()?
            .open_orders(pair)?
            .into_iter()
            .filter(|o| !self.is_cancelled(&o.order_id))
            .collect();
        for (p, o) in self.book.sim.borrow().values() {
            if p == pair && !o.filled && is_open_status(&o.status) {
                rows.push(o.clone());
            }
        }
        Ok(rows)
    }
    fn cancel(&self, pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        self.book.intents.borrow_mut().push(Intent {
            venue: self.exch.to_string(),
            call: "cancel".into(),
            pair: pair.to_string(),
            amount: 0.0,
            price: None,
            client_id: None,
            order_id: order_id.to_string(),
        });
        let mut sim = self.book.sim.borrow_mut();
        if let Some((_, o)) = sim.get_mut(order_id) {
            o.status = "cancelled".into();
        } else {
            self.book
                .cancelled
                .borrow_mut()
                .insert((self.exch.to_string(), order_id.to_string()));
        }
        Ok(ParsedOrder {
            order_id: order_id.to_string(),
            status: "cancelled".into(),
            ..Default::default()
        })
    }
    fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError> {
        self.real()?.balances()
    }
    fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError> {
        self.real()?.balances_full()
    }
    fn price(&self, pair: &str) -> Result<f64, VenueError> {
        self.real()?.price(pair)
    }
    fn limits(&self, pair: &str) -> Result<Limits, VenueError> {
        self.real()?.limits(pair)
    }
    fn round_amount(&self, pair: &str, amount: f64) -> Result<f64, VenueError> {
        self.real()?.round_amount(pair, amount)
    }
    fn round_price(&self, pair: &str, price: f64) -> Result<f64, VenueError> {
        self.real()?.round_price(pair, price)
    }
    fn market_buy(&self, pair: &str, quote: f64, cid: Option<&str>) -> Result<Value, VenueError> {
        Ok(self.market("buy", pair, quote, cid))
    }
    fn market_sell(&self, pair: &str, base: f64, cid: Option<&str>) -> Result<Value, VenueError> {
        Ok(self.market("sell", pair, base, cid))
    }
    fn limit_buy(
        &self,
        pair: &str,
        base: f64,
        price: f64,
        cid: Option<&str>,
    ) -> Result<Value, VenueError> {
        Ok(self.limit("buy", pair, base, price, cid))
    }
    fn limit_sell(
        &self,
        pair: &str,
        base: f64,
        price: f64,
        cid: Option<&str>,
    ) -> Result<Value, VenueError> {
        Ok(self.limit("sell", pair, base, price, cid))
    }
    fn qty_step(&self, pair: &str) -> Result<f64, VenueError> {
        self.real()?.qty_step(pair)
    }
    fn deposits(&self, since: f64) -> Option<Result<Vec<crate::gate::Deposit>, VenueError>> {
        match self.real() {
            Ok(v) => v.deposits(since),
            Err(e) => Some(Err(e)),
        }
    }
    fn id_matches(&self, venue_cid: &str, cid: &str) -> bool {
        match self.real() {
            Ok(v) => v.id_matches(venue_cid, cid),
            Err(_) => crate::venue::venue_id_matches(self.exch, venue_cid, cid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A venue that answers reads and panics on any write.
    struct ReadOnly;

    impl Venue for ReadOnly {
        fn name(&self) -> &'static str {
            "gate"
        }
        fn parse_order(&self, _: &Value) -> Result<ParsedOrder, VenueError> {
            panic!("the shadow must parse its own answers")
        }
        fn order_status(&self, _: &str, id: &str) -> Result<ParsedOrder, VenueError> {
            Ok(ParsedOrder {
                order_id: id.into(),
                status: "open".into(),
                ..Default::default()
            })
        }
        fn open_orders(&self, _: &str) -> Result<Vec<ParsedOrder>, VenueError> {
            Ok(vec![ParsedOrder {
                order_id: "real-1".into(),
                status: "open".into(),
                ..Default::default()
            }])
        }
        fn cancel(&self, _: &str, _: &str) -> Result<ParsedOrder, VenueError> {
            panic!("write: cancel")
        }
        fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError> {
            Ok(BTreeMap::new())
        }
        fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError> {
            Ok(BTreeMap::new())
        }
        fn price(&self, _: &str) -> Result<f64, VenueError> {
            Ok(2.0)
        }
        fn limits(&self, _: &str) -> Result<Limits, VenueError> {
            Ok(Limits::default())
        }
        fn round_amount(&self, _: &str, a: f64) -> Result<f64, VenueError> {
            Ok(a)
        }
        fn round_price(&self, _: &str, p: f64) -> Result<f64, VenueError> {
            Ok(p)
        }
        fn market_buy(&self, _: &str, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
            panic!("write: market_buy")
        }
        fn market_sell(&self, _: &str, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
            panic!("write: market_sell")
        }
        fn limit_buy(&self, _: &str, _: f64, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
            panic!("write: limit_buy")
        }
        fn limit_sell(
            &self,
            _: &str,
            _: f64,
            _: f64,
            _: Option<&str>,
        ) -> Result<Value, VenueError> {
            panic!("write: limit_sell")
        }
    }

    struct One(ReadOnly);

    impl VenueSource for One {
        fn venue(&self, exch: &str) -> Result<&dyn Venue, String> {
            if exch == "gate" {
                Ok(&self.0)
            } else {
                Err(format!("no keys for {exch}"))
            }
        }
    }

    #[test]
    fn writes_are_simulated_and_recorded_reads_see_them() {
        let src = One(ReadOnly);
        let sh = ShadowVenues::new(&src);
        let v = sh.venue("gate").unwrap();
        let raw = v.market_buy("AAA_USDT", 10.0, Some("c1")).unwrap();
        let o = v.parse_order(&raw).unwrap();
        assert!(o.filled);
        assert_eq!(o.base_qty, Some(5.0));
        assert_eq!(o.avg_price, Some(2.0));
        let raw = v.limit_sell("AAA_USDT", 5.0, 3.0, Some("c2")).unwrap();
        let l = v.parse_order(&raw).unwrap();
        assert_eq!(l.status, "open");
        assert!(!l.filled);
        v.cancel("AAA_USDT", "real-1").unwrap();
        let open: Vec<String> = v
            .open_orders("AAA_USDT")
            .unwrap()
            .into_iter()
            .map(|o| o.order_id)
            .collect();
        assert_eq!(open, vec![l.order_id.clone()]);
        assert_eq!(
            v.order_status("AAA_USDT", "real-1").unwrap().status,
            "cancelled"
        );
        assert_eq!(
            v.order_status("AAA_USDT", &l.order_id).unwrap().status,
            "open"
        );
        let calls: Vec<String> = sh.intents().iter().map(|i| i.call.clone()).collect();
        assert_eq!(calls, ["market_buy", "limit_sell", "cancel"]);
        assert!(sh.intents()[0]
            .line()
            .starts_with("SHADOW gate market_buy AAA_USDT"));
    }

    #[test]
    fn a_venue_without_keys_fails_as_it_would_live() {
        let src = One(ReadOnly);
        let sh = ShadowVenues::new(&src);
        assert_eq!(sh.venue("revx").err().unwrap(), "no keys for revx");
    }
}
