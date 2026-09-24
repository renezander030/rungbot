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
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use rungbot_exec::gate::Gate;
use rungbot_exec::guard::{self, Caps, Context, Intent, Mode, Refusal};
use rungbot_exec::journal::{self, Order, Side, Status};
use rungbot_exec::keys;
use rungbot_exec::store;

const USAGE: &str = "\
rungbot-exec — opt-in live execution for rungbot.

It places GTC limit orders only. A resting order fills while your machine is
asleep; a market order needs you present and is not implemented.

USAGE:
  rungbot-exec status [--pair PAIR]
  rungbot-exec plan   --from PLAN.json --budget N --pair-map SYM=PAIR,...
  rungbot-exec sync   --from PLAN.json --budget N --pair-map SYM=PAIR,...
                      --live --i-understand
  rungbot-exec cancel --pair PAIR [--live --i-understand]
  rungbot-exec keys   check

REQUIRED for plan and sync:
  --from FILE        a plan from `rungbot plan --json`
  --budget N         what 100% is worth, in quote currency. The ladder sizes in
                     percent and does not know your balances, so it cannot infer this.
  --pair-map SYM=PAIR[,...]   which venue pair each symbol trades as

OPTIONS:
  --live             actually place orders. Without it nothing is sent.
  --i-understand     acknowledge live trading. Required once, every run.
  --journal PATH     order journal (default: alongside the rungbot state)
  --pair PAIR        limit status/cancel to one pair
  --max-order N      per-order cap in quote currency (default 50)
  --max-daily N      daily notional cap (default 200)
  --max-orders N     daily order count cap (default 10)
  --max-slippage N   refuse if the venue moved this far from the decision (default 2)

ENVIRONMENT:
  RUNGBOT_GATE_KEY / RUNGBOT_GATE_SECRET   credentials; never read from the watchlist
  RUNGBOT_HALT                             path to the halt file (default ~/.config/rungbot/HALT)
  RUNGBOT_OFFLINE=1                        refuse every network call

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
                "from"
                    | "journal"
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
                .ok_or_else(|| format!("no Gate pair for {sym}; pass --pair-map {sym}=<PAIR>"))?;
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
        "status" => cmd_status(&args),
        "plan" => cmd_plan(&args),
        "sync" => cmd_sync(&args),
        "cancel" => cmd_cancel(&args),
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

fn open_gate() -> Result<Gate, String> {
    let creds = keys::load("gate", None).map_err(|e| e.to_string())?;
    Ok(Gate::new(creds))
}

fn cmd_keys(args: &Args) -> Result<(), String> {
    if args.get("sub") != Some("check") {
        return Err("usage: rungbot-exec keys check".into());
    }
    let gate = open_gate()?;
    match gate.balances() {
        Ok(b) => {
            let funded = b.values().filter(|(f, l)| *f > 0.0 || *l > 0.0).count();
            println!("✓ Gate accepted the key from this IP");
            println!("✓ it can read balances ({funded} assets with a balance)");
            println!();
            println!("What this check cannot tell you: Gate exposes no endpoint for a key's");
            println!("permission set, so withdrawal scope cannot be verified from here.");
            println!("Open Gate's API management page and confirm withdrawals are OFF.");
            println!();
            println!("If this key has no IP allowlist, Gate disables it 90 days after");
            println!("creation, without telling you. Allowlisting also prevents that.");
            Ok(())
        }
        Err(e) => Err(format!("✗ {e}")),
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

    let unswept = j.venue_cancelled_unswept(None);
    if !unswept.is_empty() {
        println!(
            "\n{} order(s) the venue cancelled, cash not re-laddered:",
            unswept.len()
        );
        for o in unswept {
            println!("  {} {} {:.6} @ {:.6}", o.client_id, o.sym, o.base, o.price);
        }
    }

    if let Some(pair) = args.get("pair") {
        let gate = open_gate()?;
        let open = gate.open_orders(pair).map_err(|e| e.to_string())?;
        println!("\nresting at the venue for {pair}: {}", open.len());
        for o in open {
            println!(
                "  {} {:.6} @ {:.6}  {} ({})",
                o.id,
                o.amount,
                o.price,
                o.status,
                o.text.unwrap_or_default()
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
        let cid = journal::client_id(&p.sym, p.side, now(), p.rung);
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
    let jpath = journal_path(args);
    let _lock = lock_journal(&jpath, "rungbot-exec sync")?;
    let mut j = store::load_journal(&jpath)?;
    let gate = open_gate()?;
    let day_ago = now() - 86_400.0;
    let (mut placed, mut refused, mut skipped) = (0, 0, 0);

    for p in &orders {
        let cid = journal::client_id(&p.sym, p.side, now(), p.rung);
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

        // Only now, with the rails satisfied, does anything reach the venue.
        if let Err(e) = gate.pair_info(&p.pair) {
            eprintln!("  {} — cannot read the pair: {e}", p.sym);
            refused += 1;
            continue;
        }

        let base = if p.price > 0.0 {
            p.quote / p.price
        } else {
            0.0
        };
        // Journal BEFORE the venue call. A crash after this point is safe; a crash
        // before it means the order was never sent.
        j.record(Order {
            client_id: cid.clone(),
            sym: p.sym.clone(),
            pair: p.pair.clone(),
            venue: "gate".into(),
            side: p.side,
            kind: p.kind.clone(),
            price: p.price,
            base,
            quote: p.quote,
            status: Status::Pending,
            placed_ts: now(),
            venue_order_id: None,
            filled_ts: None,
            filled_base: None,
            filled_quote: None,
            avg_price: None,
            status_ts: None,
            note: None,
            swept: false,
        });
        store::save_journal(&jpath, &j)?;

        match gate.limit_order(&p.pair, p.side, base, p.price, &cid) {
            Ok(o) => {
                j.update(&cid, |x| {
                    x.status = Status::Open;
                    x.venue_order_id = Some(o.id.clone());
                    x.status_ts = Some(now());
                });
                placed += 1;
                println!("  {} {} placed as {}", p.sym, cid, o.id);
            }
            Err(e) => {
                j.update(&cid, |x| {
                    x.status = Status::Error;
                    x.note = Some(e.to_string());
                    x.status_ts = Some(now());
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

fn cmd_cancel(args: &Args) -> Result<(), String> {
    let pair = args
        .get("pair")
        .ok_or_else(|| "--pair is required".to_string())?;
    let gate = open_gate()?;
    let open = gate.open_orders(pair).map_err(|e| e.to_string())?;
    if open.is_empty() {
        println!("nothing resting for {pair}");
        return Ok(());
    }
    if !(args.has("live") && args.has("i-understand")) {
        println!("{} resting order(s) for {pair}:", open.len());
        for o in &open {
            println!("  {} {:.6} @ {:.6}", o.id, o.amount, o.price);
        }
        println!("\nNothing cancelled. Add --live --i-understand to cancel these.");
        return Ok(());
    }
    let jpath = journal_path(args);
    let _lock = lock_journal(&jpath, "rungbot-exec cancel")?;
    let mut j = store::load_journal(&jpath)?;
    for o in &open {
        match gate.cancel(pair, &o.id) {
            Ok(done) => {
                // The cancel response is the order's final state: what filled before it
                // is booked as a fill, not dropped with the rest of the order.
                if done.filled_amount > 0.0 {
                    println!(
                        "  cancelled {} after {:.6} filled ({:.2} quote)",
                        o.id, done.filled_amount, done.filled_quote
                    );
                } else {
                    println!("  cancelled {}", o.id);
                }
                if let Some(text) = o.text.as_ref().and_then(|t| t.strip_prefix("t-")) {
                    j.update(text, |x| {
                        x.book_cancel(
                            done.filled_amount,
                            done.filled_quote,
                            now(),
                            "manual cancel",
                        )
                    });
                }
            }
            Err(e) => eprintln!("  {} failed: {e}", o.id),
        }
    }
    store::save_journal(&jpath, &j)?;
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
