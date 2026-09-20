//! The idempotent order journal.
//!
//! Every order is written here with a **deterministic client id before it is placed**.
//! If the process dies between "the venue accepted it" and "the state was saved", the
//! next run sees the id already journaled and does not place it again. That is the
//! entire defence against a crash turning into a double trade, and it is the reason the
//! id is derived from intent rather than from a counter or a clock reading.
//!
//! Two details here were bought with real money and are preserved deliberately:
//!
//! * [`root_id`] strips reprice suffixes back to the original id. Deriving the next
//!   suffix from the *previous* id instead grows it a few characters every reprice until
//!   it exceeds the venue's 36-character limit, at which point the order silently stops
//!   being placed at all.
//! * [`Journal::filled_since`] takes an `inclusive` flag. A fill that lands in the same
//!   run that writes a balance baseline shares that run's timestamp, and the baseline is
//!   read before the fill settles. With a strict `>` neither run can explain it, and it
//!   shows up forever as phantom drift.
//!
//! Pure: no clock, no network, no filesystem. Persistence is the binary's job.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The window a client id is bucketed into. Same intent inside 30 minutes is one order.
pub const WINDOW_SECS: f64 = 1800.0;

/// Gate and most venues cap a client id at 36 characters.
pub const MAX_CLIENT_ID_LEN: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    fn tag(&self) -> char {
        match self {
            Side::Buy => 'b',
            Side::Sell => 's',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Journaled, not yet sent to the venue.
    Pending,
    /// Accepted by the venue and resting.
    Open,
    Filled,
    /// Cancelled by us.
    Cancelled,
    /// Cancelled, expired or rejected by the venue.
    VenueCancelled,
    /// Placement failed. A candidate for retry.
    Error,
}

impl Status {
    /// Is this order still live at the venue?
    pub fn is_open(&self) -> bool {
        matches!(self, Status::Pending | Status::Open)
    }

    /// Did the venue end it, rather than us?
    pub fn is_venue_ended(&self) -> bool {
        matches!(self, Status::VenueCancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub client_id: String,
    pub sym: String,
    pub pair: String,
    pub venue: String,
    pub side: Side,
    /// What placed it: `ladder_buy`, `ladder_sell`, `policy_sell`, ...
    pub kind: String,
    pub price: f64,
    /// Base amount intended.
    pub base: f64,
    /// Quote amount intended.
    pub quote: f64,
    pub status: Status,
    pub placed_ts: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue_order_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled_ts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled_base: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled_quote: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_price: Option<f64>,
    /// When the venue last changed this order's status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_ts: Option<f64>,
    /// Why we ended it, when we did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Has the freed cash been re-laddered?
    #[serde(default)]
    pub swept: bool,
}

/// Deterministic, exchange-safe client id.
///
/// The same intent inside one 30-minute window produces the same id, so a retry after a
/// crash collides with the existing journal entry instead of placing a second order.
pub fn client_id(sym: &str, side: Side, window_ts: f64, rung: i64) -> String {
    let sym: String = sym
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let window = (window_ts / WINDOW_SECS).floor() as i64;
    format!("cs{sym}{}{window}r{rung}", side.tag())
}

/// The id [`client_id`] first produced, with any reprice or retry suffix stripped.
///
/// Always derive a new suffix from **this**, never from the previous id. Chaining
/// suffixes grew one real order's id a few characters per reprice until it passed the
/// venue's 36-character cap and the order stopped being placed.
pub fn root_id(cid: &str) -> &str {
    // Shape: cs<SYM><b|s><digits>r<digits>, then anything we appended.
    let bytes = cid.as_bytes();
    if !cid.starts_with("cs") {
        return cid;
    }
    let mut i = 2;
    while i < bytes.len() && bytes[i].is_ascii_alphanumeric() && !matches!(bytes[i], b'b' | b's') {
        i += 1;
    }
    if i >= bytes.len() || !matches!(bytes[i], b'b' | b's') {
        return cid;
    }
    i += 1; // the side tag
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start || i >= bytes.len() || bytes[i] != b'r' {
        return cid;
    }
    i += 1; // 'r'
    let rung_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == rung_start {
        return cid;
    }
    &cid[..i]
}

/// Append a suffix to an id without ever chaining onto a previous one.
pub fn suffixed(cid: &str, suffix: &str) -> String {
    let out = format!("{}{suffix}", root_id(cid));
    // Truncating is better than emitting an id the venue will reject outright.
    if out.len() > MAX_CLIENT_ID_LEN {
        out[..MAX_CLIENT_ID_LEN].to_string()
    } else {
        out
    }
}

/// Notes that mean *we* ended an order, so its cash was already accounted for.
pub const OUR_CANCEL_NOTES: [&str; 4] = [
    "rolled into new tranche",
    "manual cancel",
    "repriced",
    "cover restored",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    pub orders: BTreeMap<String, Order>,
}

impl Journal {
    pub fn exists(&self, cid: &str) -> bool {
        self.orders.contains_key(cid)
    }

    pub fn get(&self, cid: &str) -> Option<&Order> {
        self.orders.get(cid)
    }

    /// Write an order. Returns `false` when the id was already present, which is the
    /// signal not to place it again.
    pub fn record(&mut self, order: Order) -> bool {
        if self.orders.contains_key(&order.client_id) {
            return false;
        }
        self.orders.insert(order.client_id.clone(), order);
        true
    }

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
            .filter(|o| o.status.is_open())
            .filter(|o| kind.is_none_or(|k| o.kind == k))
            .collect()
    }

    pub fn errored(&self, kind: Option<&str>) -> Vec<&Order> {
        self.orders
            .values()
            .filter(|o| o.status == Status::Error)
            .filter(|o| kind.is_none_or(|k| o.kind == k))
            .collect()
    }

    /// Orders that filled after `ts`.
    ///
    /// `inclusive` also returns fills at exactly `ts`. See the module docs: without it a
    /// fill landing in the same run that wrote a baseline can never be explained.
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

    /// Orders the **venue** ended whose freed cash has not been re-laddered.
    ///
    /// Our own cancels are excluded: that cash was already accounted for when we
    /// cancelled. An entry with no venue timestamp is skipped rather than guessed at.
    pub fn venue_cancelled_unswept(&self, kind: Option<&str>) -> Vec<&Order> {
        self.orders
            .values()
            .filter(|o| o.status.is_venue_ended() && !o.swept)
            .filter(|o| kind.is_none_or(|k| o.kind == k))
            .filter(|o| o.status_ts.is_some())
            .filter(|o| {
                let note = o.note.as_deref().unwrap_or("");
                !OUR_CANCEL_NOTES.iter().any(|m| note.contains(m))
            })
            .collect()
    }

    pub fn mark_swept(&mut self, cids: &[String]) {
        for c in cids {
            self.update(c, |o| o.swept = true);
        }
    }

    /// Drop finished orders older than `days`. Open orders are never archived.
    pub fn archive_old(&mut self, days: f64, now: f64) -> Vec<Order> {
        let cutoff = now - days * 86_400.0;
        let stale: Vec<String> = self
            .orders
            .values()
            .filter(|o| !o.status.is_open())
            .filter(|o| o.status_ts.or(o.filled_ts).unwrap_or(o.placed_ts) < cutoff)
            .map(|o| o.client_id.clone())
            .collect();
        stale.iter().filter_map(|c| self.orders.remove(c)).collect()
    }

    /// Total quote value of orders placed on or after `since`.
    pub fn notional_since(&self, since: f64) -> f64 {
        self.orders
            .values()
            .filter(|o| o.placed_ts >= since && o.status != Status::Error)
            .map(|o| o.quote)
            .sum()
    }

    /// How many orders were placed on or after `since`.
    pub fn count_since(&self, since: f64) -> usize {
        self.orders
            .values()
            .filter(|o| o.placed_ts >= since && o.status != Status::Error)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: f64 = 1_700_000_000.0;

    fn order(cid: &str, status: Status, placed: f64) -> Order {
        Order {
            client_id: cid.into(),
            sym: "AAA".into(),
            pair: "AAA_USDT".into(),
            venue: "gate".into(),
            side: Side::Buy,
            kind: "ladder_buy".into(),
            price: 10.0,
            base: 1.0,
            quote: 10.0,
            status,
            placed_ts: placed,
            venue_order_id: None,
            filled_ts: None,
            filled_base: None,
            filled_quote: None,
            avg_price: None,
            status_ts: None,
            note: None,
            swept: false,
        }
    }

    // --- the idempotency guarantee ------------------------------------------------

    #[test]
    fn the_same_intent_in_one_window_is_the_same_id() {
        let a = client_id("AAA", Side::Buy, T0, 1);
        let b = client_id("AAA", Side::Buy, T0 + 600.0, 1);
        assert_eq!(a, b, "ten minutes later is the same 30-minute window");
        let c = client_id("AAA", Side::Buy, T0 + 1801.0, 1);
        assert_ne!(a, c, "the next window is a new order");
    }

    #[test]
    fn side_and_rung_change_the_id() {
        assert_ne!(
            client_id("AAA", Side::Buy, T0, 1),
            client_id("AAA", Side::Sell, T0, 1)
        );
        assert_ne!(
            client_id("AAA", Side::Buy, T0, 1),
            client_id("AAA", Side::Buy, T0, 2)
        );
    }

    #[test]
    fn a_crash_retry_cannot_place_a_second_order() {
        let mut j = Journal::default();
        let cid = client_id("AAA", Side::Buy, T0, 1);
        assert!(
            j.record(order(&cid, Status::Pending, T0)),
            "first write wins"
        );
        // The process dies here, then re-runs with the same intent.
        assert!(
            !j.record(order(&cid, Status::Pending, T0 + 60.0)),
            "the second write is refused, which is what prevents the double trade"
        );
        assert_eq!(j.orders.len(), 1);
    }

    #[test]
    fn ids_stay_inside_the_venues_length_cap() {
        let cid = client_id("VERYLONGSYMBOL", Side::Buy, T0, 99);
        assert!(
            cid.len() <= MAX_CLIENT_ID_LEN,
            "{cid} is {} chars",
            cid.len()
        );
    }

    #[test]
    fn a_symbol_with_punctuation_still_produces_a_safe_id() {
        let cid = client_id("BTC/USD", Side::Sell, T0, 3);
        assert!(cid.chars().all(|c| c.is_ascii_alphanumeric()), "{cid}");
        assert!(cid.contains("BTCUSD"), "{cid}");
    }

    // --- the reprice-chaining bug -------------------------------------------------

    #[test]
    fn root_id_strips_a_suffix_back_to_the_original() {
        let base = client_id("FET", Side::Sell, T0, 2);
        assert_eq!(root_id(&base), base, "an unsuffixed id is its own root");
        assert_eq!(root_id(&format!("{base}x1")), base);
        assert_eq!(root_id(&format!("{base}x1x2x3")), base);
    }

    #[test]
    fn suffixes_never_chain_and_never_outgrow_the_cap() {
        // The real failure: deriving each suffix from the previous id grew it until the
        // venue rejected it and the sell silently stopped being placed.
        let base = client_id("FET", Side::Sell, T0, 2);
        let mut cid = base.clone();
        for i in 1..40 {
            cid = suffixed(&cid, &format!("x{i}"));
            assert!(
                cid.len() <= MAX_CLIENT_ID_LEN,
                "grew to {} chars: {cid}",
                cid.len()
            );
        }
        assert!(
            cid.starts_with(&base),
            "and it is still derived from the root: {cid}"
        );
    }

    #[test]
    fn root_id_leaves_an_unrecognised_id_alone() {
        assert_eq!(root_id("not-ours-at-all"), "not-ours-at-all");
        assert_eq!(root_id(""), "");
        assert_eq!(root_id("cs"), "cs");
    }

    // --- the phantom-drift fix ----------------------------------------------------

    #[test]
    fn filled_since_can_include_a_fill_at_exactly_the_baseline() {
        let mut j = Journal::default();
        let mut o = order("a", Status::Filled, T0);
        o.filled_ts = Some(T0);
        j.record(o);

        assert!(
            j.filled_since(T0, false).is_empty(),
            "strictly after excludes it, which is how a fill becomes unexplainable"
        );
        assert_eq!(
            j.filled_since(T0, true).len(),
            1,
            "inclusive counts it exactly once, after which it folds into the baseline"
        );
    }

    // --- venue-ended vs our own cancels --------------------------------------------

    #[test]
    fn only_the_venues_own_cancels_need_sweeping() {
        let mut j = Journal::default();

        let mut ours = order("ours", Status::VenueCancelled, T0);
        ours.status_ts = Some(T0);
        ours.note = Some("repriced up to the new rung".into());
        j.record(ours);

        let mut theirs = order("theirs", Status::VenueCancelled, T0);
        theirs.status_ts = Some(T0);
        j.record(theirs);

        let mut ours_cancel = order("mine", Status::Cancelled, T0);
        ours_cancel.status_ts = Some(T0);
        j.record(ours_cancel);

        let got: Vec<&str> = j
            .venue_cancelled_unswept(None)
            .iter()
            .map(|o| o.client_id.as_str())
            .collect();
        assert_eq!(
            got,
            vec!["theirs"],
            "our own cancels were already accounted for"
        );
    }

    #[test]
    fn an_entry_without_a_venue_timestamp_is_skipped_not_guessed() {
        let mut j = Journal::default();
        j.record(order("old", Status::VenueCancelled, T0)); // no status_ts
        assert!(j.venue_cancelled_unswept(None).is_empty());
    }

    #[test]
    fn sweeping_takes_an_order_out_of_the_list() {
        let mut j = Journal::default();
        let mut o = order("x", Status::VenueCancelled, T0);
        o.status_ts = Some(T0);
        j.record(o);
        assert_eq!(j.venue_cancelled_unswept(None).len(), 1);
        j.mark_swept(&["x".to_string()]);
        assert!(j.venue_cancelled_unswept(None).is_empty());
    }

    // --- housekeeping ---------------------------------------------------------------

    #[test]
    fn open_orders_are_never_archived_however_old() {
        let mut j = Journal::default();
        j.record(order("open", Status::Open, T0 - 400.0 * 86_400.0));
        let mut done = order("done", Status::Filled, T0 - 400.0 * 86_400.0);
        done.filled_ts = Some(T0 - 400.0 * 86_400.0);
        j.record(done);

        let archived = j.archive_old(90.0, T0);
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].client_id, "done");
        assert!(
            j.exists("open"),
            "a resting order is still real money at the venue"
        );
    }

    #[test]
    fn daily_totals_ignore_failed_placements() {
        let mut j = Journal::default();
        j.record(order("a", Status::Open, T0));
        j.record(order("b", Status::Filled, T0 + 10.0));
        j.record(order("c", Status::Error, T0 + 20.0));
        j.record(order("old", Status::Filled, T0 - 86_400.0));

        assert_eq!(
            j.count_since(T0),
            2,
            "an order that never reached the venue is not one"
        );
        assert_eq!(j.notional_since(T0), 20.0);
    }

    #[test]
    fn open_and_errored_filter_by_kind() {
        let mut j = Journal::default();
        let mut sell = order("s", Status::Open, T0);
        sell.kind = "ladder_sell".into();
        j.record(sell);
        j.record(order("b", Status::Open, T0));
        j.record(order("e", Status::Error, T0));

        assert_eq!(j.open_orders(None).len(), 2);
        assert_eq!(j.open_orders(Some("ladder_sell")).len(), 1);
        assert_eq!(j.errored(None).len(), 1);
        assert_eq!(j.errored(Some("ladder_sell")).len(), 0);
    }
}
