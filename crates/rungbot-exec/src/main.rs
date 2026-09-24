//! `rungbot-exec` — the opt-in executor.
//!
//! Reads a plan produced by `rungbot plan --json`, applies the rails, and places GTC
//! limit orders. It is a separate binary from `rungbot` so that installing the ladder
//! never installs the ability to trade.
//!
//! The intended shape of a run:
//!
//! ```text
//! rungbot plan --json > plan.json      # decides; holds no key
//! rungbot-exec plan --from plan.json   # shows the exact orders, places nothing
//! rungbot-exec sync --from plan.json --live --i-understand
//! rungbot-exec reconcile               # books what the venues filled
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use rungbot_exec::clients::{Clients, VENUES};
use rungbot_exec::guard::{self, Caps, Context, Intent, Mode, Refusal};
use rungbot_exec::journal::{self, Order, Side};
use rungbot_exec::reconcile::{self, Settled};
use rungbot_exec::{ids, import, store, Venue};

const USAGE: &str = "\
rungbot-exec — opt-in live execution for rungbot.

It places GTC limit orders from a plan, and reads back what the venues did.

USAGE:
  rungbot-exec run       [--config FILE] [--dry-run] [--verbose]
  rungbot-exec deploy    status | plan [USD] | tranche USD VENUE [--only A,B]
                         | market SHARE% VENUE SYM | cancel [VENUE]   [--config FILE]
  rungbot-exec churn     [--days N] [--json] [--config FILE]
  rungbot-exec fillodds  [--horizons 30,90] [--json] [--config FILE]
  rungbot-exec funding   [--config FILE]
  rungbot-exec snapshot  [--config FILE] [--only data,scenarios,wallets] [--no-deploy]
  rungbot-exec status    [--pair PAIR --venue V]
  rungbot-exec plan      --from PLAN.json --budget N --pair-map SYM=PAIR,...
  rungbot-exec sync      --from PLAN.json --budget N --pair-map SYM=PAIR,...
                         [--venue V] --live --i-understand
  rungbot-exec reconcile
  rungbot-exec cancel    --pair PAIR [--venue V] [--live --i-understand]
  rungbot-exec archive   [--days N]
  rungbot-exec import-cex DIR [--write [--force]] | --config
  rungbot-exec keys      check [--venue V]

DEPLOY (the monthly-capital layer; the run calls it every cycle with deploy: live):
  status             baselines, each venue's quote balance, the open zones
  plan [USD]         the zone plan for USD (default 100) per venue; places nothing
  tranche USD VENUE  roll the venue's zones and ladder USD more; --only A,B limits it
                     to those coins (tranche 0 VENUE --only A re-ladders A)
  market SHARE% VENUE SYM   roll SYM's zones, buy SHARE% of their budget at market,
                     ladder the rest
  cancel [VENUE]     cancel every resting deploy zone (works while halted)
  tranche, market and cancel take the run lock; tranche and market refuse while
  live_trading_enabled is off or the halt file exists.

SNAPSHOT (the dashboard collector; read-only on every venue, see the `dashboard:`
block in contrib/rungbot-run.example.yaml):
  writes data.json, scenarios.json and wallets.json into dashboard.public_dir, then
  runs dashboard.deploy_command when DASHBOARD_DEPLOY=1 (or dashboard.deploy: yes).
  Exit 1 when data.json could not be built, or on the 3rd deploy failure in a row
  (and every 12th after), so the unit's failure hook reports it.

READ-ONLY REPORTS (the run config names the journal and state files):
  churn              how often the zones were rolled, per fill, and rung lifetimes
  fillodds           the odds each resting buy rung fills, from the candle cache
  funding            the onramp top-up card: Gate's gap and what Revolut X holds for it

RUN (the 30-minute cycle; see contrib/rungbot-run.example.yaml):
  --config FILE      the run's one config file (default ~/.config/rungbot/run.yaml,
                     or RUNGBOT_RUN_CONFIG)
  --dry-run          place nothing, save nothing, send nothing; print what it would send
  --verbose          print every coin, the new rungs and every result (implied by
                     --dry-run)

REQUIRED for plan and sync:
  --from FILE        a plan from `rungbot plan --json`
  --budget N         what 100% is worth, in quote currency. The ladder sizes in
                     percent and does not know your balances, so it cannot infer this.
  --pair-map SYM=PAIR[,...]   which venue pair each symbol trades as

OPTIONS:
  --live             actually place or cancel orders. Without it nothing is sent.
  --i-understand     acknowledge live trading. Required once, every run.
  --venue V          gate (default), revx or binance
  --journal PATH     order journal (default: alongside the rungbot state)
  --pair PAIR        limit status/cancel to one pair
  --days N           archive finished cancels older than N days (default 30)
  --write            import-cex: write the journal and the run state (ladder state,
                     P&L ledger, stale-order flags) beside it (default: a dry run)
  --force            import-cex: replace a journal that already holds rows
  --config           import-cex: print the run config the bot in DIR runs with (its
                     module defaults and its cron wrapper's exports) as YAML, and
                     write nothing. It holds personal values: keep it out of any repo
  --max-order N      per-order cap in quote currency (default 50)
  --max-daily N      daily notional cap (default 200)
  --max-orders N     daily order count cap (default 10)
  --max-slippage N   refuse if the venue moved this far from the decision (default 2)

Commands that write the journal (sync, reconcile, cancel, archive, import-cex
--write) take the run lock; a second one waits, then gives up naming the holder.

ENVIRONMENT:
  RUNGBOT_GATE_KEY / RUNGBOT_GATE_SECRET             Gate credentials
  RUNGBOT_BINANCE_KEY / RUNGBOT_BINANCE_SECRET       Binance credentials
  RUNGBOT_REVX_KEY / RUNGBOT_REVX_PRIVATE_KEY_PEM    Revolut X key and Ed25519 PEM path
                     (or ~/.config/rungbot/<venue>.env, mode 600; never the watchlist)
  RUNGBOT_HALT       path to the halt file (default ~/.config/rungbot/HALT)
  RUNGBOT_LOCK       the run lock (default: <journal>.lock)
  RUNGBOT_LOCK_WAIT  seconds a second writer waits for the lock (default 120)
  RUNGBOT_ORDER_ARCHIVE  the archive (default: orders-archive.jsonl beside the journal)
  RUNGBOT_OFFLINE=1  refuse every network call, signed or public

Gate keys with no IP allowlist are disabled after 90 days, silently. If orders
stop being accepted, check that first.
";

struct Args {
    cmd: String,
    flags: BTreeMap<String, String>,
}

impl Args {
    fn parse(argv: &[String]) -> Result<Args, String> {
        let mut it = argv.iter().peekable();
        let cmd = it.next().cloned().unwrap_or_default();
        let mut flags = BTreeMap::new();
        while let Some(arg) = it.next() {
            let Some(bare) = arg.strip_prefix("--") else {
                // `keys check` and friends: a bare word after the command.
                flags.insert("sub".into(), arg.clone());
                continue;
            };
            if let Some((k, v)) = bare.split_once('=') {
                flags.insert(k.into(), v.into());
                continue;
            }
            let takes = matches!(
                bare,
                "config"
                    | "from"
                    | "journal"
                    | "venue"
                    | "days"
                    | "pair"
                    | "pair-map"
                    | "budget"
                    | "max-order"
                    | "max-daily"
                    | "max-orders"
                    | "max-slippage"
            );
            let value = if takes {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("--{bare} needs a value"))?
            } else {
                "1".into()
            };
            flags.insert(bare.into(), value);
        }
        Ok(Args { cmd, flags })
    }

    fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(|s| s.as_str())
    }

    fn num(&self, k: &str, default: f64) -> Result<f64, String> {
        match self.get(k) {
            Some(v) => v.parse().map_err(|e| format!("--{k}: {e}")),
            None => Ok(default),
        }
    }
}

fn config_dir() -> PathBuf {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config"),
    };
    base.join("rungbot")
}

fn state_dir() -> PathBuf {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".local")
            .join("state"),
    };
    base.join("rungbot")
}

fn halt_path() -> PathBuf {
    match std::env::var("RUNGBOT_HALT") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => config_dir().join("HALT"),
    }
}

fn journal_path(args: &Args) -> PathBuf {
    args.get("journal")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir().join("orders-journal.json"))
}

/// One writer at a time on the journal: see `rungbot_exec::store`. `RUNGBOT_LOCK_WAIT`
/// (seconds, default 120) is how long a second run waits before giving up.
fn lock_journal(jpath: &Path, who: &str) -> Result<store::RunLock, String> {
    let wait = std::env::var("RUNGBOT_LOCK_WAIT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(120);
    store::RunLock::acquire(&store::lock_path(jpath), who, Duration::from_secs(wait))
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One order the plan implies, before the rails see it.
#[derive(Debug, Clone)]
struct Planned {
    sym: String,
    pair: String,
    side: Side,
    rung: i64,
    price: f64,
    quote: f64,
    kind: String,
}

/// Turn `rungbot plan --json` into concrete orders.
///
/// The ladder speaks in percentages because it does not know your balances. Converting
/// that into an amount needs a budget you state explicitly — there is no way to infer it
/// and no attempt is made to.
fn planned_orders(
    plan: &serde_json::Value,
    pair_map: &BTreeMap<String, String>,
    budget: f64,
) -> Result<Vec<Planned>, String> {
    let mut out = Vec::new();
    for (key, side, kind) in [
        ("buys", Side::Buy, "ladder_buy"),
        ("sells", Side::Sell, "ladder_sell"),
    ] {
        let Some(arr) = plan.get(key).and_then(|v| v.as_array()) else {
            continue;
        };
        for t in arr {
            let sym = t
                .get("sym")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("a {key} entry has no sym"))?
                .to_string();
            let price = t.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let pct = t.get("pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let rung = t.get("rung").and_then(|v| v.as_i64()).unwrap_or(0);
            let pair = pair_map
                .get(&sym)
                .cloned()
                .ok_or_else(|| format!("no pair for {sym}; pass --pair-map {sym}=<PAIR>"))?;
            out.push(Planned {
                sym,
                pair,
                side,
                rung,
                price,
                quote: budget * pct / 100.0,
                kind: kind.into(),
            });
        }
    }
    Ok(out)
}

fn parse_pair_map(s: Option<&str>) -> BTreeMap<String, String> {
    s.unwrap_or_default()
        .split(',')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_uppercase(), v.trim().to_string()))
        .collect()
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || matches!(argv[0].as_str(), "--help" | "-h" | "help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if matches!(argv[0].as_str(), "--version" | "-V") {
        println!("rungbot-exec {}", rungbot_exec::VERSION);
        return ExitCode::SUCCESS;
    }
    let args = match Args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            return ExitCode::from(1);
        }
    };

    let r = match args.cmd.as_str() {
        "run" => return cmd_run(&args),
        "deploy" | "churn" | "fillodds" | "funding" => cmd_layer(&argv),
        "snapshot" => return cmd_snapshot(&argv),
        "status" => cmd_status(&args),
        "plan" => cmd_plan(&args),
        "sync" => cmd_sync(&args),
        "reconcile" => cmd_reconcile(&args),
        "cancel" => cmd_cancel(&args),
        "archive" => cmd_archive(&args),
        "import-cex" => cmd_import(&args),
        "keys" => cmd_keys(&args),
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// One 30-minute cycle. Exit 0 on a clean run or a skipped one (the lock was held),
/// 1 when prices failed outright or the state could not be read.
fn cmd_run(args: &Args) -> ExitCode {
    use rungbot_exec::run::{self, config::RunConfig, hooks, market::PublicMarket};
    let path = match args.get("config") {
        Some(p) => PathBuf::from(p),
        None => match std::env::var("RUNGBOT_RUN_CONFIG") {
            Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
            _ => config_dir().join("run.yaml"),
        },
    };
    let cfg = match RunConfig::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let notifier = serde_json::from_value::<rungbot_notify::Notifier>(if cfg.notify.is_null() {
        serde_json::json!({})
    } else {
        cfg.notify.clone()
    });
    let notifier = match notifier {
        Ok(n) => n.with_env_overrides(),
        Err(e) => {
            eprintln!("{}: notify: {e}", path.display());
            return ExitCode::from(1);
        }
    };
    let clients = Clients::new();
    let market = PublicMarket::default();
    let _ = (hooks::NoDeploy, hooks::NoAudit);
    let (mut deploy, mut audit) = (
        rungbot_exec::deploy::DeployLayer,
        rungbot_exec::deploy::audit::BookAudit,
    );
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let mut deps = run::Deps {
        venues: &clients,
        market: &market,
        outbox: &notifier,
        deploy: &mut deploy,
        audit: &mut audit,
        clock: &now,
        sleep: &|s| std::thread::sleep(Duration::from_secs_f64(s)),
        out: &mut out,
        err: &mut err,
    };
    let flags = run::Flags {
        dry_run: args.has("dry-run"),
        verbose: args.has("verbose"),
    };
    match run::run_locked(&cfg, flags, &mut deps) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// One dashboard cycle: `data.json`, `scenarios.json`, `wallets.json`, then the deploy.
fn cmd_snapshot(argv: &[String]) -> ExitCode {
    use rungbot_core::watch::json::Json;
    use rungbot_exec::dashboard::{self, DashConfig, Io, Parts};
    use rungbot_exec::run::{config::RunConfig, market::PublicMarket, regime};
    let setup = || -> Result<(RunConfig, DashConfig), String> {
        let cfg = RunConfig::load(&run_config_path(argv))?;
        let d = DashConfig::from_run(&cfg)?;
        Ok((cfg, d))
    };
    let (cfg, d) = match setup() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let mut parts = Parts::default();
    if let Some(only) = flag_value(argv, "--only") {
        let want: Vec<&str> = only.split(',').map(str::trim).collect();
        parts.data = want.contains(&"data");
        parts.scenarios = want.contains(&"scenarios");
        parts.wallets = want.contains(&"wallets");
    }
    if argv.iter().any(|a| a == "--no-deploy") {
        parts.deploy = false;
    }
    let notifier = serde_json::from_value::<rungbot_notify::Notifier>(if cfg.notify.is_null() {
        serde_json::json!({})
    } else {
        cfg.notify.clone()
    })
    .map(|n| n.with_env_overrides())
    .unwrap_or_default();
    let clients = Clients::new();
    let market = PublicMarket::default();
    let to_json = |v: serde_json::Value| -> Json { serde_json::from_value(v).unwrap_or_default() };
    let read_regime = |now: f64| -> Result<(Json, Json), String> {
        Ok((
            to_json(regime::get_regime(&cfg, &market, now)),
            to_json(regime::label_history(&cfg, &market, now)),
        ))
    };
    let signer: std::cell::OnceCell<Result<(String, rungbot_exec::revx::RevxKey), String>> =
        std::cell::OnceCell::new();
    let revx_auth = |path: &str, ts: i64| -> Result<Vec<(String, String)>, String> {
        let s = signer.get_or_init(|| {
            let c = rungbot_exec::keys::load_revx(None).map_err(|e| e.to_string())?;
            let k = rungbot_exec::revx::RevxKey::from_file(&c.pem_path)?;
            Ok((c.key, k))
        });
        let (key, k) = s.as_ref().map_err(Clone::clone)?;
        Ok(vec![
            ("X-Revx-API-Key".to_string(), key.clone()),
            ("X-Revx-Timestamp".to_string(), ts.to_string()),
            (
                "X-Revx-Signature".to_string(),
                k.sign(format!("{ts}GET{path}").as_bytes()),
            ),
        ])
    };
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let mut io = Io {
        venues: &clients,
        market: &market,
        regime: &read_regime,
        http: rungbot_exec::http::Http::default(),
        revx_auth: &revx_auth,
        outbox: &notifier,
        clock: &now,
        sleep: &|s| std::thread::sleep(Duration::from_secs_f64(s)),
        deploy: &dashboard::shell_deploy,
        out: &mut out,
        err: &mut err,
    };
    ExitCode::from(dashboard::cycle(&cfg, &d, parts, &mut io) as u8)
}

/// `--config FILE`, else `RUNGBOT_RUN_CONFIG`, else `~/.config/rungbot/run.yaml`.
fn run_config_path(argv: &[String]) -> PathBuf {
    if let Some(i) = argv.iter().position(|a| a == "--config") {
        if let Some(p) = argv.get(i + 1) {
            return PathBuf::from(p);
        }
    }
    if let Some(p) = argv.iter().find_map(|a| a.strip_prefix("--config=")) {
        return PathBuf::from(p);
    }
    match std::env::var("RUNGBOT_RUN_CONFIG") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => config_dir().join("run.yaml"),
    }
}

/// The value after `--flag`.
fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    let i = argv.iter().position(|a| a == flag)?;
    argv.get(i + 1).map(String::as_str)
}

/// Positional words after the command, flags and their values left out.
fn positionals(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        if matches!(a.as_str(), "--config" | "--only" | "--days" | "--horizons") {
            it.next();
        } else if !a.starts_with("--") {
            out.push(a.clone());
        }
    }
    out
}

/// `deploy …`, `churn`, `fillodds` and `funding`: the deploy layer's commands and its
/// read-only reports, all on the run config.
fn cmd_layer(argv: &[String]) -> Result<(), String> {
    use rungbot_exec::deploy::{self, cli, Layer, LiveRegime};
    use rungbot_exec::run::{config::RunConfig, funding, market::PublicMarket};
    let cfg = RunConfig::load(&run_config_path(argv))?;
    let jpath = cfg.journal_path();
    let pos = positionals(argv);
    let json_out = argv.iter().any(|a| a == "--json");
    let mut out = std::io::stdout();
    match argv[0].as_str() {
        "churn" => {
            let j = store::load_journal(&jpath)?;
            let days: i64 = match flag_value(argv, "--days") {
                Some(d) => d.parse().map_err(|e| format!("--days: {e}"))?,
                None => 7,
            };
            let rows: Vec<&Order> = j.orders.values().collect();
            let m = deploy::churn::metrics(&rows, now(), days);
            if json_out {
                println!("{}", rungbot_exec::pyfmt::dumps(&m, Some(1)));
            } else {
                println!("{}", deploy::churn::summary(&m));
            }
            return Ok(());
        }
        "fillodds" => {
            let j = store::load_journal(&jpath)?;
            let horizons: Vec<usize> = match flag_value(argv, "--horizons") {
                Some(h) => h
                    .split(',')
                    .map(|x| x.trim().parse().map_err(|e| format!("--horizons: {e}")))
                    .collect::<Result<_, String>>()?,
                None => vec![30, 90],
            };
            let read = |p: &Path| -> Option<serde_json::Value> {
                serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
            };
            let reg = read(&cfg.regime_path())
                .ok_or_else(|| format!("{}: no regime reading yet", cfg.regime_path().display()))?;
            let hist = read(&cfg.regime_history_path());
            let rcfg = rungbot_core::RegimeConfig {
                run_min_signals: cfg.run_min_signals,
                run_ret30_min: cfg.run_ret30_min,
            };
            let res = deploy::fillodds::compute(
                &j,
                &reg,
                hist.as_ref(),
                &cfg.fillodds_cache_path(),
                &horizons,
                cfg.fillodds_low,
                rcfg,
            )?;
            if json_out {
                println!("{}", rungbot_exec::pyfmt::dumps(&res, Some(1)));
            } else {
                println!("{}", deploy::fillodds::render(&res));
            }
            return Ok(());
        }
        "funding" => {
            let clients = Clients::new();
            let rx = clients
                .get("revx")?
                .balances_full()
                .map_err(|e| e.to_string())?;
            let gt = clients
                .get("gate")?
                .balances_full()
                .map_err(|e| e.to_string())?;
            let routing: Vec<(String, String)> = cfg
                .routing
                .iter()
                .map(|(s, r)| (s.clone(), r.exch.clone()))
                .collect();
            let alloc: Vec<(String, f64)> = cfg
                .deploy_alloc
                .iter()
                .map(|(s, w)| (s.to_uppercase(), *w))
                .collect();
            let card = funding::card(
                funding::revx_stable_from(&rx),
                funding::gate_stable_from(&gt),
                rx.get("USDC").map_or(0.0, |b| b.free),
                &alloc,
                &routing,
            );
            println!("{}", rungbot_exec::pyfmt::dumps(&card, Some(1)));
            return Ok(());
        }
        _ => {}
    }
    let sub = pos.first().map(String::as_str).unwrap_or("");
    let writes = matches!(sub, "tranche" | "market" | "cancel");
    let _lock = if writes {
        let wait = Duration::from_secs_f64(cfg.run_lock_wait.max(0.0));
        Some(
            store::RunLock::acquire(
                &cfg.lock_path(),
                &format!("rungbot-exec deploy {sub}"),
                wait,
            )
            .map_err(|e| format!("not run: {e}"))?,
        )
    } else {
        None
    };
    let mut j = store::load_journal(&jpath)?;
    let clients = Clients::new();
    let market = PublicMarket::default();
    let feed = LiveRegime {
        cfg: &cfg,
        market: &market,
        now: now(),
    };
    let persist = |j: &journal::Journal| store::save_journal(&jpath, j);
    let stderr = |s: &str| eprintln!("{s}");
    let sleep = |s: f64| std::thread::sleep(Duration::from_secs_f64(s));
    let mut layer = Layer {
        cfg: &cfg,
        venues: &clients,
        regime: &feed,
        clock: &now,
        sleep: &sleep,
        persist: &persist,
        stderr: &stderr,
        j: &mut j,
        results: Vec::new(),
    };
    let num = |s: Option<&String>, what: &str| -> Result<f64, String> {
        s.ok_or_else(|| format!("usage: rungbot-exec deploy {what}"))?
            .parse::<f64>()
            .map_err(|e| format!("{what}: {e}"))
    };
    match sub {
        "status" => cli::status(&layer, &mut out),
        "plan" => {
            let b = match pos.get(1) {
                Some(v) => v.parse::<f64>().map_err(|e| format!("plan: {e}"))?,
                None => 100.0,
            };
            cli::plan(&layer, b, &mut out)
        }
        "tranche" => {
            let usage = "tranche <usd> <binance|gate|revx> [--only A,B]";
            let b = num(pos.get(1), usage)?;
            let venue = pos
                .get(2)
                .ok_or_else(|| format!("usage: rungbot-exec deploy {usage}"))?;
            let only = flag_value(argv, "--only").map(|v| {
                v.split(',')
                    .map(|x| x.trim().to_uppercase())
                    .collect::<std::collections::BTreeSet<String>>()
            });
            cli::tranche(&mut layer, b, venue, only, &mut out)
        }
        "market" => {
            let usage = "market <share%> <binance|gate|revx> <SYM>";
            let share = num(pos.get(1), usage)?;
            let (Some(venue), Some(sym)) = (pos.get(2), pos.get(3)) else {
                return Err(format!("usage: rungbot-exec deploy {usage}"));
            };
            cli::market(&mut layer, share, venue, &sym.to_uppercase(), &mut out)
        }
        "cancel" => {
            let venue = pos
                .get(1)
                .map(String::as_str)
                .filter(|v| deploy::VENUES.contains(v));
            cli::cancel(&mut layer, venue, &mut out)
        }
        _ => Err(
            "usage: rungbot-exec deploy status | plan [USD] | tranche USD VENUE \
                  [--only A,B] | market SHARE% VENUE SYM | cancel [VENUE]"
                .into(),
        ),
    }
}

fn venue_name(args: &Args) -> Result<&str, String> {
    let v = args.get("venue").unwrap_or("gate");
    if VENUES.contains(&v) {
        Ok(v)
    } else {
        Err(format!(
            "--venue {v:?}: expected one of {}",
            VENUES.join(", ")
        ))
    }
}

fn cmd_keys(args: &Args) -> Result<(), String> {
    if args.get("sub") != Some("check") {
        return Err("usage: rungbot-exec keys check [--venue V]".into());
    }
    let name = venue_name(args)?;
    let clients = Clients::new();
    let venue = clients.get(name)?;
    match venue.balances_full() {
        Ok(b) => {
            let funded = b
                .values()
                .filter(|x| x.free > 0.0 || x.locked > 0.0)
                .count();
            println!("✓ {name} accepted the key from this IP");
            println!("✓ it can read balances ({funded} assets with a balance)");
            println!();
            println!("What this check cannot tell you: no venue here exposes a key's full");
            println!("permission set, so withdrawal scope cannot be verified from here.");
            println!("Open the venue's API management page and confirm withdrawals are OFF.");
            if name == "gate" {
                println!();
                println!("If this key has no IP allowlist, Gate disables it 90 days after");
                println!("creation, without telling you. Allowlisting also prevents that.");
            }
            Ok(())
        }
        Err(e) => Err(match e.hint() {
            Some(h) => format!("✗ {e}\n  {h}"),
            None => format!("✗ {e}"),
        }),
    }
}

fn cmd_status(args: &Args) -> Result<(), String> {
    let jpath = journal_path(args);
    let j = store::load_journal(&jpath)?;
    let day_ago = now() - 86_400.0;

    println!("journal: {}", jpath.display());
    println!(
        "open: {} · filled (24h): {} · placed today: {} · notional today: {:.2}",
        j.open_orders(None).len(),
        j.filled_since(day_ago, false).len(),
        j.count_since(day_ago),
        j.notional_since(day_ago)
    );
    let halt = halt_path();
    println!(
        "halt file: {} ({})",
        halt.display(),
        if halt.exists() {
            "PRESENT — trading stopped"
        } else {
            "absent"
        }
    );
    let errors: Vec<&Order> = j
        .open_orders(None)
        .into_iter()
        .filter(|o| o.last_error.is_some())
        .collect();
    if !errors.is_empty() {
        println!("\n{} open order(s) whose last poll failed:", errors.len());
        for o in errors {
            println!(
                "  {} {} {}",
                o.client_id,
                o.exch,
                o.last_error.as_deref().unwrap_or("")
            );
        }
    }

    let unswept = j.venue_cancelled_unswept(None);
    if !unswept.is_empty() {
        println!(
            "\n{} order(s) the venue cancelled, cash not re-laddered:",
            unswept.len()
        );
        for o in unswept {
            println!(
                "  {} {} {:.6} @ {:.6}",
                o.client_id,
                o.sym,
                o.base.unwrap_or(0.0),
                o.price.unwrap_or(0.0)
            );
        }
    }

    if let Some(pair) = args.get("pair") {
        let clients = Clients::new();
        let venue = clients.get(venue_name(args)?)?;
        let open = venue.open_orders(pair).map_err(|e| e.to_string())?;
        println!("\nresting at {} for {pair}: {}", venue.name(), open.len());
        for o in open {
            println!(
                "  {} {} {:.6} @ {:.6}  {} ({})",
                o.order_id,
                o.side,
                o.qty,
                o.price.unwrap_or(0.0),
                o.status,
                o.client_id
            );
        }
    }
    Ok(())
}

fn read_plan(args: &Args) -> Result<serde_json::Value, String> {
    let path = args
        .get("from")
        .ok_or_else(|| "--from is required: rungbot plan --json > plan.json".to_string())?;
    let raw = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("{path} is not a rungbot plan: {e}"))
}

fn caps_from(args: &Args) -> Result<Caps, String> {
    let d = Caps::default();
    Ok(Caps {
        max_order_quote: args.num("max-order", d.max_order_quote)?,
        max_daily_notional: args.num("max-daily", d.max_daily_notional)?,
        max_daily_orders: args.num("max-orders", d.max_daily_orders as f64)? as usize,
        max_slippage_pct: args.num("max-slippage", d.max_slippage_pct)?,
        min_order_quote: d.min_order_quote,
    })
}

fn prepare(args: &Args) -> Result<(Vec<Planned>, Caps, f64), String> {
    let plan = read_plan(args)?;
    let budget = args.num("budget", 0.0)?;
    if budget <= 0.0 {
        return Err(
            "--budget is required: the ladder sizes in percent, so it cannot know \
                    how much a percent is worth without you saying"
                .into(),
        );
    }
    let map = parse_pair_map(args.get("pair-map"));
    Ok((
        planned_orders(&plan, &map, budget)?,
        caps_from(args)?,
        budget,
    ))
}

/// The journal id for a planned order: deterministic, and already in the shape every
/// venue accepts.
fn planned_cid(p: &Planned, at: f64) -> String {
    let raw = journal::client_id(&p.sym, p.side, at, p.rung);
    ids::safe_cid(&raw).unwrap_or(raw)
}

fn cmd_plan(args: &Args) -> Result<(), String> {
    let (orders, caps, budget) = prepare(args)?;
    let jpath = journal_path(args);
    let j = store::load_journal(&jpath)?;
    let day_ago = now() - 86_400.0;

    println!(
        "budget {budget:.2} · per-order cap {:.2} · daily cap {:.2} / {} orders",
        caps.max_order_quote, caps.max_daily_notional, caps.max_daily_orders
    );
    println!();
    if orders.is_empty() {
        println!("the plan has no buys or sells. Nothing to place.");
        return Ok(());
    }
    for p in &orders {
        let cid = planned_cid(p, now());
        let intent = Intent {
            sym: p.sym.clone(),
            quote: p.quote,
            decided_price: p.price,
            venue_price: p.price,
        };
        // Deliberately not live here: this is the dry view, whatever the flags say.
        let ctx = Context {
            mode: Mode::Live,
            halted: halt_path().exists(),
            acknowledged: true,
            today_notional: j.notional_since(day_ago),
            today_orders: j.count_since(day_ago),
        };
        let verdict = match guard::check(&intent, ctx, caps) {
            Ok(()) if j.exists(&cid) => "already placed (idempotent)".to_string(),
            Ok(()) => "would place".to_string(),
            Err(r) => format!("REFUSED: {r}"),
        };
        println!(
            "  {:<5} {:<8} {:>10.6} @ {:>12.6}  ≈ {:.2}  {verdict}",
            match p.side {
                Side::Buy => "BUY",
                Side::Sell => "SELL",
            },
            p.pair,
            if p.price > 0.0 {
                p.quote / p.price
            } else {
                0.0
            },
            p.price,
            p.quote
        );
    }
    println!("\nNothing was sent. Add --live --i-understand to place these.");
    Ok(())
}

/// Round to the venue's precision and check its minimums before anything is journaled.
fn fit(venue: &dyn Venue, p: &Planned, base: f64) -> Result<(), String> {
    let limits = venue.limits(&p.pair).map_err(|e| e.to_string())?;
    let amount = venue
        .round_amount(&p.pair, base)
        .map_err(|e| e.to_string())?;
    let price = venue
        .round_price(&p.pair, p.price)
        .map_err(|e| e.to_string())?;
    if amount <= 0.0 || price <= 0.0 {
        return Err(format!(
            "rounded to {amount} @ {price}, which is not an order"
        ));
    }
    if amount < limits.min_base {
        return Err(format!(
            "{amount} is below the venue minimum of {} on {}",
            limits.min_base, p.pair
        ));
    }
    if amount * price < limits.min_quote {
        return Err(format!(
            "{:.4} is below the venue's minimum order value of {} on {}",
            amount * price,
            limits.min_quote,
            p.pair
        ));
    }
    Ok(())
}

fn cmd_sync(args: &Args) -> Result<(), String> {
    let (orders, caps, _) = prepare(args)?;
    let mode = if args.has("live") {
        Mode::Live
    } else {
        Mode::Dry
    };
    if mode != Mode::Live {
        return Err("sync without --live does nothing; use `plan` to preview".into());
    }
    let exch = venue_name(args)?;
    let jpath = journal_path(args);
    let _lock = lock_journal(&jpath, "rungbot-exec sync")?;
    let mut j = store::load_journal(&jpath)?;
    let clients = Clients::new();
    let venue = clients.get(exch)?;
    let day_ago = now() - 86_400.0;
    let (mut placed, mut refused, mut skipped) = (0, 0, 0);

    for p in &orders {
        let ts = now();
        let cid = planned_cid(p, ts);
        if j.exists(&cid) {
            skipped += 1;
            println!("  {} {} — already journaled, not re-placed", p.sym, cid);
            continue;
        }
        let intent = Intent {
            sym: p.sym.clone(),
            quote: p.quote,
            decided_price: p.price,
            venue_price: p.price,
        };
        let ctx = Context {
            mode,
            halted: halt_path().exists(),
            acknowledged: args.has("i-understand"),
            today_notional: j.notional_since(day_ago),
            today_orders: j.count_since(day_ago),
        };
        if let Err(r) = guard::check(&intent, ctx, caps) {
            println!("  {} — REFUSED: {r}", p.sym);
            refused += 1;
            if matches!(
                r,
                Refusal::Halted | Refusal::NotAcknowledged | Refusal::NotLive(_)
            ) {
                break; // these will refuse everything else too
            }
            continue;
        }

        let base = if p.price > 0.0 {
            p.quote / p.price
        } else {
            0.0
        };
        // Only now, with the rails satisfied, does anything reach the venue.
        if let Err(e) = fit(venue, p, base) {
            eprintln!("  {} — {e}", p.sym);
            refused += 1;
            continue;
        }

        // Journal BEFORE the venue call. A crash after this point is safe; a crash
        // before it means the order was never sent.
        j.record(Order {
            client_id: cid.clone(),
            sym: p.sym.clone(),
            exch: exch.into(),
            pair: p.pair.clone(),
            side: p.side.as_str().into(),
            kind: p.kind.clone(),
            status: "pending".into(),
            quote: Some(p.quote),
            price: Some(p.price),
            base: Some(base),
            rung: Some(p.rung),
            ts: Some(ts),
            ..Default::default()
        });
        store::save_journal(&jpath, &j)?;

        let resp = match p.side {
            Side::Buy => venue.limit_buy(&p.pair, base, p.price, Some(&cid)),
            Side::Sell => venue.limit_sell(&p.pair, base, p.price, Some(&cid)),
        };
        match resp.and_then(|r| venue.parse_order(&r)) {
            Ok(o) => {
                j.update(&cid, |x| {
                    x.status = if o.status.is_empty() {
                        "open".into()
                    } else {
                        o.status.clone()
                    };
                    x.order_id = Some(o.order_id.clone());
                });
                placed += 1;
                println!("  {} {} placed as {}", p.sym, cid, o.order_id);
            }
            Err(e) => {
                j.update(&cid, |x| {
                    x.status = "error".into();
                    x.last_error = Some(e.to_string());
                });
                refused += 1;
                eprintln!("  {} {cid} FAILED: {e}", p.sym);
            }
        }
        store::save_journal(&jpath, &j)?;
    }

    println!("\nplaced {placed} · refused {refused} · already journaled {skipped}");
    Ok(())
}

fn cmd_reconcile(args: &Args) -> Result<(), String> {
    let jpath = journal_path(args);
    let _lock = lock_journal(&jpath, "rungbot-exec reconcile")?;
    let mut j = store::load_journal(&jpath)?;
    let clients = Clients::new();
    let r = reconcile::reconcile(&mut j, &clients, now());
    if r.changed {
        store::save_journal(&jpath, &j)?;
    }
    for o in &r.filled {
        println!(
            "  filled{} {} {} {} {:.8} for {:.2} @ {}",
            if o.partial == Some(true) {
                " (part, then left the book)"
            } else {
                ""
            },
            o.client_id,
            o.side,
            o.sym,
            o.filled_base.unwrap_or(0.0),
            o.filled_quote.unwrap_or(0.0),
            o.avg_price
                .map(|p| format!("{p:.8}"))
                .unwrap_or_else(|| "?".into())
        );
    }
    for a in &r.adopted {
        println!(
            "  adopted {} onto venue order {} (was {:.2}, now {:.2})",
            a.order.client_id,
            a.order.order_id.as_deref().unwrap_or(""),
            a.was_quote,
            a.order.quote.unwrap_or(0.0)
        );
    }
    let errors = j
        .open_orders(None)
        .into_iter()
        .filter(|o| o.last_error.is_some())
        .count();
    println!(
        "\nfilled {} · adopted {} · poll errors {errors}",
        r.filled.len(),
        r.adopted.len()
    );
    Ok(())
}

fn cmd_cancel(args: &Args) -> Result<(), String> {
    let pair = args
        .get("pair")
        .ok_or_else(|| "--pair is required".to_string())?;
    let clients = Clients::new();
    let venue = clients.get(venue_name(args)?)?;
    let open = venue.open_orders(pair).map_err(|e| e.to_string())?;
    if open.is_empty() {
        println!("nothing resting for {pair}");
        return Ok(());
    }
    if !(args.has("live") && args.has("i-understand")) {
        println!("{} resting order(s) for {pair}:", open.len());
        for o in &open {
            println!(
                "  {} {} {:.6} @ {:.6}",
                o.order_id,
                o.side,
                o.qty,
                o.price.unwrap_or(0.0)
            );
        }
        println!("\nNothing cancelled. Add --live --i-understand to cancel these.");
        return Ok(());
    }
    let jpath = journal_path(args);
    let _lock = lock_journal(&jpath, "rungbot-exec cancel")?;
    let mut j = store::load_journal(&jpath)?;
    for o in &open {
        if let Err(e) = venue.cancel(pair, &o.order_id) {
            eprintln!("  {} failed: {e} (may have just filled)", o.order_id);
            continue;
        }
        let cid = j
            .orders
            .values()
            .find(|x| x.exch == venue.name() && x.order_id.as_deref() == Some(&o.order_id))
            .map(|x| x.client_id.clone());
        let Some(cid) = cid else {
            println!("  cancelled {} (not in the journal)", o.order_id);
            continue;
        };
        j.update(&cid, |x| {
            x.status = "canceled".into();
            x.note = Some("manual --cancel".into());
        });
        // The order's final state, read once: what filled before the cancel is a fill.
        match reconcile::settle_cancel(&mut j, venue, &cid, now()) {
            Settled::Booked(b) => println!(
                "  cancelled {cid} after {:.8} filled ({:.2} quote); booked on the next reconcile",
                b.filled_base.unwrap_or(0.0),
                b.filled_quote.unwrap_or(0.0)
            ),
            Settled::Unreadable(e) => {
                println!("  cancelled {cid}; its final fill could not be read: {e}")
            }
            _ => println!("  cancelled {cid}"),
        }
        store::save_journal(&jpath, &j)?;
    }
    store::save_journal(&jpath, &j)?;
    Ok(())
}

fn cmd_archive(args: &Args) -> Result<(), String> {
    let days = args.num("days", 30.0)?;
    let jpath = journal_path(args);
    let _lock = lock_journal(&jpath, "rungbot-exec archive")?;
    let mut j = store::load_journal(&jpath)?;
    let moved = j.archive_old(days, now());
    if moved.is_empty() {
        println!("nothing to archive");
        return Ok(());
    }
    // The archive first: a crash between the two leaves a row in both, never in neither.
    let apath = store::archive_path(&jpath);
    store::append_archive(&apath, &moved)?;
    store::save_journal(&jpath, &j)?;
    println!(
        "archived {} finished cancel row(s) older than {days}d to {}",
        moved.len(),
        apath.display()
    );
    Ok(())
}

fn cmd_import(args: &Args) -> Result<(), String> {
    let dir = args
        .get("sub")
        .ok_or_else(|| "usage: rungbot-exec import-cex DIR [--write [--force]]".to_string())?;
    if args.has("config") {
        print!(
            "{}",
            rungbot_exec::run::cexconfig::generate(Path::new(dir))?
        );
        return Ok(());
    }
    let imported = import::read_dir(Path::new(dir))?;
    for line in imported.report.lines() {
        println!("{line}");
    }
    let jpath = journal_path(args);
    if !args.has("write") {
        println!(
            "\ndry run: nothing written. Add --write to write {}",
            jpath.display()
        );
        return Ok(());
    }
    if !imported.report.mismatches.is_empty() && !args.has("force") {
        return Err(
            "some rows would not read back as written; pass --force to import anyway".into(),
        );
    }
    let _lock = lock_journal(&jpath, "rungbot-exec import-cex")?;
    let apath = import::write(&imported, &jpath, args.has("force"))?;
    println!(
        "\nwrote {} ({} rows) and {} ({} rows)",
        jpath.display(),
        imported.journal.orders.len(),
        apath.display(),
        imported.archive.len()
    );
    for (name, present) in [
        (store::LADDER_FILE, imported.ladder.is_some()),
        (store::PNL_FILE, imported.pnl.is_some()),
        (store::TTL_FILE, imported.ttl.is_some()),
        (import::DECISIONS_FILE, imported.decisions.is_some()),
        (import::NOTICES_FILE, imported.notices.is_some()),
        (import::BTC_ALERT_FILE, imported.btc_alert.is_some()),
        (import::DEPLOY_FILE, imported.deploy.is_some()),
        (import::AUDIT_FILE, imported.audit.is_some()),
    ] {
        if present {
            println!("wrote {}", store::sibling(&jpath, name).display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_json() -> serde_json::Value {
        serde_json::json!({
            "buys": [{ "sym": "XYZ", "rung": 1, "pct": 10.0, "price": 100.0 }],
            "sells": [{ "sym": "ETH", "rung": 2, "pct": 15.0, "price": 2000.0 }]
        })
    }

    #[test]
    fn a_plan_becomes_orders_sized_against_the_budget() {
        let map = parse_pair_map(Some("XYZ=XYZ_USDT,ETH=ETH_USDT"));
        let got = planned_orders(&plan_json(), &map, 1000.0).unwrap();
        assert_eq!(got.len(), 2);
        let buy = got.iter().find(|p| p.side == Side::Buy).unwrap();
        assert_eq!(buy.quote, 100.0, "10% of a 1000 budget");
        assert_eq!(buy.pair, "XYZ_USDT");
        let sell = got.iter().find(|p| p.side == Side::Sell).unwrap();
        assert_eq!(sell.quote, 150.0);
        assert_eq!(sell.kind, "ladder_sell");
    }

    #[test]
    fn a_coin_with_no_pair_mapping_is_an_error_not_a_guess() {
        let e = planned_orders(&plan_json(), &BTreeMap::new(), 1000.0).unwrap_err();
        assert!(e.contains("--pair-map"), "{e}");
    }

    #[test]
    fn an_empty_plan_is_not_an_error() {
        let empty = serde_json::json!({ "buys": [], "sells": [] });
        assert!(planned_orders(&empty, &BTreeMap::new(), 100.0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_pair_map_is_case_insensitive_on_the_symbol() {
        let m = parse_pair_map(Some("xyz=XYZ_USDT"));
        assert_eq!(m.get("XYZ").map(String::as_str), Some("XYZ_USDT"));
    }

    #[test]
    fn flags_parse_in_both_spellings_and_demand_their_values() {
        let argv: Vec<String> = ["sync", "--from", "p.json", "--live", "--max-order=25"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = Args::parse(&argv).unwrap();
        assert_eq!(a.cmd, "sync");
        assert_eq!(a.get("from"), Some("p.json"));
        assert!(a.has("live"));
        assert_eq!(a.num("max-order", 50.0).unwrap(), 25.0);

        let bad: Vec<String> = ["sync", "--from"].iter().map(|s| s.to_string()).collect();
        assert!(Args::parse(&bad).is_err());
    }

    #[test]
    fn caps_default_small_and_can_be_raised_explicitly() {
        let a = Args::parse(&["plan".to_string()]).unwrap();
        assert_eq!(caps_from(&a).unwrap().max_order_quote, 50.0);
        let b = Args::parse(&["plan".into(), "--max-order".into(), "500".into()]).unwrap();
        assert_eq!(caps_from(&b).unwrap().max_order_quote, 500.0);
    }
}
