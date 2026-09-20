//! Turning a config file into a validated [`Config`].
//!
//! Every value that fails to parse is a hard error naming the coin and the field, not a
//! silent fallback to a default — a typo in a cron file must never quietly change the
//! strategy.

use std::path::Path;

use rungbot_core::{Bands, Coin, Config, ConfigError, Settings, Trail, Venue};

use crate::yaml::{self, Yaml};

pub const EXAMPLE: &str = include_str!("../../../examples/watchlist.yaml");

fn err<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError(msg.into()))
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return err(format!(
            "no config at {}. Run `rungbot init` to write a starter one.",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError(format!("cannot read {}: {e}", path.display())))?;
    from_str(&text)
}

pub fn from_str(text: &str) -> Result<Config, ConfigError> {
    let doc = yaml::parse(text).map_err(ConfigError)?;
    let bands = bands_from(doc.get("bands"), Bands::default())?.unwrap_or_default();
    let settings = settings_from(doc.get("ladder"), bands)?;
    let coins = coins_from(doc.get("coins"))?;
    let cfg = Config::new(coins, settings)?;
    apply_env(cfg)
}

fn number(node: &Yaml, what: &str) -> Result<f64, ConfigError> {
    node.as_f64()
        .ok_or_else(|| ConfigError(format!("`{what}` must be a number")))
}

fn bands_from(node: Option<&Yaml>, base: Bands) -> Result<Option<Bands>, ConfigError> {
    let Some(node) = node else { return Ok(None) };
    if node.is_null() {
        return Ok(None);
    }
    if node.as_map().is_none() {
        return err("`bands` must be a mapping with first_pct and step_pct");
    }
    let first = match node.get("first_pct") {
        Some(v) => number(v, "bands.first_pct")?,
        None => base.first_pct,
    };
    let step = match node.get("step_pct") {
        Some(v) => number(v, "bands.step_pct")?,
        None => base.step_pct,
    };
    Bands::new(first, step).map(Some).map_err(ConfigError)
}

fn settings_from(node: Option<&Yaml>, bands: Bands) -> Result<Settings, ConfigError> {
    let mut s = Settings {
        bands,
        ..Settings::default()
    };
    let Some(node) = node else { return Ok(s) };

    macro_rules! num_field {
        ($($key:literal => $field:ident),* $(,)?) => {$(
            if let Some(v) = node.get($key) {
                s.$field = number(v, concat!("ladder.", $key))?;
            }
        )*};
    }
    num_field! {
        "min_trade_pct"      => min_trade_pct,
        "min_core_pct"       => min_core_pct,
        "window_hours"       => window_hours,
        "buy_floor_pct"      => buy_floor_pct,
        "target_pct"         => target_pct,
        "breaker_pct"        => breaker_pct,
        "breaker_days"       => breaker_days,
        "trail_giveback_pct" => trail_giveback_pct,
    }
    if let Some(v) = node.get("trail") {
        let raw = v.as_str().unwrap_or_default();
        s.trail = Trail::parse(&raw)?;
    }
    s.validate()?;
    Ok(s)
}

fn coins_from(node: Option<&Yaml>) -> Result<Vec<Coin>, ConfigError> {
    let Some(entries) = node.and_then(|n| n.as_map()) else {
        return err("config needs a `coins:` mapping with at least one coin");
    };
    let mut coins = Vec::with_capacity(entries.len());
    for (sym, spec) in entries {
        let sym = sym.to_ascii_uppercase();
        if spec.as_map().is_none() {
            return err(format!(
                "coin {sym}: expected a mapping with venue and pair"
            ));
        }
        let venue = spec
            .get("venue")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ConfigError(format!("coin {sym}: `venue` is required")))?;
        let venue = Venue::parse(&venue).map_err(|e| ConfigError(format!("coin {sym}: {e}")))?;

        let pair = spec
            .get("pair")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if pair.trim().is_empty() {
            return err(format!(
                "coin {sym}: `pair` is required (the venue's own symbol)"
            ));
        }

        let entry = match spec.get("entry") {
            Some(v) if !v.is_null() => Some(
                v.as_f64()
                    .ok_or_else(|| ConfigError(format!("coin {sym}: `entry` must be a number")))?,
            ),
            _ => None,
        };

        let bands = match spec.get("bands") {
            Some(b) if !b.is_null() => bands_from(Some(b), Bands::default())
                .map_err(|e| ConfigError(format!("coin {sym}: {e}")))?,
            _ => None,
        };

        coins.push(Coin {
            name: spec
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| sym.clone()),
            symbol: sym,
            venue,
            pair,
            entry,
            bands,
        });
    }
    Ok(coins)
}

/// Environment overrides, for CI and one-off experiments.
///
/// `RUNGBOT_FIRST_PCT`, `RUNGBOT_MIN_CORE_PCT`, ... map onto the matching field. A value
/// that does not parse is fatal.
fn apply_env(cfg: Config) -> Result<Config, ConfigError> {
    let mut s = cfg.settings;

    fn env_num(key: &str) -> Result<Option<f64>, ConfigError> {
        match std::env::var(key) {
            Ok(v) => v
                .trim()
                .parse::<f64>()
                .map(Some)
                .map_err(|e| ConfigError(format!("{key}: {e}"))),
            Err(_) => Ok(None),
        }
    }

    if let Some(v) = env_num("RUNGBOT_FIRST_PCT")? {
        s.bands = Bands::new(v, s.bands.step_pct).map_err(ConfigError)?;
    }
    if let Some(v) = env_num("RUNGBOT_STEP_PCT")? {
        s.bands = Bands::new(s.bands.first_pct, v).map_err(ConfigError)?;
    }
    macro_rules! env_field {
        ($($key:literal => $field:ident),* $(,)?) => {$(
            if let Some(v) = env_num($key)? { s.$field = v; }
        )*};
    }
    env_field! {
        "RUNGBOT_MIN_TRADE_PCT"      => min_trade_pct,
        "RUNGBOT_MIN_CORE_PCT"       => min_core_pct,
        "RUNGBOT_WINDOW_HOURS"       => window_hours,
        "RUNGBOT_BUY_FLOOR_PCT"      => buy_floor_pct,
        "RUNGBOT_TARGET_PCT"         => target_pct,
        "RUNGBOT_BREAKER_PCT"        => breaker_pct,
        "RUNGBOT_BREAKER_DAYS"       => breaker_days,
        "RUNGBOT_TRAIL_GIVEBACK_PCT" => trail_giveback_pct,
    }
    if let Ok(v) = std::env::var("RUNGBOT_TRAIL") {
        s.trail =
            Trail::parse(&v).map_err(|_| ConfigError("RUNGBOT_TRAIL must be off or on".into()))?;
    }
    s.validate()?;
    Config::new(cfg.coins, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_example_loads() {
        let cfg = from_str(EXAMPLE).expect("the config `rungbot init` writes must parse");
        assert!(
            cfg.coins.len() >= 2,
            "the example is a usable starting point"
        );
        assert_eq!(
            cfg.settings.trail,
            Trail::Off,
            "`trail: off` survives YAML 1.1"
        );
    }

    #[test]
    fn errors_name_the_coin_and_the_field() {
        let cases = [
            ("coins:\n", "at least one coin"),
            (
                "coins:\n  BTC:\n    venue: kraken\n    pair: X\n",
                "venue must be one of",
            ),
            ("coins:\n  BTC:\n    venue: binance\n", "`pair` is required"),
            (
                "coins:\n  BTC:\n    venue: binance\n    pair: X\n    entry: soon\n",
                "must be a number",
            ),
            (
                "coins:\n  BTC:\n    venue: binance\n    pair: X\n    entry: -1\n",
                "must be > 0",
            ),
            (
                "ladder:\n  trail: maybe\ncoins:\n  B:\n    venue: gate\n    pair: X\n",
                "must be off or on",
            ),
            (
                "ladder:\n  min_core_pct: 100\ncoins:\n  B:\n    venue: gate\n    pair: X\n",
                ">= 0 and < 100",
            ),
            (
                "bands:\n  first_pct: 0\ncoins:\n  B:\n    venue: gate\n    pair: X\n",
                "first_pct",
            ),
        ];
        for (src, needle) in cases {
            let e = from_str(src).expect_err(&format!("should reject: {src:?}"));
            assert!(e.0.contains(needle), "message {:?} lacks {needle:?}", e.0);
        }
    }

    #[test]
    fn per_coin_overrides_and_inheritance() {
        let cfg = from_str(
            "bands:\n  first_pct: 12\n  step_pct: 6\ncoins:\n  btc:\n    venue: binance\n    pair: BTCUSDT\n  sol:\n    venue: gate\n    pair: SOL_USDT\n    bands:\n      first_pct: 20\n      step_pct: 10\n",
        )
        .unwrap();
        assert_eq!(cfg.coins[0].symbol, "BTC", "symbols are upper-cased");
        assert_eq!(
            cfg.coin("BTC")
                .unwrap()
                .bands_or(cfg.settings.bands)
                .first_pct,
            12.0
        );
        assert_eq!(
            cfg.coin("SOL")
                .unwrap()
                .bands_or(cfg.settings.bands)
                .first_pct,
            20.0
        );
        assert_eq!(
            cfg.coin("SOL").unwrap().entry,
            None,
            "a coin may have no cost basis"
        );
    }
}
