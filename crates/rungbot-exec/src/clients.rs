//! One client per venue, each built the first time it is needed.
//!
//! Keys load lazily, per venue: a run that only touches Gate never reads a Revolut X
//! key, and a missing key for one venue fails only that venue's rows (reconcile stores
//! the error on them) instead of the whole run.

use std::cell::OnceCell;

use crate::binance::Binance;
use crate::gate::Gate;
use crate::http::Http;
use crate::keys;
use crate::reconcile::VenueSource;
use crate::revx::Revx;
use crate::venue::Venue;

pub const VENUES: [&str; 3] = ["gate", "revx", "binance"];

#[derive(Default)]
pub struct Clients {
    http: Option<Http>,
    gate: OnceCell<Result<Gate, String>>,
    revx: OnceCell<Result<Revx, String>>,
    binance: OnceCell<Result<Binance, String>>,
}

impl Clients {
    /// Real transport, system clock, keys from the environment or the key files.
    pub fn new() -> Clients {
        Clients::default()
    }

    /// Every client built on `http` (a scripted transport in tests).
    pub fn with_http(http: Http) -> Clients {
        Clients {
            http: Some(http),
            ..Clients::default()
        }
    }

    fn http(&self) -> Http {
        self.http.clone().unwrap_or_default()
    }

    pub fn get(&self, exch: &str) -> Result<&dyn Venue, String> {
        match exch {
            "gate" => self
                .gate
                .get_or_init(|| {
                    keys::load("gate", None)
                        .map(|c| Gate::with_http(c, self.http()))
                        .map_err(|e| e.to_string())
                })
                .as_ref()
                .map(|g| g as &dyn Venue)
                .map_err(Clone::clone),
            "revx" => self
                .revx
                .get_or_init(|| {
                    keys::load_revx(None)
                        .map(|c| Revx::with_http(c, self.http()))
                        .map_err(|e| e.to_string())
                })
                .as_ref()
                .map(|g| g as &dyn Venue)
                .map_err(Clone::clone),
            "binance" => self
                .binance
                .get_or_init(|| {
                    keys::load("binance", None)
                        .map(|c| Binance::with_http(c, self.http()))
                        .map_err(|e| e.to_string())
                })
                .as_ref()
                .map(|g| g as &dyn Venue)
                .map_err(Clone::clone),
            other => Err(format!(
                "unknown venue {other:?}: expected one of {}",
                VENUES.join(", ")
            )),
        }
    }
}

impl VenueSource for Clients {
    fn venue(&self, exch: &str) -> Result<&dyn Venue, String> {
        self.get(exch)
    }
}
