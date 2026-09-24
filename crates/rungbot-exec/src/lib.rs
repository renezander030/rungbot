//! `rungbot-exec` — opt-in live execution.
//!
//! This crate is deliberately **not** a dependency of the `rungbot` crate. The `rungbot`
//! binary holds no venue key and contains no request signing, and a test asserts that
//! stays true. Everything that can move money lives here, behind its own binary that you
//! have to install and arm on purpose.
//!
//! The order of operations is the safety property:
//!
//! 1. The ladder decides, in `rungbot-core`, with no credential in the process.
//! 2. [`guard::check`] refuses anything outside the rails.
//! 3. [`journal`] writes a deterministic id **before** the venue is called.
//! 4. Only then does a venue client ([`gate`], [`revx`], [`binance`]) sign a request.
//! 5. [`reconcile`] later reads back what the venue did and books it, and
//!    [`housekeeping`] acts on it: cost basis, paired sells, retries, reprices, drift.
//!
//! Every network call goes through [`http::Transport`], which refuses all of them while
//! `RUNGBOT_OFFLINE` is set.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod binance;
pub mod clients;
pub mod gate;
pub mod guard;
pub mod housekeeping;
pub mod http;
pub mod ids;
pub mod import;
pub mod journal;
pub mod keys;
pub mod pyfmt;
pub mod reconcile;
pub mod revx;
pub mod sellcheck;
pub mod store;
#[cfg(test)]
mod testenv;
pub mod venue;

pub use binance::Binance;
pub use clients::Clients;
pub use gate::{Gate, PairInfo};
pub use guard::{Caps, Context, Intent, Mode, Refusal};
pub use http::VenueError;
pub use journal::{client_id, root_id, Journal, Order, Side};
pub use keys::{Credentials, KeyError};
pub use reconcile::{reconcile, settle_cancel, Adopted, Reconciled, Settled, VenueSource};
pub use revx::{Revx, RevxCredentials};
pub use venue::{Balance, Limits, ParsedOrder, Venue};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
