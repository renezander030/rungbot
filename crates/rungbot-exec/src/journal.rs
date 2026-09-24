//! The idempotent order journal.
//!
//! Every order is written here with a **deterministic client id before it is placed**.
//! If the process dies between "the venue accepted it" and "the state was saved", the
//! next run sees the id already journaled and does not place it again. That is the
//! entire defence against a crash turning into a double trade, and it is the reason the
//! id is derived from intent rather than from a counter or a clock reading.
//!
//! On disk the journal is one JSON object, `{client_id: row}`, in the order rows were
//! first written. A row is an [`Order`]; its field names are the stable contract other
//! programs read, so they never change spelling. Fields this crate does not know are
//! kept as they were read and written back unchanged.
//!
//! Two details were bought with real money and are preserved deliberately:
//!
//! * [`root_id`] strips reprice and retry suffixes back to the original id. Deriving the
//!   next suffix from the *previous* id instead grows it a few characters every reprice
//!   until it exceeds the venue's 36-character limit, and the order stops being placed.
//! * [`Journal::filled_since`] takes an `inclusive` flag. A fill that lands in the same
//!   run that writes a balance baseline shares that run's timestamp, and the baseline is
//!   read before the fill settles. With a strict `>` neither run can explain it, and it
//!   shows up forever as phantom drift.
//!
//! Pure: no clock, no network, no filesystem. Persistence lives in [`crate::store`].

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// The window a client id is bucketed into. Same intent inside 30 minutes is one order.
pub const WINDOW_SECS: f64 = 1800.0;

/// Binance caps a client id at 36 characters; the journal holds to the same cap.
pub const MAX_CLIENT_ID_LEN: usize = crate::ids::CID_MAX;

/// Statuses that mean the order may still be resting at the venue. Each venue spells
/// them its own way and the journal keeps the venue's spelling.
pub const OPEN: [&str; 6] = [
    "pending",
    "placed",
    "open",
    "new",
    "NEW",
    "PARTIALLY_FILLED",
];

/// Statuses that mean the order left the book without filling completely.
pub const VENUE_CANCELLED: [&str; 7] = [
    "cancelled",
    "canceled",
    "CANCELED",
    "expired",
    "EXPIRED",
    "rejected",
    "REJECTED",
];

/// Note fragments that mean *we* ended an order. A cancel without one reads as the
/// venue's, whose cash is still owed a re-ladder, and is never archived.
pub const OUR_CANCEL_NOTES: [&str; 5] = [
    "rolled into new tranche",
    "manual --cancel",
    "repriced up",
    "cover restored",
    "bull sweep",
];

pub fn is_open_status(s: &str) -> bool {
    OPEN.contains(&s)
}

pub fn is_venue_cancelled_status(s: &str) -> bool {
    VENUE_CANCELLED.contains(&s)
}

/// Does this note say we ended the order ourselves?
pub fn is_our_cancel(note: Option<&str>) -> bool {
    let note = note.unwrap_or("");
    OUR_CANCEL_NOTES.iter().any(|m| note.contains(m))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }

    fn tag(&self) -> char {
        match self {
            Side::Buy => 'b',
            Side::Sell => 's',
        }
    }

    pub fn parse(s: &str) -> Option<Side> {
        match s.to_ascii_lowercase().as_str() {
            "buy" => Some(Side::Buy),
            "sell" => Some(Side::Sell),
            _ => None,
        }
    }
}

/// One journal row.
///
/// Every field but `client_id` is optional, because rows written by different paths
/// carry different fields. An absent field and a `null` read the same.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sym: String,
    /// The venue: `gate`, `revx` or `binance`.
    #[serde(default, alias = "venue", skip_serializing_if = "String::is_empty")]
    pub exch: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pair: String,
    /// `buy` or `sell`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub side: String,
    /// What placed it: `market_buy`, `market_sell`, `limit_sell`, `deploy_buy`, ...
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// The venue's own spelling: see [`OPEN`] and [`VENUE_CANCELLED`], plus `filled` and
    /// `error`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Quote amount intended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>,
    /// Base amount intended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rung: Option<i64>,
    /// When the intent was journaled.
    #[serde(default, alias = "placed_ts", skip_serializing_if = "Option::is_none")]
    pub ts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_ts: Option<f64>,
    /// The venue's id for the order, once placed.
    #[serde(
        default,
        alias = "venue_order_id",
        deserialize_with = "loose_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub order_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled_base: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled_quote: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_price: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled_ts: Option<f64>,
    /// Partial-fill progress on an order still resting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_base: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_quote: Option<f64>,
    /// When the venue last changed the status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_ts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gave_up: Option<bool>,
    /// Has the freed cash been re-laddered?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swept: Option<bool>,
    /// The fill was booked from an order that left the book part-filled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<bool>,
    /// Booked by [`crate::reconcile::settle_cancel`], handed out by the next reconcile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book_pending: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_error: Option<String>,
    /// Re-attached to a venue-side replacement order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_ts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_size: Option<bool>,
    /// Anything else a row carried, kept verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn loose_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(crate::pyfmt::value_str(&v)),
    })
}

impl Order {
    /// A fresh row, the shape every placement path starts from.
    pub fn new(client_id: &str) -> Order {
        Order {
            client_id: client_id.into(),
            ..Default::default()
        }
    }

    pub fn is_open(&self) -> bool {
        is_open_status(&self.status)
    }

    pub fn side_enum(&self) -> Option<Side> {
        Side::parse(&self.side)
    }

    /// Base amount this order has put on (or taken off) the books so far.
    pub fn booked_base(&self) -> f64 {
        if truthy_f(self.filled_ts) {
            self.filled_base.unwrap_or(0.0)
        } else {
            self.part_base.unwrap_or(0.0)
        }
    }
}

/// `bool(x)` for an optional number.
pub(crate) fn truthy_f(x: Option<f64>) -> bool {
    x.is_some_and(|v| v != 0.0)
}

/// Deterministic, exchange-safe client id: `cs{SYM}{b|s}{slot}r{rung}`, where the slot
/// is the 30-minute window. The same intent inside one window produces the same id, so a
/// retry after a crash collides with the existing row instead of placing a second order.
///
/// The symbol is used as given; [`crate::ids::safe_cid`] repairs any character a venue
/// would refuse.
pub fn client_id(sym: &str, side: Side, window_ts: f64, rung: i64) -> String {
    let window = (window_ts / WINDOW_SECS).floor() as i64;
    format!("cs{sym}{}{window}r{rung}", side.tag())
}

/// The id [`client_id`] first produced (with an optional leading `m` for a market sell),
/// with any reprice or retry suffix stripped: the match of `^m?cs[A-Z0-9]+[bs]\d+r\d+`,
/// or the id unchanged when it has another shape.
///
/// Always derive a new suffix from **this**, never from the previous id.
pub fn root_id(cid: &str) -> &str {
    let b = cid.as_bytes();
    let try_from = |start: usize| -> Option<usize> {
        let mut i = start;
        if !cid[i..].starts_with("cs") {
            return None;
        }
        i += 2;
        let s = i;
        while i < b.len() && (b[i].is_ascii_uppercase() || b[i].is_ascii_digit()) {
            i += 1;
        }
        if i == s || i >= b.len() || !matches!(b[i], b'b' | b's') {
            return None;
        }
        i += 1;
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s || i >= b.len() || b[i] != b'r' {
            return None;
        }
        i += 1;
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        (i > s).then_some(i)
    };
    let end = if cid.starts_with('m') {
        try_from(1)
    } else {
        None
    }
    .or_else(|| try_from(0));
    match end {
        Some(e) => &cid[..e],
        None => cid,
    }
}

/// A suffixed id derived from the root, never chained onto a previous suffix, and
/// coerced to what the venues accept (an over-long id keeps a prefix and a hash tail).
pub fn suffixed(cid: &str, suffix: &str) -> String {
    let raw = format!("{}{suffix}", root_id(cid));
    crate::ids::safe_cid(&raw).unwrap_or(raw)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Journal {
    pub orders: IndexMap<String, Order>,
}

fn kind_matches(o: &Order, kind: Option<&str>) -> bool {
    kind.is_none_or(|k| o.kind == k)
}

impl Journal {
    pub fn exists(&self, cid: &str) -> bool {
        self.orders.contains_key(cid)
    }

    pub fn get(&self, cid: &str) -> Option<&Order> {
        self.orders.get(cid)
    }

    pub fn get_mut(&mut self, cid: &str) -> Option<&mut Order> {
        self.orders.get_mut(cid)
    }

    /// Write a row, replacing any row with the same id in place. Callers check
    /// [`Journal::exists`] first when a second write must not happen.
    pub fn record(&mut self, order: Order) -> &Order {
        let cid = order.client_id.clone();
        self.orders.insert(cid.clone(), order);
        &self.orders[&cid]
    }

    /// Write a row only when its id is new. `false` is the signal not to place it again.
    pub fn insert_new(&mut self, order: Order) -> bool {
        if self.orders.contains_key(&order.client_id) {
            return false;
        }
        self.orders.insert(order.client_id.clone(), order);
        true
    }

    /// Change a row in place. `false` when there is no such row, which changes nothing.
    pub fn update(&mut self, cid: &str, f: impl FnOnce(&mut Order)) -> bool {
        match self.orders.get_mut(cid) {
            Some(o) => {
                f(o);
                true
            }
            None => false,
        }
    }

    pub fn open_orders(&self, kind: Option<&str>) -> Vec<&Order> {
        self.orders
            .values()
            .filter(|o| o.is_open() && kind_matches(o, kind))
            .collect()
    }

    /// Rows whose placement failed: candidates for an automatic retry.
    pub fn errored(&self, kind: Option<&str>) -> Vec<&Order> {
        self.orders
            .values()
            .filter(|o| o.status == "error" && kind_matches(o, kind))
            .collect()
    }

    /// Rows that filled after `ts`. `inclusive` also returns fills at exactly `ts`; see
    /// the module docs for why that matters.
    pub fn filled_since(&self, ts: f64, inclusive: bool) -> Vec<&Order> {
        self.orders
            .values()
            .filter(|o| {
                let f = o.filled_ts.unwrap_or(0.0);
                if inclusive {
                    f >= ts
                } else {
                    f > ts
                }
            })
            .collect()
    }

    /// Rows the **venue** cancelled whose freed cash has not been re-laddered. Our own
    /// cancels are excluded, and so is a row with no venue timestamp (written before the
    /// timestamp existed), which is skipped rather than guessed at. `kind: None` takes
    /// every kind.
    pub fn venue_cancelled_unswept(&self, kind: Option<&str>) -> Vec<&Order> {
        self.orders
            .values()
            .filter(|o| kind_matches(o, kind))
            .filter(|o| is_venue_cancelled_status(&o.status) && o.swept != Some(true))
            .filter(|o| !is_our_cancel(o.note.as_deref()))
            .filter(|o| truthy_f(o.status_ts))
            .collect()
    }

    pub fn mark_swept(&mut self, cids: &[String]) {
        for c in cids {
            self.update(c, |o| o.swept = Some(true));
        }
    }

    /// Take finished cancel rows older than `days` out of the journal and return them,
    /// oldest position first, for the caller to append to the archive.
    ///
    /// Kept, always: open, filled and error rows; any row with a partial fill (its cash
    /// is netted elsewhere); a venue cancel not yet swept (its cash is still owed); and a
    /// row with no timestamp at all.
    pub fn archive_old(&mut self, days: f64, now: f64) -> Vec<Order> {
        let cutoff = now - days * 86_400.0;
        let moved: Vec<String> = self
            .orders
            .values()
            .filter(|o| {
                let st = o.status.as_str();
                if is_open_status(st) || st == "filled" || st == "error" {
                    return false;
                }
                if truthy_f(o.part_quote) || truthy_f(o.filled_quote) {
                    return false;
                }
                if is_venue_cancelled_status(st)
                    && !is_our_cancel(o.note.as_deref())
                    && o.swept != Some(true)
                {
                    return false;
                }
                let ts = if truthy_f(o.status_ts) {
                    o.status_ts
                } else {
                    o.ts
                }
                .unwrap_or(0.0);
                ts != 0.0 && ts <= cutoff
            })
            .map(|o| o.client_id.clone())
            .collect();
        moved
            .iter()
            .filter_map(|c| self.orders.shift_remove(c))
            .collect()
    }

    /// Total quote of orders journaled on or after `since`, failed placements excluded.
    pub fn notional_since(&self, since: f64) -> f64 {
        self.orders
            .values()
            .filter(|o| o.ts.unwrap_or(0.0) >= since && o.status != "error")
            .map(|o| o.quote.unwrap_or(0.0))
            // Not .sum(): an empty float sum is -0.0, which prints as "-0.00".
            .fold(0.0, |a, b| a + b)
    }

    /// How many orders were journaled on or after `since`, failed placements excluded.
    pub fn count_since(&self, since: f64) -> usize {
        self.orders
            .values()
            .filter(|o| o.ts.unwrap_or(0.0) >= since && o.status != "error")
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: f64 = 1_700_000_000.0;

    fn order(cid: &str, status: &str, ts: f64) -> Order {
        Order {
            client_id: cid.into(),
            sym: "AAA".into(),
            exch: "gate".into(),
            pair: "AAA_USDT".into(),
            side: "buy".into(),
            kind: "ladder_buy".into(),
            status: status.into(),
            price: Some(10.0),
            base: Some(1.0),
            quote: Some(10.0),
            ts: Some(ts),
            ..Default::default()
        }
    }

    #[test]
    fn the_same_intent_in_one_window_is_the_same_id() {
        let a = client_id("AAA", Side::Buy, T0, 1);
        assert_eq!(a, client_id("AAA", Side::Buy, T0 + 600.0, 1));
        assert_ne!(a, client_id("AAA", Side::Buy, T0 + 1801.0, 1));
        assert_ne!(a, client_id("AAA", Side::Sell, T0, 1));
        assert_ne!(a, client_id("AAA", Side::Buy, T0, 2));
    }

    #[test]
    fn a_crash_retry_cannot_place_a_second_order() {
        let mut j = Journal::default();
        let cid = client_id("AAA", Side::Buy, T0, 1);
        assert!(j.insert_new(order(&cid, "pending", T0)));
        assert!(!j.insert_new(order(&cid, "pending", T0 + 60.0)));
        assert_eq!(j.orders.len(), 1);
    }

    #[test]
    fn suffixes_never_chain_and_never_outgrow_the_cap() {
        let base = client_id("AAA", Side::Sell, T0, 2);
        let mut cid = base.clone();
        for i in 1..40 {
            cid = suffixed(&cid, &format!("u{}", 90000 + i));
            assert!(cid.len() <= MAX_CLIENT_ID_LEN, "{cid}");
            assert_eq!(root_id(&cid), base);
        }
    }

    #[test]
    fn record_replaces_in_place_and_keeps_the_order_of_rows() {
        let mut j = Journal::default();
        j.record(order("a", "pending", T0));
        j.record(order("b", "pending", T0));
        j.record(order("a", "open", T0));
        let ids: Vec<&str> = j.orders.keys().map(String::as_str).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(j.get("a").unwrap().status, "open");
    }

    #[test]
    fn daily_totals_ignore_failed_placements() {
        let mut j = Journal::default();
        j.record(order("a", "open", T0));
        j.record(order("b", "filled", T0 + 10.0));
        j.record(order("c", "error", T0 + 20.0));
        j.record(order("old", "filled", T0 - 86_400.0));
        assert_eq!(j.count_since(T0), 2);
        assert_eq!(j.notional_since(T0), 20.0);
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let raw =
            r#"{"a": {"client_id": "a", "status": "open", "custom": {"x": [1, 2]}, "base": null}}"#;
        let j: Journal = serde_json::from_str(raw).unwrap();
        let back: serde_json::Value = serde_json::to_value(&j).unwrap();
        assert_eq!(back["a"]["custom"]["x"][1], 2);
        assert!(
            back["a"].get("base").is_none(),
            "null and absent read the same"
        );
    }
}
