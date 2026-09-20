//! The watchlist and the ladder's tunables — types and validation only.
//!
//! Nothing about a particular portfolio lives in the code. A coin, its venue, its pair
//! and its optional cost basis are all configuration. The strategy is general; the book
//! is yours.
//!
//! This module deliberately does no file I/O: parsing a config file belongs to the CLI,
//! so the core stays clean for `wasm32`, where there is no filesystem.

use serde::{Deserialize, Serialize};

use crate::ladder::Bands;

/// A config a human needs to fix, with a message that says how.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigError(pub String);

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError(msg.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Venue {
    Binance,
    Gate,
    /// Revolut X — the EEA/UK venue. Pairs look like `BTC/USD`.
    Revx,
    Coingecko,
}

impl Venue {
    pub const ALL: [&'static str; 4] = ["binance", "gate", "revx", "coingecko"];

    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        match s.to_ascii_lowercase().as_str() {
            "binance" => Ok(Venue::Binance),
            "gate" => Ok(Venue::Gate),
            // Accept the spellings a human would reach for, canonicalise to `revx`.
            "revx" | "revolutx" | "revolut-x" | "revolut" => Ok(Venue::Revx),
            "coingecko" => Ok(Venue::Coingecko),
            other => Err(ConfigError(format!(
                "venue must be one of {}, got {other:?}",
                Venue::ALL.join(", ")
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Venue::Binance => "binance",
            Venue::Gate => "gate",
            Venue::Revx => "revx",
            Venue::Coingecko => "coingecko",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coin {
    pub symbol: String,
    pub venue: Venue,
    /// The venue's own symbol: Binance `BTCUSDT`, Gate `BTC_USDT`, CoinGecko `bitcoin`.
    pub pair: String,
    #[serde(default)]
    pub name: String,
    /// Cost basis in the quote currency. `None` means the coin is watched for dips only:
    /// rungbot will not suggest selling something it has no basis for.
    #[serde(default)]
    pub entry: Option<f64>,
    /// Per-coin override of the global bands.
    #[serde(default)]
    pub bands: Option<Bands>,
}

impl Coin {
    pub fn bands_or(&self, default: Bands) -> Bands {
        self.bands.unwrap_or(default)
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.symbol
        } else {
            &self.name
        }
    }
}

/// How the sell side behaves on the way up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trail {
    /// Fire every newly crossed rung immediately. The backtested default.
    Off,
    /// Lock in rung 1, then hold the upper rungs while the move runs.
    On,
}

impl Trail {
    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        match s.trim().to_ascii_lowercase().as_str() {
            // YAML 1.1 turns a bare `off` into the boolean false, so both spellings
            // arrive here depending on which reader produced the value.
            "off" | "false" | "no" => Ok(Trail::Off),
            "on" | "true" | "yes" => Ok(Trail::On),
            other => Err(ConfigError(format!(
                "`ladder.trail` must be off or on, got {other:?}"
            ))),
        }
    }
}

/// Every knob the ladder reads. Defaults are the conservative, notify-only ones.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub bands: Bands,
    /// Don't emit a trade smaller than this.
    pub min_trade_pct: f64,
    /// Never sell below this % of the position.
    pub min_core_pct: f64,
    /// After acting, a coin is locked to that direction for this long.
    pub window_hours: f64,
    /// Stop dip-buying past this 24h drop.
    pub buy_floor_pct: f64,
    /// Take-profit target shown in the report.
    pub target_pct: f64,
    /// Freeze dip-buys on a coin this far underwater. 0 disables the breaker.
    pub breaker_pct: f64,
    pub breaker_days: f64,
    pub trail: Trail,
    pub trail_giveback_pct: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            bands: Bands::default(),
            min_trade_pct: 1.0,
            min_core_pct: 20.0,
            window_hours: 24.0,
            buy_floor_pct: 50.0,
            target_pct: 10.0,
            breaker_pct: 40.0,
            breaker_days: 7.0,
            trail: Trail::Off,
            trail_giveback_pct: 5.0,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.min_core_pct.is_nan() || self.min_core_pct < 0.0 || self.min_core_pct >= 100.0 {
            return err("`ladder.min_core_pct` must be >= 0 and < 100");
        }
        Bands::new(self.bands.first_pct, self.bands.step_pct).map_err(ConfigError)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Order matters: it is the report's tie-break order.
    pub coins: Vec<Coin>,
    pub settings: Settings,
}

impl Config {
    pub fn new(coins: Vec<Coin>, settings: Settings) -> Result<Self, ConfigError> {
        if coins.is_empty() {
            return err("config needs a `coins:` mapping with at least one coin");
        }
        settings.validate()?;
        for c in &coins {
            if c.pair.trim().is_empty() {
                return err(format!(
                    "coin {}: `pair` is required (the venue's own symbol)",
                    c.symbol
                ));
            }
            if let Some(e) = c.entry {
                if e.is_nan() || e <= 0.0 {
                    return err(format!("coin {}: `entry` must be > 0", c.symbol));
                }
            }
            if let Some(b) = c.bands {
                Bands::new(b.first_pct, b.step_pct)
                    .map_err(|m| ConfigError(format!("coin {}: {m}", c.symbol)))?;
            }
        }
        Ok(Config { coins, settings })
    }

    pub fn coin(&self, symbol: &str) -> Option<&Coin> {
        self.coins.iter().find(|c| c.symbol == symbol)
    }
}
