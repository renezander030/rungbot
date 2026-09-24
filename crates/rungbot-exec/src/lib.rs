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
//! 4. Only then does [`gate`] sign a request.
//!
//! Only GTC limit orders are implemented. A resting order fills while your machine is
//! asleep, which is what lets a ladder run on a laptop; a market order needs you present
//! and is not something a schedule should ever send.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod gate;
pub mod guard;
pub mod ids;
pub mod journal;
pub mod keys;
pub mod pyfmt;
pub mod store;
#[cfg(test)]
mod testenv;

pub use gate::{Gate, GateError, PairInfo, VenueOrder};
pub use guard::{Caps, Context, Intent, Mode, Refusal};
pub use journal::{client_id, root_id, Journal, Order, Side, Status};
pub use keys::{Credentials, KeyError};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
