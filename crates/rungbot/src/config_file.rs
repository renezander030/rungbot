//! Turning a config file into a validated [`Config`].
//!
//! Every value that fails to parse is a hard error naming the coin and the field, not a
//! silent fallback to a default — a typo in a cron file must never quietly change the
//! strategy.

use std::path::Path;

use std::collections::BTreeMap;

use rungbot_core::{
    Bands, Coin, Config, ConfigError, RegimeConfig, ScreenConfig, SellPolicyConfig, Settings,
    Trail, Venue,
};

use crate::notify::NotifyConfig;

use crate::yaml::{self, Yaml};

pub const EXAMPLE: &str = include_str!("../assets/watchlist.yaml");

fn err<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError(msg.into()))
}

/// Everything the CLI needs: the pure strategy config plus the layers around it.
#[derive(Debug, Clone)]
pub struct CliConfig {
    pub core: Config,
    pub regime: RegimeConfig,
    /// Present when the bull sell policy is configured. `None` = ladder only.
    pub sellpolicy: Option<SellPolicyConfig>,
    pub notify: NotifyConfig,
    /// Per-coin candle source override, `symbol -> "venue:pair"`.
    pub klines: BTreeMap<String, String>,
    pub screen: ScreenConfig,
}

pub fn load(path: &Path) -> Result<CliConfig, ConfigError> {
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

pub fn from_str(text: &str) -> Result<CliConfig, ConfigError> {
    let doc = yaml::parse(text).map_err(ConfigError)?;
    let bands = bands_from(doc.get("bands"), Bands::default())?.unwrap_or_default();
    let settings = settings_from(doc.get("ladder"), bands)?;
    let coins = coins_from(doc.get("coins"))?;
    let core = apply_env(Config::new(coins, settings)?)?;

    Ok(CliConfig {
        regime: regime_from(doc.get("regime"))?,
        sellpolicy: sellpolicy_from(doc.get("sellpolicy"))?,
        notify: notify_from(doc.get("notify"))?,
        klines: klines_from(doc.get("coins")),
        screen: screen_from(doc.get("research"))?,
        core,
    })
}

fn screen_from(node: Option<&Yaml>) -> Result<ScreenConfig, ConfigError> {
    let mut s = ScreenConfig::default();
    let Some(n) = node else { return Ok(s) };
    macro_rules! num_field {
        ($($key:literal => $field:ident),* $(,)?) => {$(
            if let Some(v) = n.get($key) { s.$field = number(v, concat!("research.", $key))? as _; }
        )*};
    }
    num_field! {
        "min_vol_24h"       => min_vol_24h,
        "min_drawdown_pct"  => min_drawdown_pct,
        "max_drawdown_pct"  => max_drawdown_pct,
        "fee_floor_30d"     => fee_floor_30d,
    }
    if let Some(v) = n.get("min_rank") {
        s.min_rank = number(v, "research.min_rank")? as u32;
    }
    if let Some(v) = n.get("max_rank") {
        s.max_rank = number(v, "research.max_rank")? as u32;
    }
    if let Some(v) = n.get("limit") {
        s.limit = number(v, "research.limit")? as usize;
    }
    if s.min_drawdown_pct > s.max_drawdown_pct {
        return err("`research.min_drawdown_pct` cannot exceed max_drawdown_pct");
    }
    if s.min_rank > s.max_rank {
        return err("`research.min_rank` cannot exceed max_rank");
    }
    Ok(s)
}

fn regime_from(node: Option<&Yaml>) -> Result<RegimeConfig, ConfigError> {
    let mut r = RegimeConfig::default();
    let Some(n) = node else { return Ok(r) };
    if let Some(v) = n.get("run_min_signals") {
        let x = number(v, "regime.run_min_signals")?;
        if !(1.0..=4.0).contains(&x) {
            return err("`regime.run_min_signals` must be between 1 and 4");
        }
        r.run_min_signals = x as usize;
    }
    if let Some(v) = n.get("run_ret30_min") {
        r.run_ret30_min = number(v, "regime.run_ret30_min")?;
    }
    Ok(r)
}

fn str_list(node: Option<&Yaml>) -> Vec<String> {
    // A comma-separated scalar, because the config format has no lists.
    node.and_then(|v| v.as_str())
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_ascii_uppercase())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn num_list(node: Option<&Yaml>, what: &str) -> Result<Option<Vec<f64>>, ConfigError> {
    let Some(v) = node.and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for part in v.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        out.push(
            part.parse::<f64>()
                .map_err(|e| ConfigError(format!("`{what}`: {part:?} is not a number: {e}")))?,
        );
    }
    Ok(Some(out))
}

fn sellpolicy_from(node: Option<&Yaml>) -> Result<Option<SellPolicyConfig>, ConfigError> {
    let Some(n) = node else { return Ok(None) };
    // `sellpolicy: off` disables it explicitly.
    if matches!(n, Yaml::Bool(false)) {
        return Ok(None);
    }
    let mut s = SellPolicyConfig::default();
    macro_rules! num_field {
        ($($key:literal => $field:ident),* $(,)?) => {$(
            if let Some(v) = n.get($key) { s.$field = number(v, concat!("sellpolicy.", $key))?; }
        )*};
    }
    num_field! {
        "giveback_pct"         => giveback_pct,
        "giveback_next_pct"    => giveback_next_pct,
        "armed_giveback_pct"   => armed_giveback_pct,
        "core_pct"             => core_pct,
        "tranche_pct"          => tranche_pct,
        "trail_slice_pct"      => trail_slice_pct,
        "trail_slice_next_pct" => trail_slice_next_pct,
        "trail_arm_mult"       => trail_arm_mult,
    }
    if let Some(list) = num_list(n.get("tranches"), "sellpolicy.tranches")? {
        s.tranches = list;
    }
    s.no_tranche = str_list(n.get("no_tranche"));
    s.no_base_trail = str_list(n.get("no_base_trail"));
    if !(0.0..100.0).contains(&s.core_pct) {
        return err("`sellpolicy.core_pct` must be >= 0 and < 100");
    }
    Ok(Some(s))
}

fn notify_from(node: Option<&Yaml>) -> Result<NotifyConfig, ConfigError> {
    let mut c = NotifyConfig::default();
    let Some(n) = node else { return Ok(c) };
    if let Some(u) = n.get("webhook_url").and_then(|v| v.as_str()) {
        if !u.starts_with("https://") && !u.starts_with("http://") {
            return err("`notify.webhook_url` must be an http(s) URL");
        }
        c.webhook_url = Some(u);
    }
    // The bot token is deliberately NOT read from the config: it lives in the
    // environment, so a watchlist stays safe to paste into an issue.
    if let Some(chat) = n.get("telegram_chat_id").and_then(|v| v.as_str()) {
        c.telegram_chat_id = Some(chat);
    }
    Ok(c)
}

fn klines_from(node: Option<&Yaml>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(entries) = node.and_then(|n| n.as_map()) {
        for (sym, spec) in entries {
            if let Some(k) = spec.get("klines").and_then(|v| v.as_str()) {
                out.insert(sym.to_ascii_uppercase(), k);
            }
        }
    }
    out
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
    if let Some(e) = node.flow_error("bands") {
        return err(e);
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
            cfg.core.coins.len() >= 2,
            "the example is a usable starting point"
        );
        assert_eq!(
            cfg.core.settings.trail,
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
        assert_eq!(cfg.core.coins[0].symbol, "BTC", "symbols are upper-cased");
        assert_eq!(
            cfg.core
                .coin("BTC")
                .unwrap()
                .bands_or(cfg.core.settings.bands)
                .first_pct,
            12.0
        );
        assert_eq!(
            cfg.core
                .coin("SOL")
                .unwrap()
                .bands_or(cfg.core.settings.bands)
                .first_pct,
            20.0
        );
        assert_eq!(
            cfg.core.coin("SOL").unwrap().entry,
            None,
            "a coin may have no cost basis"
        );
    }
}
