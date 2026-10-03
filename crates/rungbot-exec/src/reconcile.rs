//! Polling the venues and writing what really happened into the journal.
//!
//! [`reconcile`] walks every open row with a venue id and asks its venue for the
//! order's status:
//!
//! * **Filled**: the fill is booked (`filled_base` net of a base-coin fee,
//!   `filled_quote`, `avg_price`, `filled_ts`) and the row is returned as a new fill.
//! * **Left the book part-filled** (cancelled or expired with a base amount): the part is
//!   a fill. It is booked the same way with `partial` set, the status stays the cancel,
//!   and the row is returned as a fill. Without this the part is never counted.
//! * **Still open with progress**: `part_base` / `part_quote` record it.
//! * **Cancelled by the venue** with a note that is not ours: before writing the rung
//!   off, [`adopt_replacement`] looks for the order a venue-side resize leaves behind
//!   and re-attaches the row to it.
//! * A poll that fails on the way (a timeout, a dropped connection) is asked once more
//!   in the same pass, see [`read_status`]. A failure that survives that, or a refused
//!   request, stores `last_error` and moves on; the next good poll clears it.
//!
//! Rows [`settle_cancel`] booked after one of our own cancels are handed out first.
//!
//! Pure over the journal: the caller owns loading, saving and the clock (`now`).

use std::collections::{BTreeMap, BTreeSet};

use crate::http::VenueError;
use crate::journal::{is_our_cancel, is_venue_cancelled_status, truthy_f, Journal, Order};
use crate::venue::{ParsedOrder, Venue};

/// Where reconcile finds the client for a row's `exch`.
pub trait VenueSource {
    /// The client for `exch`, or the error text to store on its rows.
    fn venue(&self, exch: &str) -> Result<&dyn Venue, String>;
}

/// A row re-attached to a venue-side replacement.
#[derive(Debug, Clone, PartialEq)]
pub struct Adopted {
    /// The row as it is after adoption.
    pub order: Order,
    /// The quote the row carried before: the size the resize replaced.
    pub was_quote: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reconciled {
    /// Rows that newly filled this pass, including the filled part of an order that left
    /// the book part-filled, in journal order after the rows settled earlier.
    pub filled: Vec<Order>,
    pub adopted: Vec<Adopted>,
    /// Did anything change? Save the journal when it did.
    pub changed: bool,
}

/// Base amount actually held from a venue status: a fee charged in the base coin
/// reduces it. A sell's fee is charged in quote and leaves it alone.
pub fn net_base(o: &Order, st: &ParsedOrder) -> f64 {
    let fb = st.base_qty.unwrap_or(0.0);
    if st.fee != 0.0 && st.fee_asset.as_deref() == Some(o.sym.as_str()) {
        (fb - st.fee).max(0.0)
    } else {
        fb
    }
}

/// An order that left the book after a partial fill: that part is a fill. The status
/// stays the cancel.
pub fn book_partial(o: &mut Order, st: &ParsedOrder, now: f64) {
    o.filled_base = Some(net_base(o, st));
    o.filled_quote = Some(st.quote);
    o.avg_price = st.avg_price;
    o.filled_ts = Some(now);
    o.partial = Some(true);
}

/// The same venue price level: not "close", the price IS the rung's identity.
pub fn price_eq(a: Option<f64>, b: Option<f64>) -> bool {
    let (a, b) = (a.unwrap_or(0.0), b.unwrap_or(0.0));
    a > 0.0 && b > 0.0 && (a - b).abs() <= 1e-9 * a.max(b)
}

type OpenCache = BTreeMap<(String, String), Vec<ParsedOrder>>;

/// A venue-side cancel is not always a dead rung. Resizing a resting order in a venue's
/// app is a cancel and a re-create: our order id dies and an unjournaled one appears on
/// the same pair, same side, same price, carrying the new size. The rung is the price;
/// the size is the owner's call. Re-attach to it.
///
/// Conservative by construction: exactly ONE unclaimed venue order at that exact price
/// on that side, and never a row that already recorded a partial fill (its cash would be
/// split across two orders). Returns the updated row, or `None`.
pub fn adopt_replacement(
    venue: &dyn Venue,
    o: &Order,
    claimed: &BTreeSet<String>,
    cache: &mut OpenCache,
    now: f64,
) -> Option<Order> {
    if truthy_f(o.part_quote) || truthy_f(o.filled_quote) {
        return None;
    }
    let side = o.side.to_lowercase();
    if side.is_empty() || !truthy_f(o.price) {
        return None;
    }
    let price = o.price.expect("checked truthy");
    let key = (o.exch.clone(), o.pair.clone());
    let resting = cache
        .entry(key)
        .or_insert_with(|| venue.open_orders(&o.pair).unwrap_or_default());
    let cands: Vec<&ParsedOrder> = resting
        .iter()
        .filter(|x| !x.order_id.is_empty() && !claimed.contains(&x.order_id))
        .filter(|x| x.side.to_lowercase() == side)
        .filter(|x| price_eq(x.price, Some(price)))
        .filter(|x| x.qty > 0.0)
        .collect();
    let [new] = cands.as_slice() else {
        return None;
    };
    let mut out = o.clone();
    out.order_id = Some(new.order_id.clone());
    out.base = Some(new.qty);
    out.quote = Some(new.qty * price);
    out.status = if new.status.is_empty() {
        "open".into()
    } else {
        new.status.clone()
    };
    out.last_error = None;
    out.adopted_ts = Some(now);
    out.manual_size = Some(true);
    Some(out)
}

/// One order's status from its venue. A read that failed on the way (a timeout, a
/// dropped connection) is asked once more before it counts: one slow reply is not an
/// outage. A refused request (any HTTP status) is final the first time.
pub fn read_status(
    venue: &dyn Venue,
    pair: &str,
    order_id: &str,
) -> Result<ParsedOrder, VenueError> {
    match venue.order_status(pair, order_id) {
        Err(e) if e.is_network_failure() => venue.order_status(pair, order_id),
        r => r,
    }
}

/// Poll every open journaled order and book what happened. See the module docs.
pub fn reconcile(j: &mut Journal, venues: &dyn VenueSource, now: f64) -> Reconciled {
    let mut out = Reconciled::default();
    let mut filled_ids: Vec<String> = Vec::new();
    for o in j.orders.values_mut() {
        if o.book_pending.take().is_some_and(|b| b) {
            filled_ids.push(o.client_id.clone());
            out.changed = true;
        }
    }
    let mut claimed: BTreeSet<String> = j
        .orders
        .values()
        .filter_map(|x| x.order_id.clone().filter(|id| !id.is_empty()))
        .collect();
    let mut cache: OpenCache = BTreeMap::new();
    let ids: Vec<String> = j.orders.keys().cloned().collect();
    for cid in ids {
        let o = j.orders.get_mut(&cid).expect("ids come from the map");
        let Some(order_id) = o.order_id.clone().filter(|id| !id.is_empty()) else {
            continue;
        };
        if !o.is_open() {
            continue;
        }
        let polled = venues.venue(&o.exch).and_then(|v| {
            read_status(v, &o.pair, &order_id)
                .map(|st| (v, st))
                .map_err(|e| e.to_string())
        });
        let (venue, st) = match polled {
            Ok(x) => x,
            Err(e) => {
                o.last_error = Some(e);
                out.changed = true;
                continue;
            }
        };
        if o.last_error.as_deref().is_some_and(|e| !e.is_empty()) {
            o.last_error = None; // a good poll clears a stale poll error
            out.changed = true;
        }
        let new_status = if st.filled {
            "filled".to_string()
        } else if !st.status.is_empty() {
            st.status.clone()
        } else {
            o.status.clone()
        };
        // A cancel that carries a fill (part-filled since the last poll) is never a resize
        // to adopt: its fill is booked below, else it is lost and later reads as drift.
        if new_status != o.status
            && is_venue_cancelled_status(&new_status)
            && st.base_qty.unwrap_or(0.0) == 0.0
            && !is_our_cancel(o.note.as_deref())
        {
            if let Some(adopted) = adopt_replacement(venue, o, &claimed, &mut cache, now) {
                let was = o.quote.unwrap_or(0.0);
                *o = adopted;
                o.status_ts = None; // the rung never died: nothing to sweep
                claimed.insert(o.order_id.clone().unwrap_or_default());
                out.changed = true;
                out.adopted.push(Adopted {
                    order: o.clone(),
                    was_quote: was,
                });
                continue;
            }
        }
        if new_status != o.status {
            o.status_ts = Some(now); // when the VENUE changed it
        }
        o.status = new_status;
        let base = st.base_qty.unwrap_or(0.0);
        if st.filled {
            // Net of a base-coin buy fee, so the fill (and any sell paired off it) is
            // what is actually held.
            o.filled_base = Some(net_base(o, &st));
            o.filled_quote = Some(st.quote);
            o.avg_price = st.avg_price;
            o.filled_ts = Some(now);
            filled_ids.push(cid.clone());
        } else if base != 0.0 && is_venue_cancelled_status(&o.status) {
            book_partial(o, &st, now); // left the book part-filled
            filled_ids.push(cid.clone());
        } else if base != 0.0 {
            // Progress on an order still resting.
            o.part_base = Some(net_base(o, &st));
            o.part_quote = Some(st.quote);
        }
        out.changed = true;
    }
    out.filled = filled_ids
        .iter()
        .filter_map(|c| j.orders.get(c).cloned())
        .collect();
    out
}

/// What [`settle_cancel`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// No such row, or it never reached the venue.
    NotPlaced,
    /// The status read failed; `settle_error` is stored on the row.
    Unreadable(String),
    /// Nothing had filled.
    NothingFilled,
    /// A fill was booked and queued for the next reconcile.
    Booked(Box<Order>),
}

/// After WE cancel an order, read its final state once. A fill that landed after the
/// last poll (or the partial it already had) is booked, and the row is queued so the
/// next reconcile hands it to the fill handlers. Never fails: an unreadable status
/// leaves the row as the cancel wrote it, plus `settle_error`.
pub fn settle_cancel(j: &mut Journal, venue: &dyn Venue, cid: &str, now: f64) -> Settled {
    let Some(o) = j.orders.get_mut(cid) else {
        return Settled::NotPlaced;
    };
    let Some(order_id) = o.order_id.clone().filter(|id| !id.is_empty()) else {
        return Settled::NotPlaced;
    };
    let st = match read_status(venue, &o.pair, &order_id) {
        Ok(st) => st,
        Err(e) => {
            o.settle_error = Some(e.to_string());
            return Settled::Unreadable(e.to_string());
        }
    };
    if st.base_qty.unwrap_or(0.0) == 0.0 {
        return Settled::NothingFilled;
    }
    book_partial(o, &st, now);
    o.book_pending = Some(true);
    Settled::Booked(Box::new(o.clone()))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};

    use serde_json::Value;

    use super::*;
    use crate::venue::{Balance, Limits};

    const TIMEOUT: &str = "GET /api/1.0/orders/o1 -> None <urlopen error the timeout of the \
                           request was reached>";

    /// A venue whose status reads answer from a script, one entry per call; writes panic.
    struct Scripted {
        answers: RefCell<VecDeque<Result<ParsedOrder, VenueError>>>,
        reads: RefCell<usize>,
    }

    impl Scripted {
        fn new(answers: Vec<Result<ParsedOrder, VenueError>>) -> Scripted {
            Scripted {
                answers: RefCell::new(answers.into()),
                reads: RefCell::new(0),
            }
        }
    }

    fn timeout() -> Result<ParsedOrder, VenueError> {
        Err(VenueError::new("revx", None, TIMEOUT))
    }

    fn resting() -> Result<ParsedOrder, VenueError> {
        Ok(ParsedOrder {
            order_id: "o1".into(),
            status: "new".into(),
            ..Default::default()
        })
    }

    impl Venue for Scripted {
        fn name(&self) -> &'static str {
            "revx"
        }
        fn parse_order(&self, _: &Value) -> Result<ParsedOrder, VenueError> {
            panic!("reconcile reads parsed statuses")
        }
        fn order_status(&self, _: &str, _: &str) -> Result<ParsedOrder, VenueError> {
            *self.reads.borrow_mut() += 1;
            self.answers
                .borrow_mut()
                .pop_front()
                .expect("a read past the script")
        }
        fn open_orders(&self, _: &str) -> Result<Vec<ParsedOrder>, VenueError> {
            Ok(vec![])
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
            Ok(1.0)
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

    struct Only<'a>(&'a Scripted);

    impl VenueSource for Only<'_> {
        fn venue(&self, _: &str) -> Result<&dyn Venue, String> {
            Ok(self.0)
        }
    }

    fn journal() -> Journal {
        let mut o = Order::new("c1");
        o.exch = "revx".into();
        o.pair = "AAA/USD".into();
        o.side = "buy".into();
        o.status = "new".into();
        o.order_id = Some("o1".into());
        let mut j = Journal::default();
        j.record(o);
        j
    }

    #[test]
    fn a_single_timeout_is_asked_again_and_leaves_no_poll_error() {
        let v = Scripted::new(vec![timeout(), resting()]);
        let mut j = journal();
        reconcile(&mut j, &Only(&v), 1.0);
        assert_eq!(*v.reads.borrow(), 2);
        let o = &j.orders["c1"];
        assert_eq!(o.last_error, None);
        assert_eq!(o.status, "new");
    }

    #[test]
    fn a_second_timeout_is_stored_as_the_poll_error() {
        let v = Scripted::new(vec![timeout(), timeout()]);
        let mut j = journal();
        let r = reconcile(&mut j, &Only(&v), 1.0);
        assert_eq!(*v.reads.borrow(), 2);
        assert!(r.changed);
        assert_eq!(j.orders["c1"].last_error.as_deref(), Some(TIMEOUT));
    }

    #[test]
    fn a_refused_request_is_not_asked_again() {
        let refused = "GET /api/v4/spot/orders/o1 -> 417 ";
        let v = Scripted::new(vec![Err(VenueError::new("gate", Some(417), refused))]);
        let mut j = journal();
        reconcile(&mut j, &Only(&v), 1.0);
        assert_eq!(*v.reads.borrow(), 1);
        assert_eq!(j.orders["c1"].last_error.as_deref(), Some(refused));
    }

    #[test]
    fn the_offline_switch_is_not_asked_again() {
        let off = "RUNGBOT_OFFLINE is set: no venue calls";
        let v = Scripted::new(vec![Err(VenueError::new("revx", None, off))]);
        assert!(read_status(&v, "AAA/USD", "o1").is_err());
        assert_eq!(*v.reads.borrow(), 1);
    }

    #[test]
    fn the_read_after_our_cancel_is_asked_again_too() {
        let v = Scripted::new(vec![timeout(), resting()]);
        let mut j = journal();
        assert_eq!(settle_cancel(&mut j, &v, "c1", 1.0), Settled::NothingFilled);
        assert_eq!(*v.reads.borrow(), 2);
    }
}
