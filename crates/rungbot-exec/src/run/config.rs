//! The run's one config file: every knob `rungbot-exec run` reads, in one YAML file.
//!
//! Each top-level key is one knob, named after the environment variable that can
//! override it (`first_pct` ↔ `FIRST_PCT`). A key left out takes its default; the
//! defaults are the conservative ones (notify only, nothing armed). The documented
//! example in `contrib/rungbot-run.example.yaml` sets a live profile.
//!
//! Precedence, lowest first: the default, the YAML file, the environment variable.
//! Map-valued knobs (`bands`, `sell_giveback`, `sell_trail_arm`, `deploy_alloc`,
//! `deploy_zones`) take a JSON object from their `*_JSON` variable.
//!
//! The four coin tables are separate on purpose, because their orders differ and both
//! orders are behaviour:
//!
//! * `watchlist` (symbol → the id errors print) is the report order and analysis order.
//! * `routing` (symbol → `venue pair quote`) is the order holdings, drift and the
//!   regime read coins in.
//! * `names` and `entries` (seed cost basis) are lookups.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rungbot_core::yaml::{self, Yaml};
use serde_json::Value;

/// One coin's route: where it trades and in which quote asset.
#[derive(Debug, Clone, PartialEq)]
pub struct CoinRoute {
    pub exch: String,
    pub pair: String,
    pub quote: String,
}

/// A deploy zone override: depths below spot and the weight of each, in percent.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Zone {
    pub depths: Vec<f64>,
    pub weights: Vec<f64>,
}

/// Every knob of one run. Field names are the snake_case of the env variable.
#[derive(Debug, Clone, PartialEq)]
pub struct RunConfig {
    // ---- mode and rails
    /// `off` (notify only), `dry` (plan orders, place none) or `live`.
    pub trade_mode: String,
    /// The second switch: live orders place only when this is on.
    pub live_trading_enabled: bool,
    /// While this file exists, no live order is placed.
    pub halt_file: PathBuf,
    pub max_slippage_pct: f64,
    pub max_order_usd: f64,
    pub max_daily_notional: f64,
    pub max_daily_orders: i64,
    pub max_weekly_notional: f64,
    pub usdc_bag_usd: f64,

    // ---- ladder
    pub first_pct: f64,
    pub step_pct: f64,
    pub target_pct: f64,
    pub min_trade_pct: f64,
    pub min_core_pct: f64,
    pub window_hours: f64,
    pub buy_floor_pct: f64,
    pub breaker_pct: f64,
    pub breaker_days: f64,
    /// `off`, `on` or `auto` (trail the coins the regime flags as running).
    pub trail_tp: String,
    pub trail_giveback_pct: f64,
    /// Per-coin `[first_pct, step_pct]` overrides.
    pub bands: BTreeMap<String, (f64, f64)>,

    // ---- housekeeping
    pub limit_ttl_days: f64,
    pub limit_ttl_days_bear: f64,
    pub limit_ttl_days_chop: f64,
    pub ttl_renag_days: f64,
    pub fee_pct_binance: f64,
    pub fee_pct_gate: f64,
    pub fee_pct_revx: f64,

    // ---- the macro level alert (0 disables a band)
    pub btc_alert_usd: f64,
    pub btc_warn_usd: f64,
    /// What the alert calls the level, e.g. `the flip line`.
    pub btc_line_name: String,

    // ---- bull sell policy
    /// `auto`, `ladder` or `bull`.
    pub sell_policy: String,
    pub sell_giveback_pct: f64,
    pub sell_giveback: BTreeMap<String, f64>,
    pub sell_armed_giveback_pct: f64,
    pub sell_core_pct: f64,
    pub sell_tranches: Vec<f64>,
    pub sell_tranche_pct: f64,
    pub sell_no_tranche: Vec<String>,
    pub sell_no_base_trail: Vec<String>,
    pub sell_trail_slice_pct: f64,
    pub sell_giveback_next_pct: f64,
    pub sell_trail_slice_next_pct: f64,
    pub sell_trail_arm_mult: f64,
    pub sell_trail_arm: BTreeMap<String, f64>,
    pub sell_arm_all: bool,
    pub sell_arm_file: PathBuf,

    // ---- regime
    pub regime_ttl_h: f64,
    pub regime_hist_ttl_h: f64,
    pub regime_hist_days: usize,
    pub regime_confirm_days: i64,
    pub run_min_signals: usize,
    pub run_ret30_min: f64,
    /// Revolut X pairs read their daily candles from a deeper venue: `pair → venue symbol`.
    pub regime_kline_source: BTreeMap<String, (String, String)>,
    /// The market proxy's daily candles (Binance symbol).
    pub regime_market_symbol: String,

    // ---- dedupe, logs, housekeeping of files
    pub signal_remind_s: f64,
    pub decisions_max_mb: f64,
    pub order_archive_days: f64,
    pub run_lock_wait: f64,
    /// The run's name in the error mail: `<name> ERROR (n)`.
    pub mail_name: String,
    /// Where the error mail says the log is.
    pub log_hint: String,

    // ---- deploy (read by the deploy layer)
    pub deploy: String,
    pub deploy_min_usd: f64,
    pub deploy_max_tranche_usd: f64,
    pub deploy_max_depth_pct: f64,
    pub deploy_pin_prices: bool,
    pub deploy_sweep_idle: bool,
    pub deploy_bull_sweep: bool,
    pub deploy_bull_sweep_days: f64,
    pub deploy_bull_sweep_max_age: f64,
    pub deploy_alloc: Vec<(String, f64)>,
    pub deploy_zones: BTreeMap<String, Zone>,
    /// Coins listed on Revolut X but routed elsewhere (symbol → pair): the deploy layer
    /// ladders the onramp venue over these while no coin is routed there.
    pub revx_pairs: Vec<(String, String)>,
    /// A resting buy rung under these odds of filling (at the longest horizon) counts as
    /// sidelined cash in `rungbot-exec fillodds`.
    pub fillodds_low: f64,

    // ---- coins
    pub watchlist: Vec<(String, String)>,
    pub names: BTreeMap<String, String>,
    pub entries: BTreeMap<String, f64>,
    pub routing: Vec<(String, CoinRoute)>,

    // ---- files
    /// Where every state file lives.
    pub state_dir: PathBuf,
    /// Overrides for single files; `None` = the default name in `state_dir`.
    pub state_path: Option<PathBuf>,
    pub order_journal: Option<PathBuf>,
    pub pnl_ledger: Option<PathBuf>,
    pub ttl_warn_state: Option<PathBuf>,
    pub decisions_log: Option<PathBuf>,
    pub signal_notices: Option<PathBuf>,
    pub btc_alert_state: Option<PathBuf>,
    pub regime_state: Option<PathBuf>,
    pub froth_state: Option<PathBuf>,
    pub run_lock: Option<PathBuf>,
    pub deploy_state: Option<PathBuf>,
    pub audit_state: Option<PathBuf>,
    /// Cached daily candles for `rungbot-exec fillodds` (`binance_{SYM}USDT.json`,
    /// `gate_{SYM}_USDT.json`).
    pub fillodds_cache: Option<PathBuf>,

    /// The `notify:` block as JSON, for [`rungbot_notify::Notifier`].
    pub notify: Value,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// `$XDG_STATE_HOME/rungbot`, else `~/.local/state/rungbot`.
pub fn default_state_dir() -> PathBuf {
    match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v).join("rungbot"),
        _ => home().join(".local").join("state").join("rungbot"),
    }
}

/// `$XDG_CONFIG_HOME/rungbot`, else `~/.config/rungbot`.
pub fn default_config_dir() -> PathBuf {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v).join("rungbot"),
        _ => home().join(".config").join("rungbot"),
    }
}

/// `~/x` → `$HOME/x`.
pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if p == "~" => home(),
        None => PathBuf::from(p),
    }
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            trade_mode: "off".into(),
            live_trading_enabled: false,
            halt_file: default_config_dir().join("HALT"),
            max_slippage_pct: 2.0,
            max_order_usd: 50.0,
            max_daily_notional: 200.0,
            max_daily_orders: 10,
            max_weekly_notional: 500.0,
            usdc_bag_usd: 0.0,
            first_pct: 10.0,
            step_pct: 5.0,
            target_pct: 10.0,
            min_trade_pct: 1.0,
            min_core_pct: 20.0,
            window_hours: 24.0,
            buy_floor_pct: 50.0,
            breaker_pct: 40.0,
            breaker_days: 7.0,
            trail_tp: "off".into(),
            trail_giveback_pct: 5.0,
            bands: BTreeMap::new(),
            limit_ttl_days: 14.0,
            limit_ttl_days_bear: 45.0,
            limit_ttl_days_chop: 30.0,
            ttl_renag_days: 30.0,
            fee_pct_binance: 0.1,
            fee_pct_gate: 0.2,
            fee_pct_revx: 0.1,
            btc_alert_usd: 0.0,
            btc_warn_usd: 0.0,
            btc_line_name: "the flip line".into(),
            sell_policy: "auto".into(),
            sell_giveback_pct: 40.0,
            sell_giveback: BTreeMap::new(),
            sell_armed_giveback_pct: 15.0,
            sell_core_pct: 20.0,
            sell_tranches: vec![4.0, 8.0, 16.0, 32.0],
            sell_tranche_pct: 20.0,
            sell_no_tranche: Vec::new(),
            sell_no_base_trail: Vec::new(),
            sell_trail_slice_pct: 25.0,
            sell_giveback_next_pct: 20.0,
            sell_trail_slice_next_pct: 0.0,
            sell_trail_arm_mult: 2.0,
            sell_trail_arm: BTreeMap::new(),
            sell_arm_all: false,
            sell_arm_file: default_config_dir().join("sell-armed"),
            regime_ttl_h: 6.0,
            regime_hist_ttl_h: 12.0,
            regime_hist_days: 420,
            regime_confirm_days: 14,
            run_min_signals: 3,
            run_ret30_min: 25.0,
            regime_kline_source: BTreeMap::new(),
            regime_market_symbol: "BTCUSDT".into(),
            signal_remind_s: 24.0 * 3600.0,
            decisions_max_mb: 20.0,
            order_archive_days: 30.0,
            run_lock_wait: 300.0,
            mail_name: "rungbot".into(),
            log_hint: "journalctl -u rungbot-run".into(),
            deploy: "off".into(),
            deploy_min_usd: 25.0,
            deploy_max_tranche_usd: 1000.0,
            deploy_max_depth_pct: 20.0,
            deploy_pin_prices: true,
            deploy_sweep_idle: true,
            deploy_bull_sweep: false,
            deploy_bull_sweep_days: 14.0,
            deploy_bull_sweep_max_age: 120.0,
            deploy_alloc: Vec::new(),
            deploy_zones: BTreeMap::new(),
            revx_pairs: Vec::new(),
            fillodds_low: 0.30,
            watchlist: Vec::new(),
            names: BTreeMap::new(),
            entries: BTreeMap::new(),
            routing: Vec::new(),
            state_dir: default_state_dir(),
            state_path: None,
            order_journal: None,
            pnl_ledger: None,
            ttl_warn_state: None,
            decisions_log: None,
            signal_notices: None,
            btc_alert_state: None,
            regime_state: None,
            froth_state: None,
            run_lock: None,
            deploy_state: None,
            audit_state: None,
            fillodds_cache: None,
            notify: Value::Null,
        }
    }
}

/// One knob's raw value, from YAML or from the environment.
enum Raw<'a> {
    Yaml(&'a Yaml),
    Env(String),
}

impl Raw<'_> {
    fn num(&self, key: &str) -> Result<f64, String> {
        match self {
            Raw::Yaml(y) => y
                .as_f64()
                .ok_or_else(|| format!("`{key}` must be a number")),
            Raw::Env(s) => s
                .trim()
                .parse()
                .map_err(|_| format!("{}={s:?} is not a number", key.to_uppercase())),
        }
    }

    /// A word. YAML 1.1 reads a bare `off`/`on` as a boolean; both come back as words.
    fn word(&self, key: &str) -> Result<String, String> {
        match self {
            Raw::Yaml(Yaml::Bool(true)) => Ok("on".into()),
            Raw::Yaml(Yaml::Bool(false)) => Ok("off".into()),
            Raw::Yaml(Yaml::Null) => Ok(String::new()),
            Raw::Yaml(y) => y.as_str().ok_or_else(|| format!("`{key}` must be a word")),
            Raw::Env(s) => Ok(s.clone()),
        }
    }

    /// A yes/no switch: `yes`, `true`, `on` and `1` are yes.
    fn flag(&self, key: &str) -> Result<bool, String> {
        match self {
            Raw::Yaml(Yaml::Bool(b)) => Ok(*b),
            _ => Ok(matches!(
                self.word(key)?.trim().to_ascii_lowercase().as_str(),
                "yes" | "true" | "on" | "1"
            )),
        }
    }

    /// A comma list, `4,8,16,32`.
    fn list(&self, key: &str) -> Result<Vec<String>, String> {
        let s = match self {
            Raw::Yaml(Yaml::Num(n)) => rungbot_core::fmt::py_g(*n, 17),
            _ => self.word(key)?,
        };
        Ok(s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect())
    }

    /// A mapping: a YAML block, or a JSON object in the env variable.
    fn map(&self, key: &str) -> Result<Vec<(String, Value)>, String> {
        match self {
            Raw::Yaml(Yaml::Map(m)) => Ok(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
            Raw::Yaml(Yaml::Null) => Ok(Vec::new()),
            Raw::Yaml(y) => Err(y
                .flow_error(key)
                .unwrap_or_else(|| format!("`{key}` must be a mapping"))),
            Raw::Env(s) if s.trim().is_empty() => Ok(Vec::new()),
            Raw::Env(s) => {
                // Keep the variable's key order, as the reference kept its dict order.
                let v: indexmap::IndexMap<String, Value> = serde_json::from_str(s)
                    .map_err(|e| format!("{}_JSON: {e}", key.to_uppercase()))?;
                Ok(v.into_iter().collect())
            }
        }
    }

    fn path(&self, key: &str) -> Result<PathBuf, String> {
        Ok(expand(&self.word(key)?))
    }
}

/// A YAML value as JSON: maps keep their order in a `serde_json` map (sorted), scalars map
/// one to one.
pub fn to_json(y: &Yaml) -> Value {
    match y {
        Yaml::Str(s) | Yaml::Flow(s) => Value::String(s.clone()),
        Yaml::Num(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Yaml::Bool(b) => Value::Bool(*b),
        Yaml::Null => Value::Null,
        Yaml::Map(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
    }
}

fn num_of(v: &Value, what: &str) -> Result<f64, String> {
    match v {
        Value::Number(n) => n.as_f64().ok_or_else(|| format!("{what}: not a number")),
        Value::String(s) => s
            .trim()
            .parse()
            .map_err(|_| format!("{what}: not a number")),
        _ => Err(format!("{what}: not a number")),
    }
}

fn nums_of(v: &Value, what: &str) -> Result<Vec<f64>, String> {
    match v {
        Value::Array(a) => a.iter().map(|x| num_of(x, what)).collect(),
        Value::String(s) => s
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .filter(|x| !x.trim().is_empty())
            .map(|x| {
                x.trim()
                    .parse()
                    .map_err(|_| format!("{what}: not a list of numbers"))
            })
            .collect(),
        Value::Number(_) => Ok(vec![num_of(v, what)?]),
        _ => Err(format!("{what}: expected a list of numbers")),
    }
}

impl RunConfig {
    /// Read `path`, then apply the environment.
    pub fn load(path: &Path) -> Result<RunConfig, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::from_yaml(&text, &|k| std::env::var(k).ok())
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Build from YAML text and an environment lookup.
    pub fn from_yaml(
        text: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<RunConfig, String> {
        let doc = yaml::parse(text)?;
        let entries = doc.as_map().unwrap_or(&[]);
        let mut c = RunConfig::default();
        for (k, v) in entries {
            c.set(k, Raw::Yaml(v))?;
        }
        for key in ENV_KNOBS {
            let var = env_name(key);
            if let Some(v) = env(&var) {
                c.set(key, Raw::Env(v))?;
            }
        }
        c.validate()?;
        Ok(c)
    }

    fn set(&mut self, key: &str, v: Raw) -> Result<(), String> {
        match key {
            "trade_mode" => self.trade_mode = v.word(key)?.to_lowercase(),
            // Real money: only an explicit yes arms it, as the reference read it.
            "live_trading_enabled" => {
                self.live_trading_enabled = match &v {
                    Raw::Yaml(Yaml::Bool(b)) => *b,
                    _ => v.word(key)?.trim().eq_ignore_ascii_case("yes"),
                }
            }
            "halt_file" => self.halt_file = v.path(key)?,
            "max_slippage_pct" => self.max_slippage_pct = v.num(key)?,
            "max_order_usd" => self.max_order_usd = v.num(key)?,
            "max_daily_notional" => self.max_daily_notional = v.num(key)?,
            "max_daily_orders" => self.max_daily_orders = v.num(key)? as i64,
            "max_weekly_notional" => self.max_weekly_notional = v.num(key)?,
            "usdc_bag_usd" => self.usdc_bag_usd = v.num(key)?,
            "first_pct" => self.first_pct = v.num(key)?,
            "step_pct" => self.step_pct = v.num(key)?,
            "target_pct" => self.target_pct = v.num(key)?,
            "min_trade_pct" => self.min_trade_pct = v.num(key)?,
            "min_core_pct" => self.min_core_pct = v.num(key)?,
            "window_hours" => self.window_hours = v.num(key)?,
            "buy_floor_pct" => self.buy_floor_pct = v.num(key)?,
            "breaker_pct" => self.breaker_pct = v.num(key)?,
            "breaker_days" => self.breaker_days = v.num(key)?,
            "trail_tp" => self.trail_tp = v.word(key)?.to_lowercase(),
            "trail_giveback_pct" => self.trail_giveback_pct = v.num(key)?,
            "bands" => {
                // A value that does not read is ignored with a warning, as the reference did.
                self.bands = match parse_bands(&v, key) {
                    Ok(b) => b,
                    Err(_) => {
                        eprintln!("WARN: BANDS_JSON unparseable, ignoring");
                        BTreeMap::new()
                    }
                }
            }
            "limit_ttl_days" => self.limit_ttl_days = v.num(key)?,
            "limit_ttl_days_bear" => self.limit_ttl_days_bear = v.num(key)?,
            "limit_ttl_days_chop" => self.limit_ttl_days_chop = v.num(key)?,
            "ttl_renag_days" => self.ttl_renag_days = v.num(key)?,
            "fee_pct_binance" => self.fee_pct_binance = v.num(key)?,
            "fee_pct_gate" => self.fee_pct_gate = v.num(key)?,
            "fee_pct_revx" => self.fee_pct_revx = v.num(key)?,
            "btc_alert_usd" => self.btc_alert_usd = v.num(key)?,
            "btc_warn_usd" => self.btc_warn_usd = v.num(key)?,
            "btc_line_name" => self.btc_line_name = v.word(key)?,
            "sell_policy" => self.sell_policy = v.word(key)?.trim().to_lowercase(),
            "sell_giveback_pct" => self.sell_giveback_pct = v.num(key)?,
            "sell_giveback" => self.sell_giveback = upper_num_map(&v, key),
            "sell_armed_giveback_pct" => self.sell_armed_giveback_pct = v.num(key)?,
            "sell_core_pct" => self.sell_core_pct = v.num(key)?,
            "sell_tranches" => {
                self.sell_tranches = v
                    .list(key)?
                    .iter()
                    .map(|x| {
                        x.parse()
                            .map_err(|_| format!("`{key}`: {x:?} is not a number"))
                    })
                    .collect::<Result<_, _>>()?
            }
            "sell_tranche_pct" => self.sell_tranche_pct = v.num(key)?,
            "sell_no_tranche" => self.sell_no_tranche = upper_list(&v, key)?,
            "sell_no_base_trail" => self.sell_no_base_trail = upper_list(&v, key)?,
            "sell_trail_slice_pct" => self.sell_trail_slice_pct = v.num(key)?,
            "sell_giveback_next_pct" => self.sell_giveback_next_pct = v.num(key)?,
            "sell_trail_slice_next_pct" => self.sell_trail_slice_next_pct = v.num(key)?,
            "sell_trail_arm_mult" => self.sell_trail_arm_mult = v.num(key)?,
            "sell_trail_arm" => self.sell_trail_arm = upper_num_map(&v, key),
            "sell_arm_all" => self.sell_arm_all = v.flag(key)?,
            "sell_arm_file" => self.sell_arm_file = v.path(key)?,
            "regime_ttl_h" => self.regime_ttl_h = v.num(key)?,
            "regime_hist_ttl_h" => self.regime_hist_ttl_h = v.num(key)?,
            "regime_hist_days" => self.regime_hist_days = v.num(key)? as usize,
            "regime_confirm_days" => self.regime_confirm_days = v.num(key)? as i64,
            "run_min_signals" => self.run_min_signals = v.num(key)? as usize,
            "run_ret30_min" => self.run_ret30_min = v.num(key)?,
            "regime_kline_source" => {
                let mut m = BTreeMap::new();
                for (pair, src) in v.map(key)? {
                    let s = src.as_str().unwrap_or_default().to_string();
                    let mut it = s.split_whitespace();
                    match (it.next(), it.next()) {
                        (Some(exch), Some(sym)) => {
                            m.insert(pair, (exch.to_string(), sym.to_string()));
                        }
                        _ => {
                            return Err(format!(
                                "`{key}.{pair}` must be `<venue> <symbol>`, got {s:?}"
                            ))
                        }
                    }
                }
                self.regime_kline_source = m;
            }
            "regime_market_symbol" => self.regime_market_symbol = v.word(key)?,
            "signal_remind_s" => self.signal_remind_s = v.num(key)?,
            "decisions_max_mb" => self.decisions_max_mb = v.num(key)?,
            "order_archive_days" => self.order_archive_days = v.num(key)?,
            "run_lock_wait" => self.run_lock_wait = v.num(key)?,
            "mail_name" => self.mail_name = v.word(key)?,
            "log_hint" => self.log_hint = v.word(key)?,
            "deploy" => self.deploy = v.word(key)?.to_lowercase(),
            "deploy_min_usd" => self.deploy_min_usd = v.num(key)?,
            "deploy_max_tranche_usd" => self.deploy_max_tranche_usd = v.num(key)?,
            "deploy_max_depth_pct" => self.deploy_max_depth_pct = v.num(key)?,
            "deploy_pin_prices" => self.deploy_pin_prices = v.flag(key)?,
            "deploy_sweep_idle" => self.deploy_sweep_idle = v.flag(key)?,
            "deploy_bull_sweep" => self.deploy_bull_sweep = v.flag(key)?,
            "deploy_bull_sweep_days" => self.deploy_bull_sweep_days = v.num(key)?,
            "deploy_bull_sweep_max_age" => self.deploy_bull_sweep_max_age = v.num(key)?,
            "deploy_alloc" => {
                self.deploy_alloc = v
                    .map(key)?
                    .into_iter()
                    .map(|(k, x)| Ok((k.clone(), num_of(&x, &format!("{key}.{k}"))?)))
                    .collect::<Result<_, String>>()?
            }
            "deploy_zones" => {
                let mut m = BTreeMap::new();
                for (sym, z) in v.map(key)? {
                    let what = format!("{key}.{sym}");
                    let depths = z.get("depths").map(|d| nums_of(d, &what)).transpose()?;
                    let weights = z.get("weights").map(|d| nums_of(d, &what)).transpose()?;
                    m.insert(
                        sym,
                        Zone {
                            depths: depths.unwrap_or_default(),
                            weights: weights.unwrap_or_default(),
                        },
                    );
                }
                self.deploy_zones = m;
            }
            "revx_pairs" => {
                self.revx_pairs = v
                    .map(key)?
                    .into_iter()
                    .map(|(k, x)| (k, x.as_str().map(str::to_string).unwrap_or_default()))
                    .collect()
            }
            "fillodds_low" => self.fillodds_low = v.num(key)?,
            "watchlist" => {
                self.watchlist = v
                    .map(key)?
                    .into_iter()
                    .map(|(k, x)| (k, x.as_str().map(str::to_string).unwrap_or_default()))
                    .collect()
            }
            "names" => {
                self.names = v
                    .map(key)?
                    .into_iter()
                    .map(|(k, x)| (k, x.as_str().map(str::to_string).unwrap_or_default()))
                    .collect()
            }
            "entries" => {
                self.entries = v
                    .map(key)?
                    .into_iter()
                    .map(|(k, x)| Ok((k.clone(), num_of(&x, &format!("{key}.{k}"))?)))
                    .collect::<Result<_, String>>()?
            }
            "routing" => {
                let mut out = Vec::new();
                for (sym, r) in v.map(key)? {
                    let s = r.as_str().unwrap_or_default().to_string();
                    let parts: Vec<&str> = s.split_whitespace().collect();
                    let [exch, pair, quote] = parts[..] else {
                        return Err(format!(
                            "`routing.{sym}` must be `<venue> <pair> <quote>`, got {s:?}"
                        ));
                    };
                    out.push((
                        sym,
                        CoinRoute {
                            exch: exch.into(),
                            pair: pair.into(),
                            quote: quote.into(),
                        },
                    ));
                }
                self.routing = out;
            }
            "state_dir" => self.state_dir = v.path(key)?,
            "state_path" => self.state_path = Some(v.path(key)?),
            "order_journal" => self.order_journal = Some(v.path(key)?),
            "pnl_ledger" => self.pnl_ledger = Some(v.path(key)?),
            "ttl_warn_state" => self.ttl_warn_state = Some(v.path(key)?),
            "decisions_log" => self.decisions_log = Some(v.path(key)?),
            "signal_notices" => self.signal_notices = Some(v.path(key)?),
            "btc_alert_state" => self.btc_alert_state = Some(v.path(key)?),
            "regime_state" => self.regime_state = Some(v.path(key)?),
            "froth_state" => self.froth_state = Some(v.path(key)?),
            "run_lock" => self.run_lock = Some(v.path(key)?),
            "deploy_state" => self.deploy_state = Some(v.path(key)?),
            "audit_state" => self.audit_state = Some(v.path(key)?),
            "fillodds_cache" => self.fillodds_cache = Some(v.path(key)?),
            "notify" => {
                if let Raw::Yaml(y) = v {
                    self.notify = to_json(y);
                }
            }
            other => return Err(format!("unknown key `{other}`")),
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.watchlist.is_empty() {
            return Err("`watchlist` needs at least one coin".into());
        }
        for (sym, _) in &self.watchlist {
            if self.route(sym).is_none() {
                return Err(format!("`routing` has no entry for {sym}"));
            }
        }
        if !(self.deploy_min_usd.is_finite() && self.deploy_min_usd > 0.0) {
            return Err(format!(
                "`deploy_min_usd` must be a number above 0, got {}",
                self.deploy_min_usd
            ));
        }
        if !(self.deploy_max_tranche_usd.is_finite() && self.deploy_max_tranche_usd > 0.0) {
            return Err(format!(
                "`deploy_max_tranche_usd` must be a number above 0, got {}",
                self.deploy_max_tranche_usd
            ));
        }
        for (sym, z) in &self.deploy_zones {
            let total: f64 = z.weights.iter().sum();
            if z.weights.iter().any(|w| !w.is_finite()) || (total - 100.0).abs() > 1e-9 {
                return Err(format!(
                    "`deploy_zones.{sym}.weights` must sum to 100, got {total}"
                ));
            }
        }
        Ok(())
    }

    pub fn route(&self, sym: &str) -> Option<&CoinRoute> {
        self.routing.iter().find(|(s, _)| s == sym).map(|(_, r)| r)
    }

    pub fn name(&self, sym: &str) -> String {
        self.names
            .get(sym)
            .cloned()
            .unwrap_or_else(|| sym.to_string())
    }

    /// `(first_pct, step_pct)` for a coin: its override, else the global bands.
    pub fn bands_for(&self, sym: &str) -> (f64, f64) {
        self.bands
            .get(sym)
            .copied()
            .unwrap_or((self.first_pct, self.step_pct))
    }

    pub fn fee_pct(&self, exch: &str) -> f64 {
        match exch {
            "binance" => self.fee_pct_binance,
            "revx" => self.fee_pct_revx,
            _ => self.fee_pct_gate,
        }
    }

    /// `usdc_bag_usd / watchlist size`, or `None` without a bag.
    pub fn base_usd(&self) -> Option<f64> {
        (self.usdc_bag_usd > 0.0).then(|| self.usdc_bag_usd / self.watchlist.len() as f64)
    }

    fn file(&self, over: &Option<PathBuf>, name: &str) -> PathBuf {
        over.clone().unwrap_or_else(|| self.state_dir.join(name))
    }

    /// The ladder state: one file per trade mode, so a dry experiment never touches the
    /// live ledger.
    pub fn ladder_path(&self) -> PathBuf {
        let name = match self.trade_mode.as_str() {
            "live" => "ladder-state.json",
            "dry" => "ladder-state.dry.json",
            _ => "ladder-state.off.json",
        };
        self.file(&self.state_path, name)
    }

    pub fn journal_path(&self) -> PathBuf {
        self.file(&self.order_journal, "orders-journal.json")
    }
    pub fn pnl_path(&self) -> PathBuf {
        self.file(&self.pnl_ledger, "pnl-ledger.json")
    }
    pub fn ttl_path(&self) -> PathBuf {
        self.file(&self.ttl_warn_state, "ttl-warned.json")
    }
    pub fn decisions_path(&self) -> PathBuf {
        self.file(&self.decisions_log, "decisions.jsonl")
    }
    pub fn notices_path(&self) -> PathBuf {
        self.file(&self.signal_notices, "signal-notices.json")
    }
    pub fn btc_alert_path(&self) -> PathBuf {
        self.file(&self.btc_alert_state, "btc-alert-state.json")
    }
    pub fn regime_path(&self) -> PathBuf {
        self.file(&self.regime_state, "regime-state.json")
    }
    /// The label history sits beside the regime cache.
    pub fn regime_history_path(&self) -> PathBuf {
        self.regime_path().with_file_name("regime-history.json")
    }
    pub fn froth_path(&self) -> PathBuf {
        self.file(&self.froth_state, "froth-state.json")
    }
    pub fn archive_path(&self) -> PathBuf {
        self.journal_path().with_file_name("orders-archive.jsonl")
    }
    /// The deploy layer's machine state (baselines, stuck markers, the in-flight top-up).
    pub fn deploy_state_path(&self) -> PathBuf {
        self.file(&self.deploy_state, "deploy-state.json")
    }
    /// The last book audit, for the dashboard banner.
    pub fn audit_state_path(&self) -> PathBuf {
        self.file(&self.audit_state, "audit-state.json")
    }
    /// The daily audit marker: the audit state's name plus `.last`.
    pub fn audit_marker_path(&self) -> PathBuf {
        let mut p = self.audit_state_path().into_os_string();
        p.push(".last");
        PathBuf::from(p)
    }
    pub fn fillodds_cache_path(&self) -> PathBuf {
        self.fillodds_cache
            .clone()
            .unwrap_or_else(|| self.state_dir.join("replay").join("cache"))
    }
    /// The Revolut X pair for a coin listed there.
    pub fn revx_pair(&self, sym: &str) -> Option<&str> {
        self.revx_pairs
            .iter()
            .find(|(s, _)| s == sym)
            .map(|(_, p)| p.as_str())
    }
    pub fn lock_path(&self) -> PathBuf {
        self.run_lock
            .clone()
            .unwrap_or_else(|| self.journal_path().with_extension("lock"))
    }

    /// The bull sell policy's knobs, for the core.
    pub fn sell_policy_config(&self) -> rungbot_core::SellPolicyConfig {
        rungbot_core::SellPolicyConfig {
            giveback_pct: self.sell_giveback_pct,
            giveback_next_pct: self.sell_giveback_next_pct,
            armed_giveback_pct: self.sell_armed_giveback_pct,
            core_pct: self.sell_core_pct,
            tranche_pct: self.sell_tranche_pct,
            tranches: self.sell_tranches.clone(),
            trail_slice_pct: self.sell_trail_slice_pct,
            trail_slice_next_pct: self.sell_trail_slice_next_pct,
            trail_arm_mult: self.sell_trail_arm_mult,
            no_tranche: self.sell_no_tranche.clone(),
            no_base_trail: self.sell_no_base_trail.clone(),
            giveback: self.sell_giveback.clone(),
            trail_arm: self.sell_trail_arm.clone(),
        }
    }
}

fn parse_bands(v: &Raw, key: &str) -> Result<BTreeMap<String, (f64, f64)>, String> {
    let mut out = BTreeMap::new();
    for (sym, b) in v.map(key)? {
        let what = format!("{key}.{sym}");
        let pair = match &b {
            Value::Object(o) => (
                num_of(o.get("first_pct").unwrap_or(&Value::Null), &what)?,
                num_of(o.get("step_pct").unwrap_or(&Value::Null), &what)?,
            ),
            other => {
                let n = nums_of(other, &what)?;
                if n.len() < 2 {
                    return Err(format!("{what}: needs [first_pct, step_pct]"));
                }
                (n[0], n[1])
            }
        };
        out.insert(sym, pair);
    }
    Ok(out)
}

/// A symbol → number map with upper-cased keys. A value that does not read makes the
/// whole map empty, as the reference's `except: {}` did.
fn upper_num_map(v: &Raw, key: &str) -> BTreeMap<String, f64> {
    let Ok(m) = v.map(key) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (k, x) in m {
        match num_of(&x, key) {
            Ok(n) => {
                out.insert(k.to_uppercase(), n);
            }
            Err(_) => return BTreeMap::new(),
        }
    }
    out
}

fn upper_list(v: &Raw, key: &str) -> Result<Vec<String>, String> {
    Ok(v.list(key)?.into_iter().map(|s| s.to_uppercase()).collect())
}

/// The knobs an environment variable may override, by config key.
pub const ENV_KNOBS: &[&str] = &[
    "trade_mode",
    "live_trading_enabled",
    "halt_file",
    "max_slippage_pct",
    "max_order_usd",
    "max_daily_notional",
    "max_daily_orders",
    "max_weekly_notional",
    "usdc_bag_usd",
    "first_pct",
    "step_pct",
    "target_pct",
    "min_trade_pct",
    "min_core_pct",
    "window_hours",
    "buy_floor_pct",
    "breaker_pct",
    "breaker_days",
    "trail_tp",
    "trail_giveback_pct",
    "bands",
    "limit_ttl_days",
    "limit_ttl_days_bear",
    "limit_ttl_days_chop",
    "ttl_renag_days",
    "fee_pct_binance",
    "fee_pct_gate",
    "fee_pct_revx",
    "btc_alert_usd",
    "btc_warn_usd",
    "sell_policy",
    "sell_giveback_pct",
    "sell_giveback",
    "sell_armed_giveback_pct",
    "sell_core_pct",
    "sell_tranches",
    "sell_tranche_pct",
    "sell_no_tranche",
    "sell_no_base_trail",
    "sell_trail_slice_pct",
    "sell_giveback_next_pct",
    "sell_trail_slice_next_pct",
    "sell_trail_arm_mult",
    "sell_trail_arm",
    "sell_arm_all",
    "sell_arm_file",
    "regime_ttl_h",
    "regime_hist_ttl_h",
    "regime_hist_days",
    "regime_confirm_days",
    "run_min_signals",
    "run_ret30_min",
    "signal_remind_s",
    "decisions_max_mb",
    "order_archive_days",
    "run_lock_wait",
    "deploy",
    "deploy_min_usd",
    "deploy_max_tranche_usd",
    "deploy_max_depth_pct",
    "deploy_pin_prices",
    "deploy_sweep_idle",
    "deploy_bull_sweep",
    "deploy_bull_sweep_days",
    "deploy_bull_sweep_max_age",
    "deploy_alloc",
    "deploy_zones",
    "state_path",
    "order_journal",
    "pnl_ledger",
    "ttl_warn_state",
    "decisions_log",
    "signal_notices",
    "btc_alert_state",
    "regime_state",
    "froth_state",
    "run_lock",
    "deploy_state",
    "audit_state",
    "fillodds_cache",
    "fillodds_low",
];

/// The environment variable for a knob: its upper-case name, with the reference's
/// spellings where they differ (`trail_tp` is `TRAIL_TP`, maps end in `_JSON`).
pub fn env_name(key: &str) -> String {
    match key {
        "bands" => "BANDS_JSON".into(),
        "sell_giveback" => "SELL_GIVEBACK_JSON".into(),
        "sell_trail_arm" => "SELL_TRAIL_ARM_JSON".into(),
        "deploy_alloc" => "DEPLOY_ALLOC_JSON".into(),
        "deploy_zones" => "DEPLOY_ZONES_JSON".into(),
        "regime_ttl_h" => "REGIME_TTL_H".into(),
        "regime_hist_ttl_h" => "REGIME_HIST_TTL_H".into(),
        other => other.to_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = "watchlist:\n  AAA: aaa-coin\nrouting:\n  AAA: gate AAA_USDT USDT\n";

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_minimal_file_takes_every_default() {
        let c = RunConfig::from_yaml(MIN, &no_env).unwrap();
        assert_eq!(c.trade_mode, "off");
        assert!(!c.live_trading_enabled);
        assert_eq!((c.first_pct, c.step_pct, c.target_pct), (10.0, 5.0, 10.0));
        assert_eq!(c.sell_tranches, vec![4.0, 8.0, 16.0, 32.0]);
        assert_eq!(c.route("AAA").unwrap().pair, "AAA_USDT");
    }

    #[test]
    fn yaml_booleans_read_as_the_words_they_were() {
        let t = format!("{MIN}trade_mode: off\ntrail_tp: on\nlive_trading_enabled: yes\n");
        let c = RunConfig::from_yaml(&t, &no_env).unwrap();
        assert_eq!((c.trade_mode.as_str(), c.trail_tp.as_str()), ("off", "on"));
        assert!(c.live_trading_enabled);
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let t = format!("{MIN}first_pct: 15\n");
        let env = |k: &str| match k {
            "FIRST_PCT" => Some("12".to_string()),
            "BANDS_JSON" => Some(r#"{"AAA": [20, 10]}"#.to_string()),
            "SELL_NO_TRANCHE" => Some("aaa, bbb".to_string()),
            _ => None,
        };
        let c = RunConfig::from_yaml(&t, &env).unwrap();
        assert_eq!(c.first_pct, 12.0);
        assert_eq!(c.bands_for("AAA"), (20.0, 10.0));
        assert_eq!(c.sell_no_tranche, vec!["AAA", "BBB"]);
    }

    #[test]
    fn maps_and_lists_read_from_blocks() {
        let t = format!(
            "{MIN}bands:\n  AAA:\n    first_pct: 20\n    step_pct: 10\ndeploy_zones:\n  AAA:\n    \
             depths: \"2,5,9\"\n    weights: \"30,40,30\"\nsell_tranches: \"4,8\"\n"
        );
        let c = RunConfig::from_yaml(&t, &no_env).unwrap();
        assert_eq!(c.bands_for("AAA"), (20.0, 10.0));
        assert_eq!(c.deploy_zones["AAA"].depths, vec![2.0, 5.0, 9.0]);
        assert_eq!(c.sell_tranches, vec![4.0, 8.0]);
    }

    #[test]
    fn an_unknown_key_or_a_coin_without_a_route_is_an_error() {
        assert!(
            RunConfig::from_yaml(&format!("{MIN}frist_pct: 1\n"), &no_env)
                .unwrap_err()
                .contains("unknown key")
        );
        let bad = "watchlist:\n  AAA: a\n  BBB: b\nrouting:\n  AAA: gate AAA_USDT USDT\n";
        assert!(RunConfig::from_yaml(bad, &no_env)
            .unwrap_err()
            .contains("BBB"));
    }

    #[test]
    fn the_ladder_file_is_per_mode() {
        let mut c = RunConfig::from_yaml(MIN, &no_env).unwrap();
        c.state_dir = PathBuf::from("/s");
        c.trade_mode = "live".into();
        assert_eq!(c.ladder_path(), PathBuf::from("/s/ladder-state.json"));
        c.trade_mode = "dry".into();
        assert_eq!(c.ladder_path(), PathBuf::from("/s/ladder-state.dry.json"));
    }
}
