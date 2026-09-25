//! `rungbot-exec run`: one 30-minute cycle of the live runtime.
//!
//! In order:
//!
//! 1. The regime: the market label for the mails, and in `trail_tp: auto` the coins
//!    whose sell side trails. The sell-policy mode: bull rules while the label is a
//!    confirmed bull, the ladder otherwise; an unknown label keeps the last run's mode.
//! 2. Prices from the venues' public tickers, then the ladder
//!    ([`rungbot_core::analyze_with`]).
//! 3. Execution ([`execute`]): `dry` plans, `live` reconciles and does its housekeeping,
//!    then places market orders inside the rails.
//! 4. The deploy layer and the daily book audit ([`hooks`]), live only.
//! 5. Rollback: a signal that did not execute leaves its coin's ladder entry as it was
//!    (rungs, ledger and trade window), so it fires again next run.
//! 6. The decision log, the signal-mail dedupe, then at most one of the signal and the
//!    housekeeping mails, the BTC level alert, and the error mail.
//! 7. The ladder state (only when prices came back), and once a day the journal archive.
//!
//! A live run writes the journal before and after every venue call and the P&L ledger
//! after every realized sell, as it goes. Once a venue has been called, nothing ends the
//! run early: a journal write that fails stops further orders, becomes a fatal result
//! and an error mail, and the ladder state, cap counters and P&L ledger are still saved.
//!
//! `--dry-run` places nothing, saves no state, sends nothing and prints what it would
//! send. The exit code is 1 when every price failed and there were errors, else 0. A run
//! that finds the lock held prints `SKIPPED run: …` and exits 0.

pub mod cexconfig;
pub mod config;
pub mod decisions;
pub mod emails;
pub mod execute;
pub mod funding;
pub mod hooks;
pub mod market;
pub mod regime;
pub mod signals;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::housekeeping::{Books, LadderState, Level, Outcome, Persist};
use crate::journal::Journal;
use crate::reconcile::VenueSource;
use crate::{pyfmt, store};
use config::RunConfig;
use execute::{ExecCtx, ExecIo};
use hooks::{AuditHook, DeployHook, HookCtx};
use market::Market;
use signals::{Analysis, Signal, SteerInputs};

/// One result line of a run: what happened to a signal, a housekeeping step, a deploy
/// action. Exactly one of `plan`, `done`, `skip`, `warn`, `err` normally carries the
/// text; an empty text counts as absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunResult {
    pub sym: Option<String>,
    pub side: Option<String>,
    pub mode: Option<String>,
    /// Housekeeping, not this run's ladder signal: never rolled back.
    pub hk: bool,
    /// An order reached a venue.
    pub committed: bool,
    /// The run cannot trade at all.
    pub fatal: bool,
    /// A bull-policy sell.
    pub policy: bool,
    /// From the deploy layer.
    pub deploy: bool,
    /// From the book audit.
    pub audit: bool,
    pub plan: Option<String>,
    pub done: Option<String>,
    pub skip: Option<String>,
    pub warn: Option<String>,
    pub err: Option<String>,
    /// A note for the log only: no mail, no decision line.
    pub info: Option<String>,
}

impl RunResult {
    /// The text under `key` (`plan`/`done`/`skip`/`warn`/`err`), when non-empty.
    pub fn get(&self, key: &str) -> Option<&str> {
        let v = match key {
            "plan" => &self.plan,
            "done" => &self.done,
            "skip" => &self.skip,
            "warn" => &self.warn,
            "err" => &self.err,
            _ => return None,
        };
        v.as_deref().filter(|s| !s.is_empty())
    }

    /// The line's message: plan, else done, skip, warn, err.
    pub fn message(&self) -> String {
        ["plan", "done", "skip", "warn", "err"]
            .iter()
            .find_map(|k| self.get(k))
            .unwrap_or_default()
            .to_string()
    }

    /// A housekeeping outcome as a result line.
    pub fn from_outcome(o: &Outcome) -> RunResult {
        let mut r = RunResult {
            sym: o.sym.clone(),
            side: o.side.clone(),
            mode: Some(o.mode.clone()),
            hk: o.hk,
            committed: o.committed,
            ..Default::default()
        };
        match o.level {
            Level::Done => r.done = Some(o.text.clone()),
            Level::Warn => r.warn = Some(o.text.clone()),
            Level::Err => r.err = Some(o.text.clone()),
        }
        r
    }

    /// The line as a JSON object, absent keys left out.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                m.insert(k.into(), json!(v));
            }
        };
        put("sym", &self.sym);
        put("side", &self.side);
        put("mode", &self.mode);
        put("plan", &self.plan);
        put("done", &self.done);
        put("skip", &self.skip);
        put("warn", &self.warn);
        put("err", &self.err);
        put("info", &self.info);
        for (k, b) in [
            ("hk", self.hk),
            ("committed", self.committed),
            ("fatal", self.fatal),
            ("policy", self.policy),
            ("deploy", self.deploy),
            ("audit", self.audit),
        ] {
            if b {
                m.insert(k.into(), json!(true));
            }
        }
        Value::Object(m)
    }

    fn exec_result(&self) -> rungbot_notify::signal_notices::ExecResult {
        rungbot_notify::signal_notices::ExecResult {
            sym: self.sym.clone().unwrap_or_default(),
            side: self.side.clone().unwrap_or_default(),
            done: self.done.clone(),
            err: self.err.clone(),
            skip: self.skip.clone(),
            plan: self.plan.clone(),
        }
    }
}

/// Where a run sends mail and Telegram pings.
pub trait Outbox {
    /// `true` when the mail went out; the reason is on stderr otherwise.
    fn email(&self, subject: &str, text: &str, html: Option<&str>) -> bool;
    /// Best effort: a ping that fails is dropped.
    fn telegram(&self, text: &str);
}

impl Outbox for rungbot_notify::Notifier {
    fn email(&self, subject: &str, text: &str, html: Option<&str>) -> bool {
        self.send_email(subject, text, html)
    }
    fn telegram(&self, text: &str) {
        if self.telegram.is_some() {
            let _ = rungbot_notify::Notifier::telegram(self, text);
        }
    }
}

/// Everything a run talks to. Tests script every one of them.
pub struct Deps<'a> {
    pub venues: &'a dyn VenueSource,
    pub market: &'a dyn Market,
    pub outbox: &'a dyn Outbox,
    pub deploy: &'a mut dyn DeployHook,
    pub audit: &'a mut dyn AuditHook,
    /// Epoch seconds.
    pub clock: &'a dyn Fn() -> f64,
    /// Seconds to wait between ticker retries.
    pub sleep: &'a dyn Fn(f64),
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

/// `--dry-run` / `--verbose`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    pub dry_run: bool,
    pub verbose: bool,
}

macro_rules! say {
    ($w:expr, $($t:tt)*) => {{
        let _ = writeln!($w, $($t)*);
    }};
}

/// Ticker attempts per coin, and the wait between failed attempts.
pub const TICKER_TRIES: usize = 3;
pub const TICKER_BACKOFF_S: f64 = 4.0;

/// `(price, 24h change)` per coin from its venue. A coin whose ticker fails every try
/// is left out; all failing is an error.
pub fn fetch_prices(
    cfg: &RunConfig,
    m: &dyn Market,
    sleep: &dyn Fn(f64),
) -> Result<BTreeMap<String, (f64, f64)>, String> {
    let mut out = BTreeMap::new();
    let mut last_err = String::from("None");
    for (sym, _) in &cfg.watchlist {
        let Some(r) = cfg.route(sym) else {
            last_err = pyfmt::repr_str(sym);
            continue;
        };
        for attempt in 0..TICKER_TRIES {
            match m.ticker(&r.exch, &r.pair) {
                Ok(t) => {
                    out.insert(sym.clone(), t);
                    break;
                }
                Err(e) => {
                    last_err = e;
                    if attempt < TICKER_TRIES - 1 {
                        sleep(TICKER_BACKOFF_S);
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return Err(format!("venue price fetch failed: {last_err}"));
    }
    Ok(out)
}

fn read_ladder(path: &Path) -> Result<LadderState, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string());
    match raw.and_then(|r| serde_json::from_str::<Value>(&r).map_err(|e| e.to_string())) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Ok(Map::new()),
        Err(e) => Err(format!(
            "{} exists but cannot be read ({e}); refusing to run on an empty ladder state \
             -- restore it or move it aside",
            path.display()
        )),
    }
}

/// When a daily marker was last touched, in epoch seconds.
fn marker_age(path: &Path) -> Option<f64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(
        m.duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
    )
}

fn marker_due(path: &Path, now: f64) -> bool {
    marker_age(path).is_none_or(|t| now - t > 86_400.0)
}

/// Create or refresh a marker, stamped with the run's clock.
fn touch(path: &Path, now: f64) -> std::io::Result<()> {
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(p)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(now.max(0.0));
    f.set_modified(t)
}

/// `[('AAA', 1, 15.0)]`: a signal list as the verbose log prints it.
fn rung_list(sigs: &[Signal]) -> String {
    let items: Vec<String> = sigs
        .iter()
        .map(|s| {
            let pct: f64 = format!("{:.1}", s.pct).parse().unwrap_or(s.pct);
            format!(
                "({}, {}, {})",
                pyfmt::repr_str(&s.sym),
                s.rung,
                pyfmt::float_repr(pct)
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

fn fresh_coin() -> Value {
    json!({"buy": 0, "sell": 0, "deployed_pct": 0.0, "sold_pct": 0.0,
           "win_until": 0.0, "win_dir": ""})
}

const SELL_KEYS: [&str; 4] = ["sell", "sold_pct", "peak_pnl", "bull"];
const BUY_KEYS: [&str; 2] = ["buy", "deployed_pct"];
const WINDOW_KEYS: [&str; 2] = ["win_until", "win_dir"];

/// Roll back every coin whose signal did not execute, so a rung, a ledger or a window
/// is never marked as acted on without a trade.
///
/// Nothing of the coin executed: its whole entry goes back. Something else of it did (a
/// sell on the other side, a housekeeping fill): only the failed side's keys go back,
/// and the trade window with them when this signal is what opened it.
pub fn rollback(execution: &[RunResult], old_state: &LadderState, new_state: &mut LadderState) {
    for (i, res) in execution.iter().enumerate() {
        let Some(sym) = res.sym.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let failed = res.get("skip").is_some() || res.get("err").is_some();
        if !failed || res.committed || res.hk {
            continue;
        }
        let old = old_state.get(sym).cloned().unwrap_or_else(fresh_coin);
        let others: Vec<&RunResult> = execution
            .iter()
            .enumerate()
            .filter(|(j, r)| *j != i && r.sym.as_deref() == Some(sym) && r.committed)
            .map(|(_, r)| r)
            .collect();
        if others.is_empty() {
            new_state.insert(sym.to_string(), old);
            continue;
        }
        let side = res.side.as_deref().unwrap_or("buy");
        let keys: &[&str] = if side == "sell" {
            &SELL_KEYS
        } else {
            &BUY_KEYS
        };
        let old_m = old.as_object().cloned().unwrap_or_default();
        let mut cur = new_state
            .get(sym)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| old_m.clone());
        // The window this signal opened goes too, unless something that executed opened
        // it: a committed signal on the same side, or a sell fill the housekeeping booked
        // (a booked buy fill opens no window).
        let opened_here = cur.get("win_dir").and_then(Value::as_str) == Some(side)
            && !others
                .iter()
                .any(|r| r.side.as_deref() == Some(side) && (!r.hk || side == "sell"));
        let window: &[&str] = if opened_here { &WINDOW_KEYS } else { &[] };
        for k in keys.iter().chain(window) {
            match old_m.get(*k) {
                Some(v) => {
                    cur.insert((*k).into(), v.clone());
                }
                None => {
                    cur.remove(*k);
                }
            }
        }
        new_state.insert(sym.to_string(), Value::Object(cur));
    }
}

fn policy_switch(
    cfg: &RunConfig,
    prev: &Map<String, Value>,
    pol_on: bool,
    pol_armed: bool,
    arm: &Map<String, Value>,
) -> Option<String> {
    let prev_on = prev.get("on");
    let prev_armed = prev.get("armed");
    let changed = !prev.is_empty()
        && (prev_on != Some(&json!(pol_on)) || prev_armed != Some(&json!(pol_armed)));
    if changed {
        let head = if pol_on {
            "BULL rules (trail from peak, alt tranches, core kept)"
        } else {
            "ladder (+15%/+8pp)"
        };
        let tail = if pol_armed {
            if cfg.sell_arm_file.exists() {
                format!(
                    "; BTC blow-off ARMED by the MANUAL ARM FILE -> one-shot exit {}% below \
                     peak (rm {} to disarm)",
                    pyfmt::g(cfg.sell_armed_giveback_pct, 6),
                    cfg.sell_arm_file.display()
                )
            } else {
                let n = |k: &str| arm.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                format!(
                    "; BTC blow-off ARMED by froth-watch (pi {:.2}, Mayer {:.2}, wRSI {:.0}) -> \
                     one-shot exit {}% below peak",
                    n("pi"),
                    n("mayer"),
                    n("wrsi"),
                    pyfmt::g(cfg.sell_armed_giveback_pct, 6)
                )
            }
        } else if pyfmt::truthy(prev_armed) {
            "; BTC arming OFF".into()
        } else {
            String::new()
        };
        return Some(format!("SELL POLICY: {head}{tail}"));
    }
    if prev.is_empty() && pol_on {
        return Some(
            "SELL POLICY: BULL rules now govern sells (first run); ladder dormant until the \
             label leaves bull"
                .into(),
        );
    }
    None
}

/// The fatal result for a journal write that failed once orders may be at a venue.
fn journal_fatal(cfg: &RunConfig, e: &str) -> RunResult {
    RunResult {
        mode: Some(cfg.trade_mode.clone()),
        hk: true,
        fatal: true,
        err: Some(execute::journal_fatal_text(e)),
        ..Default::default()
    }
}

/// The files a run owns, loaded.
struct Stores {
    journal: Journal,
    ttl: crate::housekeeping::TtlWarned,
    pnl: crate::housekeeping::PnlLedger,
}

/// One run under the run lock. A lock another writer keeps past `run_lock_wait` skips
/// this slot: `SKIPPED run: …` on stdout, exit 0, the next run catches up.
pub fn run_locked(cfg: &RunConfig, flags: Flags, deps: &mut Deps) -> Result<i32, String> {
    let wait = std::time::Duration::from_secs_f64(cfg.run_lock_wait.max(0.0));
    let _lock = match store::RunLock::acquire(&cfg.lock_path(), "rungbot-exec run", wait) {
        Ok(l) => l,
        Err(e) => {
            say!(deps.out, "SKIPPED run: {e}");
            return Ok(0);
        }
    };
    run_once(cfg, flags, deps)
}

/// One run, without the lock. Returns the exit code, or an error that stops the run
/// before anything was decided (an unreadable state file).
pub fn run_once(cfg: &RunConfig, flags: Flags, deps: &mut Deps) -> Result<i32, String> {
    let dry_run = flags.dry_run;
    let verbose = flags.verbose || dry_run;
    let mut errors: Vec<String> = Vec::new();
    let t0 = (deps.clock)();

    // 1. Regime: the label always; the RUN gate feeds the sell side in trail_tp: auto.
    let reg = regime::get_regime(cfg, deps.market, t0);
    let mut running: BTreeSet<String> = BTreeSet::new();
    if cfg.trail_tp == "auto" {
        if let Some(coins) = reg.get("coins").and_then(Value::as_object) {
            for (s, c) in coins {
                if pyfmt::truthy(c.get("running")) {
                    running.insert(s.clone());
                }
            }
        }
    }
    let label = reg
        .get("market")
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    let breadth = reg
        .get("breadth_above_sma30")
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_else(|| "?".into());
    let mut regime_note = format!(
        "Market regime: {} (breadth {breadth} above 30d SMA)",
        label.to_uppercase()
    );
    if !running.is_empty() {
        let list: Vec<&str> = running.iter().map(String::as_str).collect();
        regime_note.push_str(&format!(" | RUN -> trailing: {}", list.join(", ")));
    }

    // The sell-policy mode, from the label's history.
    let hist = if cfg.sell_policy == "auto" {
        Some(regime::label_history(cfg, deps.market, t0))
    } else {
        None
    };
    let mut mode = regime::PolicyMode::new(&cfg.sell_policy, hist.as_ref());
    let mut pol_on = mode.active();
    let (mut pol_armed, mut pol_arm) = regime::btc_armed(&cfg.froth_path(), t0);
    if cfg.sell_arm_file.exists() {
        pol_armed = true;
        pol_arm.insert("fired".into(), json!(["manual arm file"]));
    }
    regime_note.push_str(" | sell side: ");
    regime_note.push_str(if pol_on {
        "BULL POLICY (trail + tranches)"
    } else {
        "ladder"
    });
    if pol_armed {
        regime_note.push_str(" | BTC blow-off ARMED");
    }

    let state = read_ladder(&cfg.ladder_path())?;
    let run_now = (deps.clock)();
    let prev_pol = state
        .get("_sellpolicy")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    mode.fallback = prev_pol.get("on").map(|v| pyfmt::truthy(Some(v)));
    pol_on = mode.active();
    let pol_switch = policy_switch(cfg, &prev_pol, pol_on, pol_armed, &pol_arm);

    // 2. Prices and the ladder.
    let trailing: BTreeSet<String> = match cfg.trail_tp.as_str() {
        "on" => cfg
            .watchlist
            .iter()
            .map(|(s, _)| s.clone())
            .chain(cfg.routing.iter().map(|(s, _)| s.clone()))
            .collect(),
        "auto" => running.clone(),
        _ => BTreeSet::new(),
    };
    let steer = SteerInputs {
        policy_on: pol_on,
        armed: cfg
            .watchlist
            .iter()
            .filter(|(s, _)| regime::armed_for(cfg, s, run_now))
            .map(|(s, _)| s.clone())
            .collect(),
        trailing: trailing.iter().cloned().collect(),
    };
    let analysis = fetch_prices(cfg, deps.market, deps.sleep)
        .and_then(|px| signals::analyze(cfg, &px, &state, &steer, run_now));
    let Analysis {
        buys,
        sells,
        rows,
        errors: parse_errors,
        state: mut new_state,
    } = match analysis {
        Ok(a) => a,
        Err(e) => {
            errors.push(e);
            Analysis {
                state: state.clone(),
                ..Default::default()
            }
        }
    };
    errors.extend(parse_errors);

    if verbose {
        for r in &rows {
            let chg = r.chg.map_or("n/a".into(), |c| format!("{c:+.1}%"));
            let pnl = r.pnl.map_or("n/a".into(), |c| format!("{c:+.1}%"));
            let extra = if r.committed_pct > 0.0 {
                format!("  committed {:.0}%", r.committed_pct)
            } else {
                String::new()
            };
            say!(
                deps.out,
                "{:5} {chg:>8}  ${}  entry ${}  P&L {pnl}{extra}",
                r.sym,
                emails::fmt_price(Some(r.usd)),
                emails::fmt_price(r.entry)
            );
        }
        if !errors.is_empty() {
            say!(deps.out, "ERRORS:\n  {}", errors.join("\n  "));
        }
        say!(deps.out, "New buy rungs : {}", rung_list(&buys));
        say!(deps.out, "New sell rungs: {}", rung_list(&sells));
    }

    // 3. Execution. Never during a --dry-run preview. Only a live run reads the order
    // books; one it cannot read stops the run before an order could be placed twice.
    let live_run = !dry_run && cfg.trade_mode == "live";
    let mut stores = if live_run {
        Stores {
            journal: store::load_journal(&cfg.journal_path())?,
            ttl: store::load_ttl_warned(&cfg.ttl_path()),
            pnl: store::load_pnl(&cfg.pnl_path())?,
        }
    } else {
        Stores {
            journal: Journal::default(),
            ttl: Default::default(),
            pnl: Vec::new(),
        }
    };
    let (journal0, ttl0, pnl0) = (stores.journal.clone(), stores.ttl.clone(), stores.pnl.len());
    let halted = cfg.halt_file.exists();
    let block: Option<String> = if cfg.trade_mode == "live" && !cfg.live_trading_enabled {
        Some("LIVE_TRADING_ENABLED is not 'yes'".into())
    } else if cfg.trade_mode == "live" && halted {
        Some(format!("halt file present ({})", cfg.halt_file.display()))
    } else {
        None
    };
    let market_label = reg
        .get("market")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut execution: Vec<RunResult> = Vec::new();
    let jpath = cfg.journal_path();
    let mut persist = if live_run {
        Persist::to_files(jpath.clone(), cfg.pnl_path())
    } else {
        Persist::none()
    };
    if !dry_run {
        let ctx = ExecCtx {
            now: run_now,
            halted,
            market: market_label.clone(),
            trailing: trailing.clone(),
            policy_on: pol_on,
        };
        let mut io = ExecIo {
            venues: deps.venues,
            books: Books {
                journal: &mut stores.journal,
                ladder: &mut new_state,
                ttl: &mut stores.ttl,
                pnl: &mut stores.pnl,
            },
            persist: &mut persist,
        };
        execution = execute::execute_trades(cfg, &buys, &sells, &ctx, &mut io);
    }
    new_state.insert(
        "_sellpolicy".into(),
        json!({"on": pol_on, "armed": pol_armed, "ts": run_now}),
    );
    if let Some(sw) = &pol_switch {
        execution.push(RunResult {
            hk: true,
            committed: true,
            done: Some(sw.clone()),
            ..Default::default()
        });
        if !dry_run {
            deps.outbox.telegram(sw);
        }
    }
    // A policy sell is a risk exit in what may be a fast top: ping, not just mail.
    let policy_syms: BTreeSet<&str> = sells
        .iter()
        .filter(|s| s.policy.as_deref().is_some_and(|p| !p.is_empty()))
        .map(|s| s.sym.as_str())
        .collect();
    if !dry_run {
        for res in &execution {
            let sym = res.sym.as_deref().unwrap_or("");
            if policy_syms.contains(sym) && res.side.as_deref() == Some("sell") {
                if let Some(t) = res.get("done").or_else(|| res.get("err")) {
                    deps.outbox
                        .telegram(&format!("BULL POLICY SELL {sym}: {t}"));
                }
            }
        }
    }

    // 4. The deploy layer and the daily book audit. Not after a failed journal write:
    // the deploy layer places orders.
    if !dry_run && cfg.trade_mode == "live" && persist.failed().is_none() {
        let mut hctx = HookCtx {
            cfg,
            now: run_now,
            venues: deps.venues,
            market: deps.market,
            clock: deps.clock,
            sleep: deps.sleep,
            journal: &mut stores.journal,
            ladder: &mut new_state,
            regime: Some(&reg),
            policy_on: pol_on,
            blocked: block.as_deref(),
            persist: &mut persist,
        };
        if let Err(e) = deps.deploy.check(&mut hctx, &mut execution) {
            execution.push(RunResult {
                mode: Some(cfg.trade_mode.clone()),
                hk: true,
                err: Some(format!("deploy layer: {e}")),
                ..Default::default()
            });
        }
        let marker = cfg.audit_marker_path();
        if marker_due(&marker, run_now) {
            match deps.audit.run(&mut hctx) {
                None => {}
                Some(Ok(rep)) => {
                    let _ = touch(&marker, run_now);
                    execution.extend(rep.results.iter().cloned());
                    say!(deps.out, "{}", rep.log_line());
                }
                Some(Err(e)) => say!(deps.err, "book audit failed: {e}"),
            }
        }
    }
    if let Some(e) = persist.failed() {
        if !execution.iter().any(|r| r.fatal) {
            execution.push(journal_fatal(cfg, e));
        }
    }
    if live_run {
        // Best effort from here on: orders may already be at a venue, so a failed write
        // is reported and the run goes on to save what it can and send the mails.
        if stores.journal != journal0 {
            if let Err(e) = store::save_journal(&jpath, &stores.journal) {
                if !execution.iter().any(|r| r.fatal) {
                    execution.push(journal_fatal(cfg, &format!("journal write failed: {e}")));
                }
            }
        }
        if stores.pnl.len() != pnl0 {
            if let Err(e) = store::save_pnl(&cfg.pnl_path(), &stores.pnl) {
                say!(deps.err, "WARN: could not write P&L ledger: {e}");
            }
        }
        if stores.ttl != ttl0 {
            let _ = store::save_ttl_warned(&cfg.ttl_path(), &stores.ttl);
        }
    }

    // 5. Rollback.
    rollback(&execution, &state, &mut new_state);
    let fatal = execution.iter().any(|r| r.fatal);
    errors.extend(
        execution
            .iter()
            .filter(|r| r.fatal)
            .map(|r| r.err.clone().unwrap_or_default()),
    );
    if verbose {
        for res in &execution {
            let t = ["plan", "done", "skip", "err"]
                .iter()
                .find_map(|k| res.get(k))
                .unwrap_or("None");
            say!(deps.out, "TRADE: {t}");
        }
    }

    // 6. Decision log, dedupe, mails.
    let recs = decisions::lines_for(
        run_now,
        &execution,
        buys.len(),
        sells.len(),
        &cfg.trade_mode,
    );
    let logged = if dry_run {
        Ok(())
    } else {
        decisions::append(&cfg.decisions_path(), &recs, cfg.decisions_max_mb)
    };
    match logged {
        Ok(()) => {
            for r in &recs {
                say!(deps.out, "{}", r.log_line());
            }
        }
        Err(e) => say!(deps.err, "decision log failed: {e}"),
    }

    let mut sent: Vec<String> = Vec::new();
    let ex: Vec<_> = execution.iter().map(RunResult::exec_result).collect();
    let npath = cfg.notices_path();
    let mut nstate = rungbot_notify::signal_notices::load(&npath);
    let filtered = rungbot_notify::signal_notices::filter_signals(
        &mut nstate,
        &buys,
        &sells,
        &ex,
        run_now,
        cfg.signal_remind_s,
    );
    let (mail_buys, mail_sells, suffix): (Vec<&Signal>, Vec<&Signal>, String) = {
        let saved = if dry_run {
            Ok(())
        } else {
            rungbot_notify::signal_notices::save(&npath, &nstate).map_err(|e| e.to_string())
        };
        match saved {
            Ok(()) => {
                let mb: Vec<Signal> = filtered.buys.iter().map(|s| (*s).clone()).collect();
                let ms: Vec<Signal> = filtered.sells.iter().map(|s| (*s).clone()).collect();
                let suffix = rungbot_notify::signal_notices::subject_suffix(&mb, &ms, &ex);
                for s in &filtered.suppressed {
                    say!(deps.out, "SIGNAL-NOTICE suppressed: {s}");
                }
                (filtered.buys, filtered.sells, suffix)
            }
            Err(e) => {
                say!(deps.err, "signal-notice dedupe failed: {e}");
                (buys.iter().collect(), sells.iter().collect(), String::new())
            }
        }
    };

    let when = emails::when(run_now);
    if !mail_buys.is_empty() || !mail_sells.is_empty() {
        let mctx = emails::MailCtx {
            trade_mode: &cfg.trade_mode,
            regime_note: &regime_note,
            first_pct: cfg.first_pct,
            step_pct: cfg.step_pct,
            target_pct: cfg.target_pct,
            min_core_pct: cfg.min_core_pct,
            base_usd: cfg.base_usd(),
            when: &when,
        };
        let (subject, text, html) =
            emails::build_signals_email(&mail_buys, &mail_sells, &rows, &execution, &mctx);
        let subject = subject + &suffix;
        if dry_run {
            say!(
                deps.out,
                "\n--- Would send SIGNALS email ---\nSubject: {subject}\n{text}"
            );
        } else if deps.outbox.email(&subject, &text, Some(&html)) {
            sent.push(format!("signals(buy={},sell={})", buys.len(), sells.len()));
        } else {
            errors.push("failed to send signals email".into());
        }
    } else if !execution.is_empty() {
        let noteworthy: Vec<&RunResult> = execution
            .iter()
            .filter(|r| {
                r.get("done").is_some() || r.get("err").is_some() || r.get("warn").is_some()
            })
            .collect();
        if !noteworthy.is_empty() {
            let (subject, text) = emails::build_housekeeping_email(&noteworthy, &when);
            if dry_run {
                say!(
                    deps.out,
                    "\n--- Would send HOUSEKEEPING email ---\nSubject: {subject}\n{text}"
                );
            } else if deps.outbox.email(&subject, &text, None) {
                sent.push(format!("housekeeping({})", noteworthy.len()));
            } else {
                errors.push("failed to send housekeeping email".into());
            }
        }
    }

    // The BTC level alert: mail only, it never halts or cancels.
    if let Some((bsubj, btext)) = check_btc_level(cfg, &reg, dry_run, run_now) {
        if dry_run {
            say!(
                deps.out,
                "\n--- Would send BTC-LEVEL email ---\nSubject: {bsubj}\n{btext}"
            );
        } else if deps.outbox.email(&bsubj, &btext, None) {
            sent.push("btc-level".into());
        } else {
            errors.push("failed to send BTC-level email".into());
        }
    }

    if !errors.is_empty() {
        let (subject, text) =
            emails::build_error_email(&errors, &when, &cfg.mail_name, &cfg.log_hint);
        if dry_run {
            say!(
                deps.out,
                "\n--- Would send ERROR email ---\nSubject: {subject}\n{text}"
            );
        } else if deps.outbox.email(&subject, &text, None) {
            sent.push(format!("error({})", errors.len()));
        } else {
            say!(deps.err, "Failed to send error email.");
        }
    }

    // 7. Persist the ladder only on a real run with prices; archive the journal daily.
    if !dry_run && !rows.is_empty() {
        let lpath = cfg.ladder_path();
        if let Err(e) = store::save_ladder(&lpath, &new_state) {
            say!(
                deps.err,
                "WARN: could not write state to {}: {e}",
                lpath.display()
            );
        }
        let marker = {
            let mut p = cfg.archive_path().into_os_string();
            p.push(".last");
            std::path::PathBuf::from(p)
        };
        if marker_due(&marker, run_now) {
            // Outside a live run the journal was not read yet.
            let loaded = if live_run {
                Ok(())
            } else {
                store::load_journal(&jpath).map(|j| stores.journal = j)
            };
            let moved = match &loaded {
                Ok(()) => stores.journal.archive_old(cfg.order_archive_days, run_now),
                Err(_) => Vec::new(),
            };
            let done = loaded
                .and_then(|_| store::append_archive(&cfg.archive_path(), &moved))
                .and_then(|_| {
                    if moved.is_empty() {
                        Ok(())
                    } else {
                        store::save_journal(&jpath, &stores.journal)
                    }
                })
                .and_then(|_| touch(&marker, run_now).map_err(|e| e.to_string()));
            match done {
                Ok(()) if !moved.is_empty() => say!(
                    deps.out,
                    "journal: archived {} finished cancel rows older than {:.0}d",
                    moved.len(),
                    cfg.order_archive_days
                ),
                Ok(()) => {}
                Err(e) => say!(deps.err, "journal archive failed: {e}"),
            }
        }
    }

    // A housekeeping mail goes out on runs without a signal, so "sent" is checked first.
    if !sent.is_empty() {
        say!(deps.out, "Sent: {}", sent.join(", "));
    } else if buys.is_empty() && sells.is_empty() && errors.is_empty() {
        say!(deps.out, "No new rung, no error. Quiet run.");
    } else if errors.is_empty() && !dry_run && mail_buys.is_empty() && mail_sells.is_empty() {
        say!(
            deps.out,
            "Signals already notified, nothing sent (see SIGNAL-NOTICE lines)."
        );
    } else {
        say!(deps.out, "Sent: nothing (dry-run or send failed)");
    }
    Ok(if fatal || (!errors.is_empty() && rows.is_empty()) {
        1
    } else {
        0
    })
}

/// The BTC level alert: fires once per downward crossing of the warn or alert band,
/// with 2% hysteresis before it re-arms. Returns the mail, or `None`.
pub fn check_btc_level(
    cfg: &RunConfig,
    reg: &Value,
    dry_run: bool,
    now: f64,
) -> Option<(String, String)> {
    let btc = reg.get("btc");
    let px = match btc.and_then(|b| b.get("px")) {
        Some(Value::Number(n)) => n.as_f64()?,
        Some(Value::String(s)) => s.trim().parse().ok()?,
        _ => return None,
    };
    let sev = |b: &str| match b {
        "warn" => 1,
        "alert" => 2,
        _ => 0,
    };
    let path = cfg.btc_alert_path();
    let st: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let mut stored = st
        .get("band")
        .and_then(Value::as_str)
        .unwrap_or("ok")
        .to_string();
    let (alert, warn) = (cfg.btc_alert_usd, cfg.btc_warn_usd);
    if stored == "alert" && alert != 0.0 && px > alert * 1.02 {
        stored = "warn".into();
    }
    if stored == "warn" && warn != 0.0 && px > warn * 1.02 {
        stored = "ok".into();
    }
    let cur = if alert != 0.0 && px <= alert {
        "alert"
    } else if warn != 0.0 && px <= warn {
        "warn"
    } else {
        "ok"
    };
    let fire = sev(cur) > sev(&stored);
    if !dry_run {
        let band = if fire { cur } else { stored.as_str() };
        let body = format!(
            "{{\"band\": {}, \"px\": {}, \"ts\": {}}}",
            pyfmt::json_str(band),
            pyfmt::float_repr(px),
            now.floor() as i64
        );
        let _ = store::write_atomic(&path, &body);
    }
    if !fire {
        return None;
    }
    let sma200 = emails::value_text(btc.and_then(|b| b.get("sma200")));
    let label = reg
        .get("market")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_uppercase();
    Some(emails::build_btc_email(
        cur,
        px,
        alert,
        warn,
        &label,
        &sma200,
        &cfg.btc_line_name,
        &cfg.halt_file.display().to_string(),
    ))
}
