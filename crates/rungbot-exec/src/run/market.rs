//! Public market data: the 24h ticker each coin's signal reads, and the daily closes
//! the regime reads. No key, no signature.

use crate::binance::Binance;
use crate::gate::Gate;
use crate::http::{self, Http};
use crate::keys::Credentials;
use crate::revx::{Revx, RevxCredentials};

/// Where the run reads public prices. A trait so a test can script it.
pub trait Market {
    /// `(last price, 24h change in percent)` for a pair on its venue.
    fn ticker(&self, exch: &str, pair: &str) -> Result<(f64, f64), String>;
    /// The last `days` daily closes, oldest first.
    fn closes(&self, exch: &str, symbol: &str, days: usize) -> Result<Vec<f64>, String>;
}

/// The real thing, over [`crate::http`] (so `RUNGBOT_OFFLINE` refuses it too).
pub struct PublicMarket {
    http: Http,
    gate: Gate,
    binance: Binance,
    revx: Revx,
}

impl Default for PublicMarket {
    fn default() -> Self {
        Self::with_http(Http::default())
    }
}

impl PublicMarket {
    pub fn with_http(http: Http) -> Self {
        let none = || Credentials {
            key: String::new(),
            secret: String::new(),
        };
        PublicMarket {
            gate: Gate::with_http(none(), http.clone()),
            binance: Binance::with_http(none(), http.clone()),
            revx: Revx::with_http(
                RevxCredentials {
                    key: String::new(),
                    pem_path: Default::default(),
                },
                http.clone(),
            ),
            http,
        }
    }

    fn get_list(&self, venue: &'static str, url: &str) -> Result<Vec<serde_json::Value>, String> {
        let r = http::request(&self.http, venue, "GET", url, vec![], None)
            .map_err(|e| e.to_string())?;
        match (r.status, r.json.as_ref().and_then(|v| v.as_array())) {
            (Some(200), Some(a)) => Ok(a.clone()),
            _ => Err(format!("{} -> {}", url, r.status_str())),
        }
    }
}

fn num(v: Option<&serde_json::Value>) -> Result<f64, String> {
    v.ok_or_else(|| "short candle".to_string())
        .and_then(crate::pyfmt::to_float)
}

impl Market for PublicMarket {
    fn ticker(&self, exch: &str, pair: &str) -> Result<(f64, f64), String> {
        match exch {
            "binance" => self.binance.ticker(pair),
            "revx" => self.revx.ticker(pair),
            _ => self.gate.ticker(pair),
        }
        .map_err(|e| e.to_string())
    }

    fn closes(&self, exch: &str, symbol: &str, days: usize) -> Result<Vec<f64>, String> {
        if exch == "binance" {
            let rows = self
                .get_list(
                    "binance",
                    &format!(
                        "https://api.binance.com/api/v3/klines?symbol={symbol}&interval=1d&limit={days}"
                    ),
                )
                .map_err(|_| format!("binance klines {symbol} -> error"))?;
            rows.iter().map(|k| num(k.get(4))).collect()
        } else {
            let rows = self
                .get_list(
                    "gate",
                    &format!(
                        "https://api.gateio.ws/api/v4/spot/candlesticks?currency_pair={symbol}&interval=1d&limit={days}"
                    ),
                )
                .map_err(|_| format!("gate candlesticks {symbol} -> error"))?;
            // [ts, quote volume, close, high, low, open, ...]
            rows.iter().map(|k| num(k.get(2))).collect()
        }
    }
}
