//! `rungbot` — a dip-buy / take-profit ladder for spot crypto. Notify-only.
//!
//! Text output by default, `--json` for machines. The strategy itself lives in
//! `rungbot-core`, which this binary and a Cloudflare Worker share unchanged.

mod config_file;
mod report;
mod state;
mod tickers;
mod yaml;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use rungbot_core::{analyze, Price};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
rungbot — a dip-buy / take-profit ladder for spot crypto.
Notify-only: it holds no API keys and cannot place an order.

USAGE:
  rungbot init    [--config PATH] [--force]
  rungbot plan    [--config PATH] [--state PATH] [--save] [--fresh]
                  [--prices FILE] [--now EPOCH] [--json]
  rungbot tickers [--config PATH] [--json]
  rungbot --version | --help

PLAN OPTIONS:
  --save     persist the advanced ladder (default: changes nothing)
  --fresh    ignore prior state
  --prices   read prices from a JSON file instead of the network
  --now      epoch seconds, for reproducible runs

ENVIRONMENT:
  RUNGBOT_OFFLINE=1   refuse every network call
  RUNGBOT_CONFIG      default config path
  RUNGBOT_STATE       default state path
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
            let takes_value = matches!(bare, "config" | "state" | "prices" | "now");
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

fn load_config(args: &Args) -> Result<rungbot_core::Config, Failure> {
    let path = args.path("config", state::default_config_path);
    config_file::load(&path).map_err(|e| Failure::Config(e.0))
}

fn cmd_tickers(args: &Args) -> Result<(), Failure> {
    let cfg = load_config(args)?;
    let prices = tickers::fetch(&cfg.coins, 3).map_err(|e| Failure::Prices(e.to_string()))?;
    if args.has("json") {
        let body = serde_json::to_string_pretty(&prices)
            .map_err(|e| Failure::Other(format!("cannot render JSON: {e}")))?;
        println!("{body}");
        return Ok(());
    }
    for coin in &cfg.coins {
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
        None => tickers::fetch(&cfg.coins, 3).map_err(|e| Failure::Prices(e.to_string()))?,
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

    let prior = if args.has("fresh") {
        Default::default()
    } else {
        state::load(&spath)
    };
    let out = analyze(&cfg, &prices, &prior, now);
    let now_iso = iso8601(now);

    if args.has("json") {
        let body = serde_json::json!({
            "generated": now_iso,
            "buys": out.buys,
            "sells": out.sells,
            "rows": out.rows,
            "errors": out.errors,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&body)
                .map_err(|e| Failure::Other(format!("cannot render JSON: {e}")))?
        );
    } else {
        println!("{}", report::render(&out, &cfg, &now_iso));
    }

    if args.has("save") {
        state::save(&spath, &out.state);
    } else if !args.has("json") {
        println!(
            "\n(state not saved; pass --save to advance the ladder at {})",
            spath.display()
        );
    }
    Ok(())
}

/// UTC timestamp without pulling in a date library for one line of output.
fn iso8601(epoch: f64) -> String {
    let secs = epoch as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Civil-from-days (Howard Hinnant's algorithm), valid for the whole Gregorian range.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mth <= 2 { y + 1 } else { y };

    format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}+00:00")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_matches_known_timestamps() {
        assert_eq!(iso8601(0.0), "1970-01-01T00:00:00+00:00");
        assert_eq!(iso8601(1_700_000_000.0), "2023-11-14T22:13:20+00:00");
        assert_eq!(iso8601(1_789_919_316.0), "2026-09-20T15:48:36+00:00");
        // The three places hand-rolled date maths goes wrong: a leap day, the last
        // second of a year, and 2100 — a century that is not a leap year.
        assert_eq!(iso8601(1_709_164_800.0), "2024-02-29T00:00:00+00:00");
        assert_eq!(iso8601(1_767_225_599.0), "2025-12-31T23:59:59+00:00");
        assert_eq!(iso8601(4_107_542_400.0), "2100-03-01T00:00:00+00:00");
    }

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
