//! The surface every venue client offers, and the order shape they all parse into.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::http::VenueError;

/// One order as a venue reports it, normalised across venues.
///
/// * `base_qty` is `None` only for a Gate market buy with no average price yet: its
///   amounts are in the quote asset and the base cannot be derived.
/// * `quote` is the quote actually filled; `avg_price` the per-unit fill price.
/// * `fee` / `fee_asset` as reported. A buy fee charged in the base coin reduces what is
///   held (see [`crate::reconcile`]).
/// * `qty` is the order's base size (0 for a Gate market buy, whose size is quote).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParsedOrder {
    #[serde(default)]
    pub order_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub filled: bool,
    #[serde(default)]
    pub base_qty: Option<f64>,
    #[serde(default)]
    pub quote: f64,
    #[serde(default)]
    pub avg_price: Option<f64>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub fee: f64,
    #[serde(default)]
    pub fee_asset: Option<String>,
    #[serde(default)]
    pub side: String,
    #[serde(default)]
    pub qty: f64,
    #[serde(default)]
    pub client_id: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub free: f64,
    pub locked: f64,
}

/// Minimums and steps for one pair, in the venue-neutral shape callers size orders with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub min_base: f64,
    pub min_quote: f64,
}

/// What reconcile, placement and the status views need from any venue.
///
/// Order methods return the venue's raw response, exactly as the venue sent it; pass it
/// to [`Venue::parse_order`]. Amounts are rounded down to the pair's precision inside
/// the client before they are sent.
pub trait Venue {
    /// `gate`, `revx` or `binance`: the journal's `exch` value.
    fn name(&self) -> &'static str;
    fn parse_order(&self, raw: &serde_json::Value) -> Result<ParsedOrder, VenueError>;
    fn order_status(&self, pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError>;
    fn open_orders(&self, pair: &str) -> Result<Vec<ParsedOrder>, VenueError>;
    fn cancel(&self, pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError>;
    fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError>;
    fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError>;
    fn price(&self, pair: &str) -> Result<f64, VenueError>;
    fn limits(&self, pair: &str) -> Result<Limits, VenueError>;
    fn round_amount(&self, pair: &str, amount: f64) -> Result<f64, VenueError>;
    fn round_price(&self, pair: &str, price: f64) -> Result<f64, VenueError>;
    /// Spend `quote` of the quote asset at market.
    fn market_buy(
        &self,
        pair: &str,
        quote: f64,
        client_id: Option<&str>,
    ) -> Result<serde_json::Value, VenueError>;
    fn market_sell(
        &self,
        pair: &str,
        base: f64,
        client_id: Option<&str>,
    ) -> Result<serde_json::Value, VenueError>;
    fn limit_buy(
        &self,
        pair: &str,
        base: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<serde_json::Value, VenueError>;
    fn limit_sell(
        &self,
        pair: &str,
        base: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<serde_json::Value, VenueError>;
    /// The smallest quantity increment the venue accepts for `pair`.
    fn qty_step(&self, _pair: &str) -> Result<f64, VenueError> {
        Ok(1e-8)
    }
    /// Stable credited on-chain since `since`, from the venue's own deposit history, or
    /// `None` when this venue cannot say (no deposit history on this client).
    fn deposits(&self, _since: f64) -> Option<Result<Vec<crate::gate::Deposit>, VenueError>> {
        None
    }
    /// Does `venue_cid`, as this venue echoes it, belong to journal id `cid`?
    fn id_matches(&self, venue_cid: &str, cid: &str) -> bool {
        venue_id_matches(self.name(), venue_cid, cid)
    }
}

/// `10 ** -p` for a precision in decimal places, with `0` read as 8 (a venue that
/// reports none).
pub fn step_from_precision(p: i64) -> f64 {
    let p = if p == 0 { 8 } else { p };
    10f64.powf(-(p as f64))
}

/// Is `venue_cid` (what the venue echoes) the id we sent for journal id `cid`? Revolut X
/// is sent a uuid5 of it, Gate a hashed-down text, Binance and anything else the id.
pub fn venue_id_matches(exch: &str, venue_cid: &str, cid: &str) -> bool {
    if venue_cid.is_empty() || cid.is_empty() {
        return false;
    }
    match exch {
        "revx" => crate::ids::safe_revx_cid(cid).is_ok_and(|u| venue_cid.to_lowercase() == u),
        "gate" => crate::ids::gate_cid(cid).is_ok_and(|g| venue_cid == g),
        _ => venue_cid.starts_with(cid),
    }
}

/// Bad venue JSON as an error in the venue's voice.
pub(crate) fn shape_err(venue: &'static str, what: impl core::fmt::Display) -> VenueError {
    VenueError::new(venue, None, what.to_string())
}
