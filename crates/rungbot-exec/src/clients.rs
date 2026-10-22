//! One client per venue, each built the first time it is needed.
//!
//! Keys load lazily, per venue: a run that only touches Gate never reads a Revolut X
//! key, and a missing key for one venue fails only that venue's rows (reconcile stores
//! the error on them) instead of the whole run. [`Clients::public`] loads no key at all,
//! for the reads every venue serves unsigned (pair status, prices).

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
    keyless: bool,
}

impl Clients {
    /// Real transport, system clock, keys from the environment or the key files.
    pub fn new() -> Clients {
        Clients::default()
    }

    /// Clients that hold no key: public reads only. A signed call made through one is
    /// refused (Revolut X) or rejected by the venue (Gate, Binance).
    pub fn public() -> Clients {
        Clients {
            keyless: true,
            ..Clients::default()
        }
    }

    fn creds(&self, venue: &str) -> Result<keys::Credentials, String> {
        if self.keyless {
            return Ok(keys::Credentials {
                key: String::new(),
                secret: String::new(),
            });
        }
        keys::load(venue, None).map_err(|e| e.to_string())
    }

    fn revx_creds(&self) -> Result<crate::revx::RevxCredentials, String> {
        if self.keyless {
            return Ok(crate::revx::RevxCredentials {
                key: String::new(),
                pem_path: Default::default(),
            });
        }
        keys::load_revx(None).map_err(|e| e.to_string())
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
                .get_or_init(|| self.creds("gate").map(|c| Gate::with_http(c, self.http())))
                .as_ref()
                .map(|g| g as &dyn Venue)
                .map_err(Clone::clone),
            "revx" => self
                .revx
                .get_or_init(|| self.revx_creds().map(|c| Revx::with_http(c, self.http())))
                .as_ref()
                .map(|g| g as &dyn Venue)
                .map_err(Clone::clone),
            "binance" => self
                .binance
                .get_or_init(|| {
                    self.creds("binance")
                        .map(|c| Binance::with_http(c, self.http()))
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
