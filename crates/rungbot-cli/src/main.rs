//! `rungbot` — a dip-buy / take-profit ladder for spot crypto. Notify-only.
//!
//! Text output by default, `--json` for machines. The strategy itself lives in
//! `rungbot-core`, which this binary and a Cloudflare Worker share unchanged.

mod config_file;
mod klines;
mod notify;
mod report;
mod state;
mod tickers;
mod yaml;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use rungbot_core::{analyze_with, decisions, iso8601, regime as rg, Market, Notices, Price, Steer};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
rungbot — a dip-buy / take-profit ladder for spot crypto.
Notify-only: it holds no API keys and cannot place an order.

USAGE:
  rungbot init    [--config PATH] [--force]
  rungbot plan    [--config PATH] [--state PATH] [--save] [--fresh]
                  [--prices FILE] [--now EPOCH] [--json]
  rungbot tickers [--config PATH] [--json]
  rungbot regime  [--config PATH] [--json]
  rungbot --version | --help

PLAN OPTIONS:
  --save     persist the advanced ladder (default: changes nothing)
  --fresh    ignore prior state
  --prices   read prices from a JSON file instead of the network
  --now      epoch seconds, for reproducible runs
  --steer    read the market regime and apply the sell policy
  --armed    comma-separated coins to force a one-shot armed exit on
  --notify   send the result to the configured webhook or Telegram

ENVIRONMENT:
  RUNGBOT_OFFLINE=1   refuse every network call
  RUNGBOT_CONFIG      default config path
  RUNGBOT_STATE       default state path
  RUNGBOT_ARMED       comma-separated coins to arm
  RUNGBOT_TELEGRAM_TOKEN   bot token; never read from the config file
";

/// Minimal flag parser: `--key value`, `--key=value`, and bare switches.
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
                return Err(format!("unexpected argument {arg:?}"));
            };
            if let Some((k, v)) = bare.split_once('=') {
                flags.insert(k.to_string(), v.to_string());
                continue;
            }
            let takes_value = matches!(bare, "config" | "state" | "prices" | "now" | "armed");
            let value = if takes_value {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("--{bare} needs a value"))?
            } else {
                "1".to_string()
            };
            flags.insert(bare.to_string(), value);
        }
        Ok(Args { cmd, flags })
    }

    fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(|s| s.as_str())
    }

    fn path(&self, k: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
        self.get(k).map(PathBuf::from).unwrap_or_else(default)
    }
}

/// Exit codes: 2 = bad config, 3 = price feed, 1 = anything else.
fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" || argv[0] == "help" {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if argv[0] == "--version" || argv[0] == "-V" {
        println!("rungbot {VERSION}");
        return ExitCode::SUCCESS;
    }

    let args = match Args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            return ExitCode::from(1);
        }
    };

    let result = match args.cmd.as_str() {
        "init" => cmd_init(&args),
        "plan" => cmd_plan(&args),
        "tickers" => cmd_tickers(&args),
        "regime" => cmd_regime(&args),
        other => {
            eprintln!("unknown command {other:?}\n\n{USAGE}");
            return ExitCode::from(1);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Config(m)) => {
            eprintln!("config error: {m}");
            ExitCode::from(2)
        }
        Err(Failure::Prices(m)) => {
            eprintln!("price feed error: {m}");
            ExitCode::from(3)
        }
        Err(Failure::Other(m)) => {
            eprintln!("{m}");
            ExitCode::from(1)
        }
    }
}

enum Failure {
    Config(String),
    Prices(String),
    Other(String),
}

fn cmd_init(args: &Args) -> Result<(), Failure> {
    let path = args.path("config", state::default_config_path);
    if path.exists() && !args.has("force") {
        return Err(Failure::Other(format!(
            "refusing to overwrite {} (use --force)",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Failure::Other(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(&path, config_file::EXAMPLE)
        .map_err(|e| Failure::Other(format!("cannot write {}: {e}", path.display())))?;
    println!("wrote {}", path.display());
    println!(
        "Edit it, then run: rungbot plan --config {}",
        path.display()
    );
    Ok(())
}

fn load_config(args: &Args) -> Result<config_file::CliConfig, Failure> {
    let path = args.path("config", state::default_config_path);
    config_file::load(&path).map_err(|e| Failure::Config(e.0))
}

fn cmd_tickers(args: &Args) -> Result<(), Failure> {
    let cfg = load_config(args)?;
    let prices = tickers::fetch(&cfg.core.coins, 3).map_err(|e| Failure::Prices(e.to_string()))?;
    if args.has("json") {
        let body = serde_json::to_string_pretty(&prices)
            .map_err(|e| Failure::Other(format!("cannot render JSON: {e}")))?;
        println!("{body}");
        return Ok(());
    }
    for coin in &cfg.core.coins {
        match prices.get(&coin.symbol) {
            Some(p) => println!(
                "{:<7} {:>14} {:>+8.2}%  {}:{}",
                coin.symbol,
                report::fmt_price(Some(p.price)),
                p.chg_24h.unwrap_or(0.0),
                coin.venue.as_str(),
                coin.pair
            ),
            None => println!(
                "{:<7} {:>14}  (no price from {}:{})",
                coin.symbol,
                "-",
                coin.venue.as_str(),
                coin.pair
            ),
        }
    }
    Ok(())
}

fn cmd_plan(args: &Args) -> Result<(), Failure> {
    let cfg = load_config(args)?;
    let spath = args.path("state", state::default_path);

    let prices: BTreeMap<String, Price> = match args.get("prices") {
        Some(file) => {
            let raw = std::fs::read_to_string(file)
                .map_err(|e| Failure::Other(format!("cannot read {file}: {e}")))?;
            serde_json::from_str(&raw)
                .map_err(|e| Failure::Other(format!("{file}: not a price map: {e}")))?
        }
        None => tickers::fetch(&cfg.core.coins, 3).map_err(|e| Failure::Prices(e.to_string()))?,
    };

    let now = match args.get("now") {
        Some(v) => v
            .parse::<f64>()
            .map_err(|e| Failure::Other(format!("--now must be epoch seconds: {e}")))?,
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .map_err(|e| Failure::Other(format!("system clock is before 1970: {e}")))?,
    };

    // Steering is on when a sell policy is configured, or when asked for explicitly.
    // It costs one candle request per coin, so it is not on by default.
    let want_steer = args.has("steer") || cfg.sellpolicy.is_some();
    let (steer, regime) = if want_steer {
        let r = read_regime(&cfg)?;
        (build_steer(&cfg, &r, args), Some(r))
    } else {
        (Steer::default(), None)
    };

    let prior = if args.has("fresh") {
        Default::default()
    } else {
        state::load(&spath)
    };
    let out = analyze_with(&cfg.core, &prices, &prior, &steer, now);
    let now_iso = iso8601(now);
    let log = decisions::from_outcome(&out, now);

    if args.has("json") {
        let body = serde_json::json!({
            "generated": now_iso,
            "market": regime.as_ref().map(|r| r.market.as_str()),
            "buys": out.buys,
            "sells": out.sells,
            "rows": out.rows,
            "errors": out.errors,
            "skips": out.skips,
            "decisions": log,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&body)
                .map_err(|e| Failure::Other(format!("cannot render JSON: {e}")))?
        );
    } else {
        println!(
            "{}",
            report::render(&out, &cfg.core, regime.as_ref(), &log, &now_iso)
        );
    }

    if args.has("save") {
        state::save(&spath, &out.state);
        state::append_decisions(&state::decisions_path(&spath), &log);
    } else if !args.has("json") {
        println!(
            "\n(state not saved; pass --save to advance the ladder at {})",
            spath.display()
        );
    }

    if args.has("notify") {
        notify_run(&cfg, &out, &spath, now, args.has("json"))?;
    }
    Ok(())
}

/// Fetch candles and read the market. One request per coin, plus one for BTC.
fn read_regime(cfg: &config_file::CliConfig) -> Result<rg::Regime, Failure> {
    let mut series = Vec::with_capacity(cfg.core.coins.len());
    for coin in &cfg.core.coins {
        let (venue, pair) =
            klines::kline_source(coin, cfg.klines.get(&coin.symbol).map(|s| s.as_str()))
                .map_err(Failure::Config)?;
        // A coin whose candles fail is reported as not running rather than aborting the
        // run: one dead feed must not cost you the whole report.
        let closes = klines::closes(venue, &pair, rg::KLINE_DAYS).unwrap_or_default();
        series.push((coin.symbol.clone(), closes));
    }
    let btc =
        klines::closes(rungbot_core::Venue::Binance, "BTCUSDT", rg::KLINE_DAYS).unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    Ok(rg::assess(&series, &btc, cfg.regime, now))
}

fn comma_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_ascii_uppercase())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Turn a regime reading into the steering input for one run.
fn build_steer(cfg: &config_file::CliConfig, r: &rg::Regime, args: &Args) -> Steer {
    let armed = args
        .get("armed")
        .map(String::from)
        .or_else(|| std::env::var("RUNGBOT_ARMED").ok())
        .map(|s| comma_list(&s))
        .unwrap_or_default();

    // The bull policy governs only in a confirmed bull, and only coins with a basis.
    let policy_coins: Vec<String> = if r.market == Market::Bull {
        cfg.core
            .coins
            .iter()
            .filter(|c| c.entry.is_some())
            .map(|c| c.symbol.clone())
            .collect()
    } else {
        Vec::new()
    };

    Steer {
        policy: cfg.sellpolicy.clone(),
        policy_coins,
        armed,
        trail_coins: r.running_syms().into_iter().map(String::from).collect(),
    }
}

fn notify_run(
    cfg: &config_file::CliConfig,
    out: &rungbot_core::Outcome,
    spath: &std::path::Path,
    now: f64,
    quiet: bool,
) -> Result<(), Failure> {
    if !cfg.notify.is_configured() {
        return Err(Failure::Config(
            "--notify needs a `notify:` section with a webhook_url or telegram_chat_id".into(),
        ));
    }
    // Dedupe: one message per new signal, one reminder a day, never a 30-minute loop.
    let npath = state::notices_path(spath);
    let mut notices: Notices = state::load_notices(&npath);
    let live: Vec<String> = out
        .buys
        .iter()
        .chain(out.sells.iter())
        .map(|t| format!("{}:{:?}:{}", t.row.sym, t.side, t.rung))
        .chain(out.errors.iter().cloned())
        .collect();
    let fresh = notices.filter(&live, now, rungbot_core::notices::DEFAULT_REMIND_AFTER_S);

    if fresh.is_empty() {
        if !quiet {
            println!("\n(notify: nothing new since the last message)");
        }
        state::save_notices(&npath, &notices);
        return Ok(());
    }

    let text = notify::summary(out);
    for (channel, res) in notify::send(&cfg.notify, out, &text) {
        match res {
            Ok(()) => {
                if !quiet {
                    println!("(notify: sent via {channel})");
                }
            }
            Err(e) => eprintln!("WARN: notify via {channel} failed: {e}"),
        }
    }
    state::save_notices(&npath, &notices);
    Ok(())
}

fn cmd_regime(args: &Args) -> Result<(), Failure> {
    let cfg = load_config(args)?;
    let r = read_regime(&cfg)?;
    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&r)
                .map_err(|e| Failure::Other(format!("cannot render JSON: {e}")))?
        );
        return Ok(());
    }
    println!("market: {}", r.market.as_str());
    if let (Some(px), Some(s100), Some(s200)) = (r.btc_price, r.btc_sma100, r.btc_sma200) {
        println!("BTC {px:.0}  sma100 {s100:.0}  sma200 {s200:.0}");
    }
    println!(
        "breadth above 30d SMA: {}/{}",
        r.breadth_above_sma30,
        r.coins.len()
    );
    println!();
    for c in &r.coins {
        let mark = if c.running { "RUN " } else { "    " };
        let detail = match &c.error {
            Some(e) => e.clone(),
            None => {
                let n = c.signals.named();
                if n.is_empty() {
                    "-".into()
                } else {
                    n.join(", ")
                }
            }
        };
        println!("{mark}{:<7} {}/4  {detail}", c.sym, c.signals.count());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_parse_in_both_spellings() {
        let argv: Vec<String> = ["plan", "--config", "/tmp/a.yaml", "--json", "--now=5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = Args::parse(&argv).unwrap();
        assert_eq!(a.cmd, "plan");
        assert_eq!(a.get("config"), Some("/tmp/a.yaml"));
        assert!(a.has("json"));
        assert_eq!(a.get("now"), Some("5"));
    }

    #[test]
    fn a_flag_missing_its_value_is_an_error() {
        let argv: Vec<String> = ["plan", "--config"].iter().map(|s| s.to_string()).collect();
        assert!(Args::parse(&argv).is_err());
    }
}
