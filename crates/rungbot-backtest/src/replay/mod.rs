//! Historical replays: cycle studies over daily candles.
//!
//! Research tools, not trading logic. Each one reads public daily candles (see [`data`]
//! for the cache formats) and prints a report plus a JSON document with every number the
//! report was drawn from.
//!
//! * [`btc_confirm`] / [`btc_confirm_analysis`] — BTC bull-label confirmations since 2017
//!   and what resting dip rungs did in the 90 days after each.
//! * [`alt_confirm`] — whether those confirmations transfer to the alts, a depth/weight
//!   profile sweep, and a current-event read against live rungs.
//! * [`btc_top`] / [`alt_top`] — the anatomy of past cycle tops and what exit rules and
//!   the ladder's own sell side would have captured.
//! * [`sellpolicy_replay`] — the bull sell policy replayed over those tops.
//! * [`microstate`] — the current 30-day microstate of each coin against its rungs.

pub mod alt_confirm;
pub mod alt_top;
pub mod btc_confirm;
pub mod btc_confirm_analysis;
pub mod btc_top;
pub mod data;
pub mod microstate;
pub mod sellpolicy_replay;
