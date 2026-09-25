//! `rungbot-exec snapshot`: the read-only dashboard collector.
//!
//! One cycle writes three files a static site reads, then optionally publishes them:
//!
//! 1. `data.json` ([`snapshot`]): balances, prices, P&L, the order log, the funding card,
//!    the regime read, decisions and churn. A failure here fails the cycle.
//! 2. `scenarios.json` ([`scenarios`]): the book replayed along past cycles, the ATH
//!    view and the forecast. A failure is logged and the cycle goes on.
//! 3. `wallets.json` ([`wallets`]): self-custody balances and staking read from public
//!    chain APIs, and the "start unbonding" alert. Also soft, and bounded in time.
//! 4. The deploy step: a configured shell command (the site's own deploy tool), with a
//!    failure streak in `.deploy-status.json`. The third failure in a row fails the
//!    cycle, and every twelfth after that, so a unit's failure hook fires about hourly.
//!
//! The collector never places, cancels or signs an order. Output is byte-compatible
//! with the files the first implementation wrote, so every reader keeps working.

pub mod net;
pub mod scenarios;
pub mod snapshot;
pub mod wallets;

use std::io::Write;
use std::path::{Path, PathBuf};

use rungbot_core::watch::json::{obj, Json};
use rungbot_core::yaml::Yaml;

use crate::http::Http;
use crate::reconcile::VenueSource;
use crate::run::config::{expand, RunConfig};
use crate::run::market::Market;
use crate::run::Outbox;

/// Consecutive deploy failures that fail the cycle, and the repeat after that.
pub const ALERT_AT: i64 = 3;
pub const ALERT_EVERY: i64 = 12;

/// Every dashboard knob, from the run config's `dashboard:` block and the environment.
#[derive(Debug, Clone, PartialEq)]
pub struct DashConfig {
    /// Where `data.json`, `scenarios.json` and `wallets.json` are written.
    pub public_dir: PathBuf,
    /// Where the collector keeps its own state (`.deploy-status.json`, caches).
    pub work_dir: PathBuf,
    /// The run's log file; its modification time is the banner's "last run".
    pub run_log: Option<PathBuf>,
    /// Fills made outside the bot, shown in the order log only.
    pub manual_fills: PathBuf,
    /// Venue of orders placed before `legacy_before_ts`, for journal rows that do not
    /// name one (the book was routed differently then).
    pub legacy_venues: Vec<(String, String)>,
    pub legacy_before_ts: f64,
    /// `DASHBOARD_DEPLOY`: run the deploy step after the files are written.
    pub deploy: bool,
    /// The shell command that publishes `public_dir` (for example the static host's CLI).
    pub deploy_command: Option<String>,
    /// CoinGecko id per coin, shared by the wallet prices and the ATH refresh.
    pub coingecko: Vec<(String, String)>,
    pub scenarios: ScenarioKnobs,
    pub wallets: WalletKnobs,
}

/// The scenario model's inputs that are not the book.
#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioKnobs {
    pub replay_cache: PathBuf,
    /// `SCENARIO_FEE_PCT`: one fee for every venue, instead of each venue's.
    pub fee_pct: Option<f64>,
    pub slip_pct: f64,
    pub dex_usd: f64,
    /// `SCENARIO_DAYS`: the horizon of a window that does not set its own.
    pub days: i64,
    pub new_cash_usd: f64,
    pub paths: usize,
    pub seed: u64,
    pub timing: String,
    pub alloc: Option<Vec<(String, f64)>>,
    pub alts: Option<Vec<String>>,
    /// Per coin: `(file, kind)` in priority order.
    pub sources: Vec<(String, Vec<(String, String)>)>,
    pub ath: Vec<(String, f64, String)>,
    pub windows: Vec<rungbot_core::scenarios::ScenarioSpec>,
}

/// One self-custody wallet.
#[derive(Debug, Clone, PartialEq)]
pub enum WalletSpec {
    /// A Cosmos SDK chain through a REST endpoint.
    Cosmos {
        chain: String,
        address: String,
        decimals: u32,
        rest: String,
    },
    /// An ERC-4626 staking vault: the coin and its vault share on one or more EVM
    /// chains, the share's rate read from the first chain's vault.
    Vault {
        address: String,
        chain: String,
        unbond_days: i64,
        /// `(label, rpc, token, vault)`.
        legs: Vec<(String, String, String, String)>,
    },
    /// A Bitcoin address through a mempool-style API.
    Btc {
        address: String,
        chain: String,
        api: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct WalletKnobs {
    pub list: Vec<(String, WalletSpec)>,
    pub targets: PathBuf,
    pub tranche_x: f64,
    pub lead_pct: f64,
    pub rearm_pct: f64,
    pub trail_only: Vec<String>,
    pub timeout_s: f64,
}

fn default_windows() -> Vec<rungbot_core::scenarios::ScenarioSpec> {
    let w =
        |k: &str, l: &str, t0: &str, days: i64, d: &str| rungbot_core::scenarios::ScenarioSpec {
            key: k.into(),
            label: l.into(),
            t0: t0.into(),
            days,
            desc: d.into(),
        };
    vec![
        w("run_2023_early", "2023-24 from Jan 2023", "2023-01-26", 900,
          "Confirmation two months off the Nov 2022 low: BTC to the Mar 2024 top, then the 2024-25 bear; 900 days, so the end is the bear low."),
        w("run_2020_early", "2020-21 from May 2020", "2020-05-12", 900,
          "Confirmation two months after the Covid low: the full 2020-21 run and the 2022 bear (900 days)."),
        w("run_2020", "2020-21 mania", "2020-10-17", 0,
          "BTC confirmed bull Oct 2020: the 2021 double top, then the 2022 bear."),
        w("late_2021", "2021 late leg", "2021-09-01", 0,
          "Confirmation into the Nov 2021 top: a short rounded top, then a full bear."),
        w("run_2023", "2023-24 from Oct 2023", "2023-10-29", 0,
          "Late confirmation, five months before the alt peak: BTC to the Mar 2024 top, a long chop."),
        w("leg_2024", "2024-25 leg", "2024-10-27", 0,
          "Confirmation Oct 2024: BTC to its Oct 2025 top while the alts peaked in Dec 2024 and bled."),
        w("fail_2021", "bull fails", "2021-10-14", 365,
          "Confirmation two weeks before the cycle top: BTC -29% in 90 days, the alts far worse."),
        w("chop_2019", "2019 chop run", "2019-04-15", 365,
          "Confirmation Apr 2019: BTC +100% then a 50% give-back."),
    ]
}

fn words(y: &Yaml) -> Vec<String> {
    let s = match y {
        Yaml::Str(s) | Yaml::Flow(s) => s.clone(),
        other => other.as_str().unwrap_or_default(),
    };
    s.trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(|x| x.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn text(y: &Yaml, key: &str) -> Result<String, String> {
    match y {
        Yaml::Str(s) | Yaml::Flow(s) => Ok(s.clone()),
        Yaml::Num(_) | Yaml::Bool(_) => Ok(y.as_str().unwrap_or_default()),
        _ => Err(format!("`dashboard.{key}` must be a word")),
    }
}

fn number(y: &Yaml, key: &str) -> Result<f64, String> {
    y.as_f64()
        .ok_or_else(|| format!("`dashboard.{key}` must be a number"))
}

fn flag(y: &Yaml) -> bool {
    match y {
        Yaml::Bool(b) => *b,
        other => matches!(
            other
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_lowercase()
                .as_str(),
            "yes" | "true" | "on" | "1"
        ),
    }
}

fn map<'a>(y: &'a Yaml, key: &str) -> Result<&'a [(String, Yaml)], String> {
    match y {
        Yaml::Map(m) => Ok(m),
        Yaml::Null => Ok(&[]),
        _ => Err(format!("`dashboard.{key}` must be a mapping")),
    }
}

impl DashConfig {
    /// The `dashboard:` block of `cfg`, then the environment.
    pub fn from_run(cfg: &RunConfig) -> Result<DashConfig, String> {
        Self::from_run_env(cfg, &|k| std::env::var(k).ok())
    }

    pub fn from_run_env(
        cfg: &RunConfig,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<DashConfig, String> {
        let base = cfg.state_dir.join("dashboard");
        let mut d = DashConfig {
            public_dir: base.join("public"),
            work_dir: base.clone(),
            run_log: None,
            manual_fills: PathBuf::new(),
            legacy_venues: Vec::new(),
            legacy_before_ts: 0.0,
            deploy: false,
            deploy_command: None,
            coingecko: Vec::new(),
            scenarios: ScenarioKnobs {
                replay_cache: cfg.fillodds_cache_path(),
                fee_pct: None,
                slip_pct: 0.1,
                dex_usd: 0.0,
                days: 540,
                new_cash_usd: 10_000.0,
                paths: 20_000,
                seed: 7,
                timing: rungbot_core::scenarios::DEFAULT_TIMING.into(),
                alloc: None,
                alts: None,
                sources: Vec::new(),
                ath: Vec::new(),
                windows: default_windows(),
            },
            wallets: WalletKnobs {
                list: Vec::new(),
                targets: PathBuf::new(),
                tranche_x: 4.0,
                lead_pct: 30.0,
                rearm_pct: 10.0,
                trail_only: Vec::new(),
                timeout_s: 180.0,
            },
        };
        let (mut fills, mut targets) = (None, None);
        for (k, v) in map(&cfg.dashboard, "")? {
            let key = k.as_str();
            match key {
                "public_dir" => d.public_dir = expand(&text(v, key)?),
                "work_dir" => d.work_dir = expand(&text(v, key)?),
                "run_log" => d.run_log = Some(expand(&text(v, key)?)),
                "manual_fills" => fills = Some(expand(&text(v, key)?)),
                "wallet_targets" => targets = Some(expand(&text(v, key)?)),
                "legacy_venues" => {
                    d.legacy_venues = map(v, key)?
                        .iter()
                        .map(|(s, x)| Ok((s.to_uppercase(), text(x, key)?)))
                        .collect::<Result<_, String>>()?
                }
                "legacy_before_ts" => d.legacy_before_ts = number(v, key)?,
                "deploy" => d.deploy = flag(v),
                "deploy_command" => d.deploy_command = Some(text(v, key)?),
                "coingecko" => {
                    d.coingecko = map(v, key)?
                        .iter()
                        .map(|(s, x)| Ok((s.to_uppercase(), text(x, key)?)))
                        .collect::<Result<_, String>>()?
                }
                "replay_cache" => d.scenarios.replay_cache = expand(&text(v, key)?),
                "scenario_fee_pct" => d.scenarios.fee_pct = Some(number(v, key)?),
                "scenario_slip_pct" => d.scenarios.slip_pct = number(v, key)?,
                "scenario_dex_usd" => d.scenarios.dex_usd = number(v, key)?,
                "scenario_days" => d.scenarios.days = number(v, key)? as i64,
                "scenario_new_cash_usd" => d.scenarios.new_cash_usd = number(v, key)?,
                "forecast_paths" => d.scenarios.paths = number(v, key)? as usize,
                "forecast_seed" => d.scenarios.seed = number(v, key)? as u64,
                "forecast_timing" => d.scenarios.timing = text(v, key)?,
                "forecast_alloc" => {
                    d.scenarios.alloc = Some(
                        map(v, key)?
                            .iter()
                            .map(|(s, x)| Ok((s.to_uppercase(), number(x, key)?)))
                            .collect::<Result<_, String>>()?,
                    )
                }
                "alts" => {
                    d.scenarios.alts = Some(words(v).iter().map(|s| s.to_uppercase()).collect())
                }
                "sources" => {
                    let mut out = Vec::new();
                    for (sym, x) in map(v, key)? {
                        let mut files = Vec::new();
                        for part in words(x) {
                            let mut it = part.split_whitespace();
                            let file = it.next().unwrap_or_default().to_string();
                            let kind = it.next().unwrap_or("ohlc").to_string();
                            files.push((file, kind));
                        }
                        out.push((sym.to_uppercase(), files));
                    }
                    d.scenarios.sources = out;
                }
                "ath" => {
                    let mut out = Vec::new();
                    for (sym, x) in map(v, key)? {
                        let t = text(x, key)?;
                        let mut it = t.split_whitespace();
                        let (Some(a), Some(date)) = (it.next(), it.next()) else {
                            return Err(format!(
                                "`dashboard.ath.{sym}` must be `<price> <YYYY-MM-DD>`"
                            ));
                        };
                        let a: f64 = a
                            .parse()
                            .map_err(|_| format!("`dashboard.ath.{sym}`: {a:?} is not a number"))?;
                        out.push((sym.to_uppercase(), a, date.to_string()));
                    }
                    d.scenarios.ath = out;
                }
                "scenarios" => {
                    let mut out = Vec::new();
                    for (k2, x) in map(v, key)? {
                        let get = |f: &str| x.get(f);
                        let t0 = get("t0")
                            .map(|y| text(y, key))
                            .transpose()?
                            .unwrap_or_default();
                        let days = match get("days") {
                            Some(y) if !y.is_null() => number(y, key)? as i64,
                            _ => 0,
                        };
                        out.push(rungbot_core::scenarios::ScenarioSpec {
                            key: k2.clone(),
                            label: get("label")
                                .map(|y| text(y, key))
                                .transpose()?
                                .unwrap_or_else(|| k2.clone()),
                            t0,
                            days,
                            desc: get("desc")
                                .map(|y| text(y, key))
                                .transpose()?
                                .unwrap_or_default(),
                        });
                    }
                    d.scenarios.windows = out;
                }
                "wallet_tranche_x" => d.wallets.tranche_x = number(v, key)?,
                "wallet_lead_pct" => d.wallets.lead_pct = number(v, key)?,
                "wallet_rearm_pct" => d.wallets.rearm_pct = number(v, key)?,
                "wallet_trail_only" => {
                    d.wallets.trail_only = words(v).iter().map(|s| s.to_uppercase()).collect()
                }
                "wallet_timeout_s" => d.wallets.timeout_s = number(v, key)?,
                "wallets" => {
                    let mut out = Vec::new();
                    for (sym, x) in map(v, key)? {
                        out.push((sym.to_uppercase(), wallet_spec(sym, x)?));
                    }
                    d.wallets.list = out;
                }
                other => return Err(format!("unknown key `dashboard.{other}`")),
            }
        }
        let num_env = |var: &str| -> Result<Option<f64>, String> {
            match env(var) {
                Some(s) if !s.trim().is_empty() => s
                    .trim()
                    .parse()
                    .map(Some)
                    .map_err(|_| format!("{var}={s:?} is not a number")),
                _ => Ok(None),
            }
        };
        if let Some(x) = num_env("SCENARIO_FEE_PCT")? {
            d.scenarios.fee_pct = Some(x);
        }
        if let Some(x) = num_env("SCENARIO_SLIP_PCT")? {
            d.scenarios.slip_pct = x;
        }
        if let Some(x) = num_env("SCENARIO_DEX_USD")? {
            d.scenarios.dex_usd = x;
        }
        if let Some(x) = num_env("SCENARIO_DAYS")? {
            d.scenarios.days = x as i64;
        }
        if let Some(x) = num_env("SCENARIO_NEW_CASH_USD")? {
            d.scenarios.new_cash_usd = x;
        }
        if let Some(x) = num_env("WALLET_TRANCHE_X")? {
            d.wallets.tranche_x = x;
        }
        if let Some(x) = num_env("WALLET_LEAD_PCT")? {
            d.wallets.lead_pct = x;
        }
        if let Some(v) = env("DASHBOARD_DEPLOY") {
            d.deploy = v.trim() == "1";
        }
        for w in d.scenarios.windows.iter_mut() {
            if w.days <= 0 {
                w.days = d.scenarios.days;
            }
        }
        d.manual_fills = fills.unwrap_or_else(|| d.work_dir.join("manual-fills.json"));
        d.wallets.targets = targets.unwrap_or_else(|| d.work_dir.join("wallet-targets.json"));
        Ok(d)
    }

    pub fn data_path(&self) -> PathBuf {
        self.public_dir.join("data.json")
    }
    pub fn scenarios_path(&self) -> PathBuf {
        self.public_dir.join("scenarios.json")
    }
    pub fn wallets_path(&self) -> PathBuf {
        self.public_dir.join("wallets.json")
    }
    pub fn deploy_status_path(&self) -> PathBuf {
        self.work_dir.join(".deploy-status.json")
    }
    pub fn revx_cache_path(&self) -> PathBuf {
        self.work_dir.join(".revx-cache.json")
    }
    pub fn wallet_state_path(&self) -> PathBuf {
        self.work_dir.join(".wallets-alert-state.json")
    }
    pub fn wallet_last_good_path(&self) -> PathBuf {
        self.work_dir.join(".wallets-last-good.json")
    }
    /// The CoinGecko id configured for `sym`.
    pub fn coingecko_id(&self, sym: &str) -> Option<&str> {
        self.coingecko
            .iter()
            .find(|(s, _)| s == sym)
            .map(|(_, id)| id.as_str())
    }
}

fn wallet_spec(sym: &str, y: &Yaml) -> Result<WalletSpec, String> {
    let key = format!("wallets.{sym}");
    let get = |f: &str| -> Result<String, String> {
        y.get(f)
            .map(|v| text(v, &format!("{key}.{f}")))
            .transpose()?
            .ok_or_else(|| format!("`dashboard.{key}` needs `{f}`"))
    };
    let opt = |f: &str, dflt: &str| -> Result<String, String> {
        Ok(y.get(f)
            .map(|v| text(v, &format!("{key}.{f}")))
            .transpose()?
            .unwrap_or_else(|| dflt.to_string()))
    };
    match get("kind")?.as_str() {
        "cosmos" => Ok(WalletSpec::Cosmos {
            chain: get("chain")?,
            address: get("address")?,
            decimals: get("decimals")?
                .parse::<f64>()
                .map_err(|_| format!("`dashboard.{key}.decimals` must be a number"))?
                as u32,
            rest: opt("rest", "https://rest.cosmos.directory")?,
        }),
        "vault" => {
            let mut legs = Vec::new();
            for (label, v) in map(y.get("legs").unwrap_or(&Yaml::Null), &key)? {
                let t = text(v, &key)?;
                let p: Vec<&str> = t.split_whitespace().collect();
                let [rpc, token, vault] = p[..] else {
                    return Err(format!(
                        "`dashboard.{key}.legs.{label}` must be `<rpc> <token> <vault>`"
                    ));
                };
                legs.push((label.clone(), rpc.into(), token.into(), vault.into()));
            }
            if legs.is_empty() {
                return Err(format!("`dashboard.{key}` needs `legs`"));
            }
            Ok(WalletSpec::Vault {
                address: get("address")?,
                chain: get("chain")?,
                unbond_days: opt("unbond_days", "0")?
                    .parse::<f64>()
                    .map_err(|_| format!("`dashboard.{key}.unbond_days` must be a number"))?
                    as i64,
                legs,
            })
        }
        "btc" => Ok(WalletSpec::Btc {
            address: get("address")?,
            chain: opt("chain", "bitcoin")?,
            api: opt("api", "https://mempool.space/api")?,
        }),
        other => Err(format!(
            "`dashboard.{key}.kind` must be cosmos, vault or btc, got {other:?}"
        )),
    }
}

// ------------------------------------------------------------------ Python-shaped values

/// Python's type name of a decoded JSON value.
pub fn type_name(v: &Json) -> &'static str {
    match v {
        Json::Null => "NoneType",
        Json::Bool(_) => "bool",
        Json::Int(_) => "int",
        Json::Float(_) => "float",
        Json::Str(_) => "str",
        Json::Arr(_) => "list",
        Json::Obj(_) => "dict",
    }
}

/// Python `float(x)`, with its error text.
pub fn py_float(v: &Json) -> Result<f64, String> {
    match v {
        Json::Str(s) => v
            .to_float()
            .ok_or_else(|| format!("could not convert string to float: {}", repr_str(s))),
        Json::Null | Json::Arr(_) | Json::Obj(_) => Err(format!(
            "float() argument must be a string or a real number, not '{}'",
            type_name(v)
        )),
        _ => Ok(v.to_float().unwrap_or(0.0)),
    }
}

/// `float(d.get(key, 0) or 0)`.
pub fn float_or0(d: &Json, key: &str) -> Result<f64, String> {
    match d.get(key) {
        Some(v) if v.truthy() => py_float(v),
        _ => Ok(0.0),
    }
}

/// Python `repr(str)` (single quotes unless the text holds one).
pub fn repr_str(s: &str) -> String {
    crate::pyfmt::repr_str(s)
}

/// A JSON object key as `json.dumps` writes a non-string dict key.
pub fn key_str(v: &Json) -> String {
    match v {
        Json::Str(s) => s.clone(),
        Json::Null => "null".into(),
        Json::Bool(true) => "true".into(),
        Json::Bool(false) => "false".into(),
        Json::Int(i) => i.to_string(),
        Json::Float(f) => rungbot_core::watch::pyfmt::repr(*f),
        other => other.dumps(None),
    }
}

/// A number as Python sums it: ints stay ints until a float joins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    pub fn f(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
    pub fn json(self) -> Json {
        match self {
            Num::Int(i) => Json::Int(i),
            Num::Float(f) => Json::Float(f),
        }
    }
    pub fn of(v: &Json) -> Option<Num> {
        match v {
            Json::Int(i) => Some(Num::Int(*i)),
            Json::Float(f) => Some(Num::Float(*f)),
            Json::Bool(b) => Some(Num::Int(*b as i64)),
            _ => None,
        }
    }
    /// `a + b`.
    pub fn plus(self, b: Num) -> Num {
        match (self, b) {
            (Num::Int(x), Num::Int(y)) => Num::Int(x + y),
            (x, y) => Num::Float(x.f() + y.f()),
        }
    }
    /// `a * b`.
    pub fn times(self, b: Num) -> Num {
        match (self, b) {
            (Num::Int(x), Num::Int(y)) => Num::Int(x * y),
            (x, y) => Num::Float(x.f() * y.f()),
        }
    }
}

/// Python's built-in `sum()`: integers add exactly until the first float, which switches
/// to the compensated float sum.
pub fn py_sum<I: IntoIterator<Item = Num>>(xs: I) -> Num {
    let mut it = xs.into_iter();
    let mut i_total: i64 = 0;
    let mut first_float = None;
    for x in it.by_ref() {
        match x {
            Num::Int(i) => i_total += i,
            Num::Float(f) => {
                first_float = Some(f);
                break;
            }
        }
    }
    let Some(f) = first_float else {
        return Num::Int(i_total);
    };
    // CPython starts the float path from `int_total + first_float`, then compensates
    // every further float (Neumaier); a later int is added plainly.
    let mut s = i_total as f64 + f;
    let mut c = 0.0;
    for x in it {
        match x {
            Num::Int(i) => s += i as f64,
            Num::Float(x) => {
                let t = s + x;
                if s.abs() >= x.abs() {
                    c += (s - t) + x;
                } else {
                    c += (x - t) + s;
                }
                s = t;
            }
        }
    }
    if c != 0.0 && c.is_finite() {
        s += c;
    }
    Num::Float(s)
}

/// A JSON file, or `default` when it is missing or does not parse.
pub fn read_json_or(path: &Path, default: Json) -> Json {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Json>(&t).ok())
        .unwrap_or(default)
}

/// Epoch seconds of a file's modification time, as Python's `st_mtime` computes it.
pub fn mtime(path: &Path) -> Option<f64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    let d = m.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(d.as_secs() as f64 + d.subsec_nanos() as f64 * 1e-9)
}

/// `str(s)[:n]`: the first `n` characters.
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `datetime.now(timezone.utc).isoformat(timespec="seconds")`: the microsecond clock
/// reading, truncated to the second.
pub fn iso_seconds(now: f64) -> String {
    let full = rungbot_core::iso8601_micros(now);
    format!("{}+00:00", &full[..19])
}

/// What `date` prints in the C locale, for the cycle's header line (UTC).
pub fn c_date(now: f64) -> String {
    let (y, m, d, h, mi, s) = rungbot_core::time::civil(now);
    let days = (now as i64).div_euclid(86_400);
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{} {} {:>2} {:02}:{:02}:{:02} UTC {}",
        WD[days.rem_euclid(7) as usize],
        MO[(m - 1) as usize],
        d,
        h,
        mi,
        s,
        y
    )
}

// ------------------------------------------------------------------ the cycle

/// Signed headers for a Revolut X `GET path` at a millisecond timestamp.
pub type RevxAuth<'a> = dyn Fn(&str, i64) -> Result<Vec<(String, String)>, String> + 'a;

/// What a cycle talks to. Tests script every one of them.
pub struct Io<'a> {
    /// Venue clients: balances and the sell-path check (read-only calls only).
    pub venues: &'a dyn VenueSource,
    /// Tickers for prices.
    pub market: &'a dyn Market,
    /// The regime reading and its label history at `now`.
    pub regime: &'a dyn Fn(f64) -> Result<(Json, Json), String>,
    /// Public reads (chains, CoinGecko) and the Revolut X account read.
    pub http: Http,
    /// Signed headers for a Revolut X `GET path` at a millisecond timestamp.
    pub revx_auth: &'a RevxAuth<'a>,
    /// Where the unbond alert goes.
    pub outbox: &'a dyn Outbox,
    pub clock: &'a dyn Fn() -> f64,
    pub sleep: &'a dyn Fn(f64),
    /// Run the deploy command; `true` on success.
    pub deploy: &'a dyn Fn(&str, &mut dyn Write) -> bool,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

/// Which parts a cycle runs.
#[derive(Debug, Clone, Copy)]
pub struct Parts {
    pub data: bool,
    pub scenarios: bool,
    pub wallets: bool,
    pub deploy: bool,
}

impl Default for Parts {
    fn default() -> Self {
        Parts {
            data: true,
            scenarios: true,
            wallets: true,
            deploy: true,
        }
    }
}

/// One cycle. Returns the exit code: 1 when `data.json` could not be built or the
/// deploy failure streak crossed its alert point, else 0.
pub fn cycle(cfg: &RunConfig, d: &DashConfig, parts: Parts, io: &mut Io) -> i32 {
    let now = (io.clock)();
    let _ = writeln!(io.out, "[{}] snapshot", c_date(now));
    if parts.data {
        if let Err(e) = snapshot::run(cfg, d, io) {
            let _ = writeln!(io.err, "snapshot failed: {e}");
            return 1;
        }
    }
    if parts.scenarios {
        if let Err(e) = scenarios::run(cfg, d, io) {
            let _ = writeln!(io.err, "scenarios: {e}");
            let _ = writeln!(io.out, "scenarios failed (snapshot still deployed)");
        }
    }
    if parts.wallets {
        if let Err(e) = wallets::run(cfg, d, io) {
            let _ = writeln!(io.err, "wallets: {e}");
            let _ = writeln!(io.out, "wallets failed (snapshot still deployed)");
        }
    }
    if !parts.deploy {
        return 0;
    }
    match (&d.deploy_command, d.deploy) {
        (Some(cmd), true) => {
            let ok = (io.deploy)(cmd, io.out);
            let path = d.deploy_status_path();
            if ok {
                let st = deploy_update(&path, "ok", (io.clock)()).0;
                let _ = save_deploy_status(&path, &st);
                return 0;
            }
            let _ = writeln!(io.out, "deploy failed (snapshot still regenerated)");
            let (st, alert) = deploy_update(&path, "fail", (io.clock)());
            let _ = save_deploy_status(&path, &st);
            if alert {
                let n = st
                    .get("consecutive_failures")
                    .map(Json::py_str)
                    .unwrap_or_default();
                let _ = writeln!(io.out, "deploy failed {n}x in a row");
                let _ = writeln!(io.out, "deploy failure streak -> alerting");
                return 1;
            }
            0
        }
        (None, true) => {
            let _ = writeln!(
                io.out,
                "deploy skipped: dashboard.deploy_command is not set (data.json regenerated locally only)"
            );
            0
        }
        _ => {
            let _ = writeln!(
                io.out,
                "deploy skipped: DASHBOARD_DEPLOY!=1 (data.json regenerated locally only)"
            );
            0
        }
    }
}

/// The deploy streak file, or its fresh state.
pub fn load_deploy_status(path: &Path) -> Json {
    let fresh = obj(vec![
        ("last_ok_ts", Json::Null),
        ("last_fail_ts", Json::Null),
        ("consecutive_failures", Json::Int(0)),
    ]);
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Json>(&t).ok())
    {
        Some(j @ Json::Obj(_)) => j,
        _ => fresh,
    }
}

/// Apply one deploy result (`ok` or `fail`) at `now`: the new state, and whether this
/// failure is one that must fail the cycle (the 3rd in a row, then every 12th).
pub fn deploy_update(path: &Path, result: &str, now: f64) -> (Json, bool) {
    let mut st = load_deploy_status(path);
    if result == "ok" {
        st.set("last_ok_ts", Json::Float(now));
        st.set("consecutive_failures", Json::Int(0));
        return (st, false);
    }
    let prev = match st.get("consecutive_failures") {
        Some(Json::Int(i)) => *i,
        Some(Json::Float(f)) => *f as i64,
        Some(Json::Str(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    };
    let n = prev + 1;
    st.set("last_fail_ts", Json::Float(now));
    st.set("consecutive_failures", Json::Int(n));
    let alert = n == ALERT_AT || (n > ALERT_AT && (n - ALERT_AT) % ALERT_EVERY == 0);
    (st, alert)
}

/// Write the streak file atomically, one-space indented.
pub fn save_deploy_status(path: &Path, st: &Json) -> Result<(), String> {
    crate::store::write_atomic(path, &st.dumps(Some(1)))
}

/// Run `cmd` through the platform shell, its output passed through to `out`.
pub fn shell_deploy(cmd: &str, out: &mut dyn Write) -> bool {
    let mut c = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    match c.output() {
        Ok(o) => {
            let _ = out.write_all(&o.stdout);
            let _ = out.write_all(&o.stderr);
            o.status.success()
        }
        Err(e) => {
            let _ = writeln!(out, "deploy command did not start: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_sum_keeps_ints_until_a_float_joins() {
        assert_eq!(py_sum(Vec::<Num>::new()), Num::Int(0));
        assert_eq!(py_sum([Num::Int(2), Num::Int(3)]), Num::Int(5));
        assert_eq!(py_sum([Num::Int(1), Num::Float(0.5)]), Num::Float(1.5));
        assert_eq!(py_sum([Num::Float(0.1); 10]), Num::Float(1.0));
    }

    #[test]
    fn the_c_locale_date_pads_the_day() {
        assert_eq!(c_date(1_725_408_000.0), "Wed Sep  4 00:00:00 UTC 2024");
        assert_eq!(c_date(1_735_000_000.25), "Tue Dec 24 00:26:40 UTC 2024");
    }
}
