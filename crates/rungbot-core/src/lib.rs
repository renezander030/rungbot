//! `rungbot-core` — the ladder, as pure logic.
//!
//! This crate contains the entire strategy and nothing else. It reads no clock, opens no
//! file, makes no network call and generates no randomness, so it compiles unchanged to
//! `wasm32-unknown-unknown` for a Cloudflare Worker and to a native binary for the CLI.
//! The strategy therefore exists in exactly one place.
//!
//! ```
//! use std::collections::BTreeMap;
//! use rungbot_core::{analyze, Coin, Config, Price, Settings, Venue};
//!
//! let cfg = Config::new(
//!     vec![Coin {
//!         symbol: "BTC".into(),
//!         venue: Venue::Binance,
//!         pair: "BTCUSDT".into(),
//!         name: "Bitcoin".into(),
//!         entry: Some(61_000.0),
//!         bands: None,
//!     }],
//!     Settings::default(),
//! )
//! .unwrap();
//!
//! let mut prices = BTreeMap::new();
//! prices.insert("BTC".to_string(), Price { price: 54_900.0, chg_24h: Some(-12.0) });
//!
//! let out = analyze(&cfg, &prices, &BTreeMap::new(), 1_700_000_000.0);
//! assert_eq!(out.buys.len(), 1);
//! assert_eq!(out.buys[0].rung, 1);
//! ```

#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod analyze;
pub mod config;
pub mod ladder;

pub use analyze::{analyze, CoinState, Outcome, Price, Row, Side, State, Trade};
pub use config::{Coin, Config, ConfigError, Settings, Trail, Venue};
pub use ladder::{buy_rung_for, ladder_increment, rung_threshold, sell_rung_for, Bands};

/// The crate version, so a Worker and a binary can report the same number.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
