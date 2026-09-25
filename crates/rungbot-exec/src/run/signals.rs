//! This run's ladder signals: venue prices in, the core's analysis out, in the shapes
//! the mails, the executor and the dedupe read.
//!
//! The ladder itself is [`rungbot_core::analyze_with`]. This module only converts: the
//! ladder state file (a loose JSON object, with the run's own `_daily`, `_bal`, ...
//! entries beside the coins) to the core's typed state and back, and the core's trades
//! to [`Signal`]s.

use std::collections::BTreeMap;

use rungbot_core::{
    analyze_with, Bands, BullState, Coin, CoinState, Config, Price, Settings, State, Steer, Trail,
    Venue as CoreVenue,
};
use serde_json::{json, Value};

use super::config::RunConfig;
use crate::housekeeping::LadderState;

/// One coin's line in the full watchlist.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub sym: String,
    pub name: String,
    pub usd: f64,
    pub chg: Option<f64>,
    pub entry: Option<f64>,
    pub pnl: Option<f64>,
    pub target: Option<f64>,
    /// Where the coin stands under the bull sell policy, when the policy governs it.
    pub policy: Option<String>,
    pub committed_pct: f64,
    pub cap_now_pct: f64,
    pub over_budget: bool,
    pub win_dir: String,
    pub breaker: bool,
    /// The coin's sell side trails this run.
    pub run: bool,
}

/// A coin that crossed a new rung this run.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    pub sym: String,
    pub name: String,
    pub usd: f64,
    pub chg: Option<f64>,
    pub entry: Option<f64>,
    pub pnl: Option<f64>,
    pub side: &'static str,
    pub rung: i64,
    pub new_rungs: Vec<i64>,
    pub threshold: f64,
    /// % of the coin's base bag (buy) or of the held position (sell).
    pub pct: f64,
    /// Deployed (buy) or sold (sell) after this signal.
    pub ledger_pct: f64,
    pub cap_pct: Option<f64>,
    pub capped: bool,
    /// A bull-policy sell: the policy's reason.
    pub policy: Option<String>,
}

impl rungbot_notify::signal_notices::Signal for Signal {
    fn sym(&self) -> &str {
        &self.sym
    }
    fn rung(&self) -> i64 {
        self.rung
    }
}

/// What the ladder decided this run.
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub buys: Vec<Signal>,
    pub sells: Vec<Signal>,
    pub rows: Vec<Row>,
    pub errors: Vec<String>,
    /// The ladder state with every priced coin's entry replaced.
    pub state: LadderState,
}

/// The steering inputs of one run.
#[derive(Debug, Clone, Default)]
pub struct SteerInputs {
    /// The bull sell policy governs sells this run.
    pub policy_on: bool,
    /// Coins with an external one-shot arm.
    pub armed: Vec<String>,
    /// Coins whose sell side trails.
    pub trailing: Vec<String>,
}

fn f(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(*b as i64 as f64),
        _ => None,
    }
}

fn truthy(v: Option<&Value>) -> bool {
    crate::pyfmt::truthy(v)
}

/// The policy's memory for a coin as stored, read the way the reference read its dict:
/// missing keys take their defaults, an empty object counts as none.
pub fn bull_from(v: Option<&Value>) -> Option<BullState> {
    let o = v?.as_object().filter(|o| !o.is_empty())?;
    Some(BullState {
        peak: f(o.get("peak")).unwrap_or(0.0),
        remaining: f(o.get("remaining")).unwrap_or(100.0),
        tranches: o
            .get("tranches")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default(),
        exited: truthy(o.get("exited")),
        gb: f(o.get("gb")),
        held_below_floor: truthy(o.get("held_below_floor")),
        trail_on: truthy(o.get("trail_on")),
        hits: f(o.get("hits")).unwrap_or(0.0) as u32,
        trail_day: o
            .get("trail_day")
            .and_then(Value::as_str)
            .map(str::to_string),
        since: f(o.get("since")),
    })
}

/// A coin's ladder entry as stored, with the reference's `.get` defaults.
pub fn coin_from(v: &Value) -> CoinState {
    let o = v.as_object();
    let g = |k: &str| o.and_then(|o| o.get(k));
    CoinState {
        buy: f(g("buy")).unwrap_or(0.0) as i64,
        sell: f(g("sell")).unwrap_or(0.0) as i64,
        deployed_pct: f(g("deployed_pct")).unwrap_or(0.0),
        sold_pct: f(g("sold_pct")).unwrap_or(0.0),
        win_until: f(g("win_until")).unwrap_or(0.0),
        win_dir: g("win_dir")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        cost_basis: f(g("cost_basis")).filter(|c| *c != 0.0),
        below_since: f(g("below_since")).unwrap_or(0.0),
        breaker: truthy(g("breaker")),
        peak_pnl: f(g("peak_pnl")).unwrap_or(0.0),
        bull: bull_from(g("bull")),
    }
}

/// A coin's ladder entry as the file stores it: every key, `bull` null when there is none.
pub fn coin_to(c: &CoinState) -> Value {
    json!({
        "buy": c.buy,
        "sell": c.sell,
        "deployed_pct": c.deployed_pct,
        "sold_pct": c.sold_pct,
        "win_until": c.win_until,
        "win_dir": c.win_dir,
        "cost_basis": c.cost_basis,
        "below_since": c.below_since,
        "breaker": c.breaker,
        "peak_pnl": c.peak_pnl,
        "bull": c.bull.as_ref().map(|b| serde_json::to_value(b).unwrap_or(Value::Null)),
    })
}

/// The core config for this run's watchlist.
pub fn core_config(cfg: &RunConfig) -> Result<Config, String> {
    let mut coins = Vec::new();
    for (sym, _) in &cfg.watchlist {
        let route = cfg
            .route(sym)
            .ok_or_else(|| format!("`routing` has no entry for {sym}"))?;
        let bands = match cfg.bands.get(sym) {
            Some((a, b)) => Some(Bands::new(*a, *b).map_err(|e| format!("bands.{sym}: {e}"))?),
            None => None,
        };
        coins.push(Coin {
            symbol: sym.clone(),
            venue: CoreVenue::parse(&route.exch).map_err(|e| e.0)?,
            pair: route.pair.clone(),
            name: cfg.name(sym),
            entry: cfg.entries.get(sym).copied().filter(|e| *e > 0.0),
            bands,
        });
    }
    let settings = Settings {
        bands: Bands::new(cfg.first_pct, cfg.step_pct)?,
        min_trade_pct: cfg.min_trade_pct,
        min_core_pct: cfg.min_core_pct,
        window_hours: cfg.window_hours,
        buy_floor_pct: cfg.buy_floor_pct,
        target_pct: cfg.target_pct,
        breaker_pct: cfg.breaker_pct,
        breaker_days: cfg.breaker_days,
        trail: if cfg.trail_tp == "on" {
            Trail::On
        } else {
            Trail::Off
        },
        trail_giveback_pct: cfg.trail_giveback_pct,
    };
    Config::new(coins, settings).map_err(|e| e.0)
}

/// Run the ladder. `prices` holds `(price, 24h change)` per symbol, only for the coins
/// whose ticker answered.
pub fn analyze(
    cfg: &RunConfig,
    prices: &BTreeMap<String, (f64, f64)>,
    state: &LadderState,
    steer: &SteerInputs,
    now: f64,
) -> Result<Analysis, String> {
    let core = core_config(cfg)?;
    let mut typed: State = BTreeMap::new();
    for (sym, _) in &cfg.watchlist {
        if let Some(v) = state.get(sym) {
            typed.insert(sym.clone(), coin_from(v));
        }
    }
    let px: BTreeMap<String, Price> = prices
        .iter()
        .map(|(k, (p, c))| {
            (
                k.clone(),
                Price {
                    price: *p,
                    chg_24h: Some(*c),
                },
            )
        })
        .collect();
    // A coin whose routed holding is known to be zero has nothing for the policy to sell.
    let hold = state
        .get("_bal")
        .and_then(|b| b.get("hold"))
        .and_then(Value::as_object);
    let flat: Vec<String> = cfg
        .watchlist
        .iter()
        .filter(|(s, _)| hold.and_then(|h| f(h.get(s))).is_some_and(|v| v <= 0.0))
        .map(|(s, _)| s.clone())
        .collect();
    let all: Vec<String> = cfg.watchlist.iter().map(|(s, _)| s.clone()).collect();
    let st = Steer {
        policy: Some(cfg.sell_policy_config()),
        policy_coins: if steer.policy_on { all } else { Vec::new() },
        armed: steer.armed.clone(),
        trail_coins: steer.trailing.clone(),
        flat,
    };
    let out = analyze_with(&core, &px, &typed, &st, now);

    // The core names a missing coin by venue and pair; the run names it by its id.
    let errors = out
        .errors
        .iter()
        .map(|e| {
            for c in &core.coins {
                let core_text = format!(
                    "{} ({}:{}): missing from venue ticker",
                    c.symbol,
                    c.venue.as_str(),
                    c.pair
                );
                if *e == core_text {
                    let id = cfg
                        .watchlist
                        .iter()
                        .find(|(s, _)| *s == c.symbol)
                        .map(|(_, id)| id.as_str())
                        .unwrap_or_default();
                    return format!("{} ({id}): missing from venue ticker", c.symbol);
                }
            }
            e.clone()
        })
        .collect();

    let rows: Vec<Row> = out
        .rows
        .iter()
        .map(|r| Row {
            sym: r.sym.clone(),
            name: r.name.clone(),
            usd: r.price,
            chg: r.chg,
            entry: r.entry,
            pnl: r.pnl,
            target: r.target,
            policy: r.policy.clone(),
            committed_pct: r.committed_pct,
            cap_now_pct: r.cap_now_pct,
            over_budget: r.over_budget,
            win_dir: r.win_dir.clone(),
            breaker: r.breaker,
            run: r.trailing,
        })
        .collect();
    let signal = |t: &rungbot_core::Trade, side: &'static str| Signal {
        sym: t.row.sym.clone(),
        name: t.row.name.clone(),
        usd: t.row.price,
        chg: t.row.chg,
        entry: t.row.entry,
        pnl: t.row.pnl,
        side,
        rung: t.rung,
        new_rungs: t.new_rungs.clone(),
        threshold: t.threshold,
        pct: t.pct,
        ledger_pct: t.ledger_pct,
        cap_pct: t.cap_pct,
        capped: t.capped,
        policy: t.policy_reason.clone(),
    };
    let buys = out.buys.iter().map(|t| signal(t, "buy")).collect();
    let sells = out.sells.iter().map(|t| signal(t, "sell")).collect();

    let mut new_state = state.clone();
    for r in &rows {
        if let Some(c) = out.state.get(&r.sym) {
            new_state.insert(r.sym.clone(), coin_to(c));
        }
    }
    Ok(Analysis {
        buys,
        sells,
        rows,
        errors,
        state: new_state,
    })
}
