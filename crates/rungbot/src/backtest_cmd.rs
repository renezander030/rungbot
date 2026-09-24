//! `rungbot backtest ...` — the ladder replayed over history. Reads caches, fetches
//! public candles, writes reports; every calculation lives in `rungbot-backtest`.

use std::path::{Path, PathBuf};

use rungbot_backtest::monthly::{self, MonthlyOpts};
use rungbot_backtest::py::{loads, Py};
use rungbot_backtest::replay::alt_confirm::{self, AltCoin, LiveRungs};
use rungbot_backtest::replay::alt_top::{self, Ladder, Series};
use rungbot_backtest::replay::btc_confirm::{self, default_shapes, Btc};
use rungbot_backtest::replay::data::{self, Bar};
use rungbot_backtest::replay::microstate::{self, Orders, Watch};
use rungbot_backtest::replay::sellpolicy_replay::{self, Closes};
use rungbot_backtest::replay::{btc_confirm_analysis, btc_top};
use rungbot_backtest::sweep::{self, SweepOpts};
use rungbot_backtest::window::{self, WindowOpts};
use rungbot_backtest::{invariants, Book, History};
use rungbot_core::{time, Venue};

use crate::config_file::CliConfig;
use crate::{history, Args, Failure};

/// A neutral study file to start from: `rungbot backtest replay example > study.json`.
pub const STUDY_EXAMPLE: &str = include_str!("../assets/study.example.json");

pub const USAGE: &str = "\
rungbot backtest — the ladder replayed over history. Never trades.

USAGE:
  rungbot backtest invariants
  rungbot backtest window  --book FILE [--days N] [--bag USD] [--max-order USD]
                           [--history FILE] [--cache-dir DIR] [--refresh]
  rungbot backtest sweep   [--days N] [--book FILE] [--history FILE] [--bag USD]
  rungbot backtest monthly --book FILE [--windows 28,90,180] [--expect FILE]
                           [--dry-run] [--notify] [--drift-band PCT] [--no-percoin]
                           [--percoin-min-gain PP] [--sweep-bag USD] [--now EPOCH]
  rungbot backtest replay <study> [--study FILE] [--out DIR] [--cache-dir DIR]

REPLAY STUDIES:
  example               print a study file to start from
  fetch                 cache daily candles for every source in the study
                        (--sources a:B,c:D adds more; --force refetches)
  btc-confirm           BTC bull confirmations and the dip rungs after each
                        (--variants nobreadth,btcbreadth; --recent-from DATE)
  btc-confirm-analysis  follow-up on btc-confirm (--tranche USD)
  alt-confirm           do BTC confirmations transfer to the alts?
  alt-sweep             depth/weight profiles over alt-confirm's events
  alt-current           the live rungs against the current event
  btc-top               anatomy of BTC cycle tops (--cutoff DATE)
  alt-top               the alts at each cycle top, and the ladder's capture
  sellpolicy            the bull sell policy over btc-top's and alt-top's tops
  microstate            30-day microstate against the resting orders

The window, sweep and monthly runs read the config's coins and ladder. The start book
(--book) is JSON: {\"held\": {\"BTC\": 0.05}, \"stable\": {\"binance\": 800},
\"min_notional\": {\"BTC\": 5}}. A coin's price history comes from CoinGecko: set
`coingecko: <id>` on each coin whose venue is not coingecko.

ENVIRONMENT:
  RUNGBOT_BT_DAYS, RUNGBOT_BT_WINDOWS, RUNGBOT_BT_BAG, RUNGBOT_MAX_ORDER_USD,
  RUNGBOT_DRIFT_BAND_PCT, RUNGBOT_BT_PERCOIN=0, RUNGBOT_BT_PERCOIN_MIN_GAIN
";

fn other(e: impl std::fmt::Display) -> Failure {
    Failure::Other(e.to_string())
}

fn env_f(key: &str) -> Result<Option<f64>, Failure> {
    match std::env::var(key) {
        Ok(v) => v
            .trim()
            .parse::<f64>()
            .map(Some)
            .map_err(|e| Failure::Config(format!("{key}: {e}"))),
        Err(_) => Ok(None),
    }
}

fn flag_f(args: &Args, k: &str, env: &str) -> Result<Option<f64>, Failure> {
    match args.get(k) {
        Some(v) => v
            .parse::<f64>()
            .map(Some)
            .map_err(|e| Failure::Other(format!("--{k}: {e}"))),
        None => env_f(env),
    }
}

fn base_dir(env_key: &str, fallback: &str) -> PathBuf {
    match std::env::var(env_key) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(fallback),
    }
}

fn cache_dir(args: &Args) -> PathBuf {
    args.path("cache-dir", || {
        base_dir("XDG_CACHE_HOME", ".cache")
            .join("rungbot")
            .join("backtest")
    })
}

fn read(path: &Path) -> Result<String, Failure> {
    std::fs::read_to_string(path).map_err(|e| other(format!("cannot read {}: {e}", path.display())))
}

fn write(path: &Path, text: &str) -> Result<(), Failure> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| other(format!("cannot create {}: {e}", dir.display())))?;
    }
    // tmp-and-rename, so a killed run never leaves half a file behind
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)
        .map_err(|e| other(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| other(format!("cannot write {}: {e}", path.display())))
}

fn now_epoch(args: &Args) -> Result<i64, Failure> {
    match args.get("now") {
        Some(v) => v
            .parse::<f64>()
            .map(|x| x as i64)
            .map_err(|e| other(format!("--now must be epoch seconds: {e}"))),
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .map_err(other),
    }
}

fn days(args: &Args) -> Result<i64, Failure> {
    Ok(flag_f(args, "days", "RUNGBOT_BT_DAYS")?.unwrap_or(28.0) as i64)
}

fn load_book(path: &Path) -> Result<Book, Failure> {
    Book::parse(&read(path)?).map_err(|e| other(format!("{}: {e}", path.display())))
}

/// `coin -> CoinGecko id`, in config order. A coin on the coingecko venue is its own id.
fn coingecko_ids(cfg: &CliConfig) -> Result<Vec<(String, String)>, Failure> {
    cfg.core
        .coins
        .iter()
        .map(|c| {
            let id = match (cfg.coingecko.get(&c.symbol), c.venue) {
                (Some(id), _) => id.clone(),
                (None, Venue::Coingecko) => c.pair.clone(),
                _ => {
                    return Err(Failure::Config(format!(
                        "coin {}: set `coingecko: <id>` for its price history",
                        c.symbol
                    )))
                }
            };
            Ok((c.symbol.clone(), id))
        })
        .collect()
}

/// A window's price history: the file given, else a cache under a day old, else fetched
/// (and cached). `Err` is the line the window report prints.
fn window_history(
    args: &Args,
    cfg: &CliConfig,
    days: i64,
) -> Result<Result<History, String>, Failure> {
    if let Some(file) = args.get("history") {
        return Ok(Ok(History::parse(&read(Path::new(file))?).map_err(other)?));
    }
    let cache = cache_dir(args).join(format!("bt-hist-{days}d.json"));
    let fresh = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < 86_400);
    if fresh && !args.has("refresh") {
        return Ok(Ok(History::parse(&read(&cache)?).map_err(other)?));
    }
    let ids = coingecko_ids(cfg)?;
    Ok(match history::window_history(&ids, days) {
        Ok(h) => {
            write(&cache, &h.to_json())?;
            Ok(h)
        }
        Err(line) => Err(line),
    })
}

fn cmd_invariants() -> Result<(), Failure> {
    let (text, ok) = invariants::run();
    print!("{text}");
    if ok {
        Ok(())
    } else {
        Err(Failure::Other(String::new()))
    }
}

fn cmd_window(args: &Args, cfg: &CliConfig) -> Result<(), Failure> {
    let days = days(args)?;
    let book = load_book(Path::new(args.get("book").ok_or_else(|| {
        other("the start book comes from --book FILE (holdings, free stable per venue)")
    })?))?;
    let opts = WindowOpts {
        days,
        max_order_usd: flag_f(args, "max-order", "RUNGBOT_MAX_ORDER_USD")?
            .unwrap_or(cfg.backtest.max_order_usd),
        bag: flag_f(args, "bag", "RUNGBOT_BT_WINDOW_BAG")?,
    };
    let hist = window_history(args, cfg, days)?;
    let run = window::run(
        &cfg.core,
        hist.as_ref().map_err(|e| e.clone()),
        &book,
        &opts,
    );
    if let Some(b) = &run.book {
        // hand the exact start book to the sweep, like the reference did
        write(&cache_dir(args).join("bt-book.json"), &b.to_json())?;
    }
    print!("{}", run.text);
    Ok(())
}

fn cmd_sweep(args: &Args, cfg: &CliConfig) -> Result<(), Failure> {
    let days = days(args)?;
    let dir = cache_dir(args);
    let book_path = args
        .get("book")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("bt-book.json"));
    let book = if book_path.exists() {
        Some(load_book(&book_path)?)
    } else {
        None
    };
    let hist_path = args
        .get("history")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(format!("bt-hist-{days}d.json")));
    let hist = match std::fs::read_to_string(&hist_path) {
        Ok(t) => Ok(History::parse(&t).map_err(other)?),
        Err(_) => Err(format!(
            "FileNotFoundError: [Errno 2] No such file or directory: '{}'",
            hist_path.display()
        )),
    };
    let opts = SweepOpts {
        days,
        bag: flag_f(args, "bag", "RUNGBOT_BT_BAG")?,
        regime: cfg.regime,
    };
    let text = sweep::run(
        &cfg.core,
        hist.as_ref().map_err(|e| e.clone()),
        book.as_ref(),
        &opts,
    )
    .map_err(Failure::Other)?;
    print!("{text}");
    Ok(())
}

fn cmd_monthly(args: &Args, cfg: &CliConfig) -> Result<(), Failure> {
    let book = load_book(Path::new(args.get("book").ok_or_else(|| {
        other("the start book comes from --book FILE (holdings, free stable per venue)")
    })?))?;
    let windows: Vec<i64> = match args
        .get("windows")
        .map(str::to_string)
        .or_else(|| std::env::var("RUNGBOT_BT_WINDOWS").ok())
    {
        Some(w) => w
            .split(',')
            .filter(|x| !x.trim().is_empty())
            .map(|x| {
                x.trim()
                    .parse::<i64>()
                    .map_err(|e| other(format!("--windows: {e}")))
            })
            .collect::<Result<_, _>>()?,
        None => cfg.backtest.windows.clone(),
    };
    let percoin = !args.has("no-percoin")
        && std::env::var("RUNGBOT_BT_PERCOIN").as_deref() != Ok("0")
        && cfg.backtest.percoin;
    let opts = MonthlyOpts {
        windows,
        drift_band_pct: flag_f(args, "drift-band", "RUNGBOT_DRIFT_BAND_PCT")?
            .unwrap_or(cfg.backtest.drift_band_pct),
        percoin,
        percoin_min_gain: flag_f(args, "percoin-min-gain", "RUNGBOT_BT_PERCOIN_MIN_GAIN")?
            .unwrap_or(cfg.backtest.percoin_min_gain),
        max_order_usd: flag_f(args, "max-order", "RUNGBOT_MAX_ORDER_USD")?
            .unwrap_or(cfg.backtest.max_order_usd),
        sweep_bag: flag_f(args, "sweep-bag", "RUNGBOT_BT_BAG")?,
        regime: cfg.regime,
        now: now_epoch(args)?,
    };
    let fetch_err = std::cell::RefCell::new(None);
    let history_for = |d: i64| match window_history(args, cfg, d) {
        Ok(h) => h,
        Err(e) => {
            let msg = match &e {
                Failure::Config(m) | Failure::Prices(m) | Failure::Other(m) => m.clone(),
            };
            *fetch_err.borrow_mut() = Some(msg.clone());
            Err(msg)
        }
    };
    let report = monthly::run(&cfg.core, &book, &history_for, &opts);
    if let Some(exp) = &report.expectation {
        let path = args.path("expect", || {
            base_dir("XDG_STATE_HOME", ".local/state")
                .join("rungbot")
                .join("backtest-expectation.json")
        });
        write(&path, exp)?;
    }
    if args.has("dry-run") || !args.has("notify") {
        print!("{}", report.dry_run_text());
        return Ok(());
    }
    if !cfg.notify.is_configured() {
        return Err(Failure::Config(
            "--notify needs a `notify:` webhook or Telegram chat in the config".into(),
        ));
    }
    let empty = rungbot_core::Outcome {
        buys: Vec::new(),
        sells: Vec::new(),
        rows: Vec::new(),
        errors: Vec::new(),
        skips: Vec::new(),
        state: Default::default(),
    };
    let text = format!("{}\n\n{}", report.subject, report.body);
    let results = crate::notify::send(&cfg.notify, &empty, &text);
    if results.iter().any(|(_, r)| r.is_ok()) {
        println!(
            "Sent monthly backtest verdict across {}/{} windows.",
            report.windows_ok,
            opts.windows.len()
        );
        Ok(())
    } else {
        Err(Failure::Other(
            "Failed to send monthly backtest verdict.".into(),
        ))
    }
}

// ------------------------------------------------------------------ replays

struct Replay {
    cache: PathBuf,
    out: PathBuf,
    study: Py,
}

impl Replay {
    fn new(args: &Args) -> Result<Replay, Failure> {
        let cache = cache_dir(args);
        let out = args.path("out", || cache.join("replay"));
        let study = match args.get("study") {
            Some(f) => loads(&read(Path::new(f))?).map_err(|e| other(format!("{f}: {e}")))?,
            None => Py::dict(),
        };
        Ok(Replay {
            cache: cache.join("candles"),
            out,
            study,
        })
    }

    fn need(&self, key: &str) -> Result<&Py, Failure> {
        self.study
            .get(key)
            .ok_or_else(|| Failure::Config(format!("the study file (--study) needs `{key}`")))
    }

    fn file_for(&self, source: &str) -> PathBuf {
        self.cache
            .join(format!("{}.json", source.replace([':', '/'], "_")))
    }

    fn load(&self, source: &str) -> Result<Py, Failure> {
        let p = self.file_for(source);
        loads(&read(&p).map_err(|_| {
            other(format!(
                "no candle cache for {source} at {} -- run `rungbot backtest replay fetch`",
                p.display()
            ))
        })?)
        .map_err(|e| other(format!("{}: {e}", p.display())))
    }

    /// Bars for a source: `llama:` caches are price-only.
    fn bars(&self, source: &str) -> Result<Vec<Bar>, Failure> {
        let v = self.load(source)?;
        Ok(if source.starts_with("llama:") {
            data::px_bars(&v)
        } else {
            data::ohlc(&v)
        })
    }

    fn btc_source(&self) -> String {
        self.study
            .get("btc_source")
            .map(|s| s.to_py_string())
            .unwrap_or_else(|| "binance:BTCUSDT".into())
    }

    fn save(&self, name: &str, doc: &Py, indent: usize) -> Result<PathBuf, Failure> {
        let p = self.out.join(name);
        write(&p, &doc.json(Some(indent)))?;
        Ok(p)
    }

    fn doc(&self, name: &str, hint: &str) -> Result<Py, Failure> {
        let p = self.out.join(name);
        loads(
            &read(&p)
                .map_err(|_| other(format!("{} missing -- run `{hint}` first", p.display())))?,
        )
        .map_err(|e| other(format!("{}: {e}", p.display())))
    }
}

fn f64s(v: &Py) -> Vec<f64> {
    v.as_list().iter().filter_map(|x| x.as_f64()).collect()
}

fn three(v: Option<&Py>, what: &str) -> Result<[f64; 3], Failure> {
    let x = v.map(f64s).unwrap_or_default();
    x.try_into()
        .map_err(|_| Failure::Config(format!("`{what}` must list three numbers")))
}

fn str_of(v: &Py, k: &str) -> Result<String, Failure> {
    v.get(k)
        .map(|s| s.to_py_string())
        .ok_or_else(|| Failure::Config(format!("study entry is missing `{k}`")))
}

fn study_sources(r: &Replay) -> Vec<String> {
    let mut out = vec![r.btc_source()];
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    for sect in ["confirm", "current"] {
        for c in r
            .study
            .get(sect)
            .and_then(|s| s.get("coins"))
            .map(|c| c.as_list())
            .unwrap_or(&[])
        {
            if let Some(s) = c.get("source") {
                push(s.to_py_string());
            }
        }
    }
    for (_, srcs) in r
        .study
        .get("tops")
        .and_then(|t| t.get("coins"))
        .map(|c| c.items())
        .unwrap_or(&[])
    {
        for s in srcs.as_list() {
            push(s.to_py_string());
        }
    }
    if r.study.get("tops").is_some() {
        // the BTC top study reads the long price history and the sentiment index too
        push("coinmetrics:btc_priceusd".into());
        push("fng:history".into());
    }
    out
}

fn cmd_fetch(args: &Args, r: &Replay) -> Result<(), Failure> {
    let mut sources = study_sources(r);
    if let Some(extra) = args.get("sources") {
        for s in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !sources.iter().any(|x| x == s) {
                sources.push(s.to_string());
            }
        }
    }
    let now = now_epoch(args)?;
    for src in &sources {
        let (venue, sym) = src.split_once(':').unwrap_or((src.as_str(), ""));
        let path = r.file_for(src);
        let shown = |n: usize, rows: &[Py]| {
            let d = |r: &Py| {
                r.as_list()
                    .first()
                    .map(|x| x.to_py_string())
                    .unwrap_or_default()
            };
            format!(
                "{} {} n={n} {}..{}",
                rungbot_backtest::py::fs(venue, "8"),
                rungbot_backtest::py::fs(sym, "10"),
                rows.first().map(d).unwrap_or_default(),
                rows.last().map(d).unwrap_or_default()
            )
        };
        if path.exists() && !args.has("force") {
            let rows = r.load(src)?;
            let rows = rows.as_list();
            let line = shown(rows.len(), rows);
            println!("{}", line.replacen(" n=", " cached n=", 1));
            continue;
        }
        let got = match venue {
            "binance" => history::binance_daily_ohlc(sym, 1_501_545_600_000),
            "gate" => history::gate_daily_ohlc(sym, now),
            "kucoin" => history::kucoin_daily_ohlc(sym, now),
            "coinmetrics" => {
                let v = history::coinmetrics_btc().map_err(Failure::Prices)?;
                write(&path, &v.json(None))?;
                println!("{} {} n={}", venue, sym, v.as_list().len());
                continue;
            }
            "fng" => {
                let v = history::fng_history().map_err(Failure::Prices)?;
                write(&path, &v.json(None))?;
                println!("{} {} n={}", venue, sym, v.as_list().len());
                continue;
            }
            other_venue => Err(format!(
                "no fetcher for {other_venue}; put the cache at {}",
                path.display()
            )),
        };
        match got {
            Ok(rows) => {
                write(&path, &Py::List(rows.clone()).json(None))?;
                println!("{}", shown(rows.len(), &rows));
            }
            Err(e) => println!(
                "{} {} FAIL {e}",
                rungbot_backtest::py::fs(venue, "8"),
                rungbot_backtest::py::fs(sym, "10")
            ),
        }
    }
    Ok(())
}

fn alt_coins(r: &Replay) -> Result<Vec<AltCoin>, Failure> {
    r.need("confirm")?
        .get("coins")
        .map(|c| c.as_list())
        .unwrap_or(&[])
        .iter()
        .map(|c| {
            let rung1 = c
                .get("rung1")
                .map(f64s)
                .filter(|v| v.len() == 2)
                .map(|v| (v[0], v[1]));
            Ok(AltCoin {
                sym: str_of(c, "sym")?,
                source: str_of(c, "source")?,
                depths: three(c.get("depths"), "confirm.coins[].depths")?,
                weights: three(c.get("weights"), "confirm.coins[].weights")?,
                rung1,
            })
        })
        .collect()
}

fn tops_series(r: &Replay) -> Result<Vec<(String, Series)>, Failure> {
    let mut out = Vec::new();
    for (sym, srcs) in r
        .need("tops")?
        .get("coins")
        .map(|c| c.items())
        .unwrap_or(&[])
    {
        let mut sources = Vec::new();
        for s in srcs.as_list() {
            let s = s.to_py_string();
            if r.file_for(&s).exists() {
                let name = s.split(':').next().unwrap_or("").to_string();
                sources.push((name, r.bars(&s)?));
            }
        }
        out.push((sym.to_py_string(), Series::merge(&sources)));
    }
    Ok(out)
}

fn utc_minute(epoch: i64) -> String {
    let (y, m, d, h, mi, _) = time::civil(epoch as f64);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02} UTC")
}

fn cmd_replay(args: &Args) -> Result<(), Failure> {
    let study = args.pos.get(1).map(String::as_str).unwrap_or("");
    if study == "example" {
        print!("{STUDY_EXAMPLE}");
        return Ok(());
    }
    let r = Replay::new(args)?;
    let loader = |src: &str| r.bars(src).unwrap_or_default();
    match study {
        "fetch" => cmd_fetch(args, &r),
        "btc-confirm" => {
            let btc = Btc::new(&r.bars(&r.btc_source())?);
            let variants: Vec<String> = args
                .get("variants")
                .unwrap_or("nobreadth,btcbreadth")
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            let v: Vec<&str> = variants.iter().map(String::as_str).collect();
            let recent = match args.get("recent-from") {
                Some(d) => d.to_string(),
                None => btc.dates[btc.dates.len().saturating_sub(25)].clone(),
            };
            let (out, docs) = btc_confirm::report(&btc, &v, &default_shapes(), &recent);
            for (name, doc) in &docs {
                r.save(&format!("replay_{name}.json"), doc, 1)?;
            }
            print!("{out}");
            Ok(())
        }
        "btc-confirm-analysis" => {
            let btc = Btc::new(&r.bars(&r.btc_source())?);
            let doc = r.doc(
                "replay_nobreadth.json",
                "rungbot backtest replay btc-confirm",
            )?;
            let tranche = flag_f(args, "tranche", "RUNGBOT_BT_TRANCHE_USD")?
                .or_else(|| r.study.get("tranche_usd").and_then(|v| v.as_f64()))
                .unwrap_or(1000.0);
            let (out, analysis) = btc_confirm_analysis::run(&btc, &doc, tranche);
            r.save("analysis.json", &analysis, 1)?;
            print!("{out}");
            Ok(())
        }
        "alt-confirm" => {
            let coins = alt_coins(&r)?;
            let btc = r.bars(&r.btc_source())?;
            let (out, results) = alt_confirm::run(&btc, &coins, &loader);
            let p = r.save("results.json", &results, 1)?;
            print!("{out}");
            println!("\nwrote {}", p.display());
            Ok(())
        }
        "alt-sweep" => {
            let results = r.doc("results.json", "rungbot backtest replay alt-confirm")?;
            print!(
                "{}",
                alt_confirm::profile_sweep(&results, &alt_confirm::default_profiles())
            );
            Ok(())
        }
        "alt-current" => {
            let cur = r.need("current")?;
            let coins = cur
                .get("coins")
                .map(|c| c.as_list())
                .unwrap_or(&[])
                .iter()
                .map(|c| {
                    Ok(LiveRungs {
                        sym: str_of(c, "sym")?,
                        source: str_of(c, "source")?,
                        rungs: c.get("rungs").map(f64s).unwrap_or_default(),
                        spot: c.get("spot").and_then(|v| v.as_f64()).unwrap_or(f64::NAN),
                    })
                })
                .collect::<Result<Vec<_>, Failure>>()?;
            print!(
                "{}",
                alt_confirm::current(
                    &coins,
                    &loader,
                    &str_of(cur, "last_bear_close")?,
                    &str_of(cur, "confirmed")?
                )
            );
            Ok(())
        }
        "btc-top" => {
            let cutoff = args.get("cutoff").map(str::to_string);
            let bars = r.bars(&r.btc_source())?;
            // the last candle is today's partial unless a cutoff says otherwise
            let binance: Vec<(String, f64)> = match &cutoff {
                Some(c) => bars
                    .iter()
                    .filter(|b| b.date.as_str() < c.as_str())
                    .map(|b| (b.date.clone(), b.close))
                    .collect(),
                None => bars[..bars.len().saturating_sub(1)]
                    .iter()
                    .map(|b| (b.date.clone(), b.close))
                    .collect(),
            };
            let inputs = btc_top::Inputs {
                binance,
                coinmetrics: data::coinmetrics(&r.load("coinmetrics:btc_priceusd")?),
                fng: data::fng(&r.load("fng:history")?),
            };
            let (out, doc) = btc_top::run(&inputs);
            r.save("btc_top_results.json", &doc, 1)?;
            print!("{out}");
            Ok(())
        }
        "alt-top" => {
            let btc = Series::merge(&[("binance".into(), r.bars(&r.btc_source())?)]);
            let coins = tops_series(&r)?;
            let (out, doc) = alt_top::run(&Ladder::default(), &btc, &coins);
            r.save("alt_top_results.json", &doc, 1)?;
            print!("{out}");
            Ok(())
        }
        "sellpolicy" => {
            let alt_results = r.doc("alt_top_results.json", "rungbot backtest replay alt-top")?;
            let btc_results = r.doc("btc_top_results.json", "rungbot backtest replay btc-top")?;
            let coins = tops_series(&r)?;
            let alt = |sym: &str| {
                coins
                    .iter()
                    .find(|(s, _)| s == sym)
                    .map(|(_, s)| Closes {
                        d: s.d.clone(),
                        c: s.c.clone(),
                    })
                    .unwrap_or_default()
            };
            let mut merged = std::collections::BTreeMap::new();
            if let Ok(cm) = r.load("coinmetrics:btc_priceusd") {
                for (d, v) in data::coinmetrics(&cm) {
                    merged.insert(d, v);
                }
            }
            for b in r.bars(&r.btc_source())? {
                merged.insert(b.date, b.close);
            }
            let btc = Closes {
                d: merged.keys().map(|d| data::day_num(d)).collect(),
                c: merged.values().copied().collect(),
            };
            let (out, doc) = sellpolicy_replay::run(&alt_results, &btc_results, &alt, &btc);
            let p = r.save("sellpolicy_replay.json", &doc, 1)?;
            print!("{out}");
            println!("\nwritten {}", p.display());
            Ok(())
        }
        "microstate" => {
            let coins = r
                .need("microstate")?
                .get("coins")
                .map(|c| c.as_list())
                .unwrap_or(&[])
                .iter()
                .map(|c| {
                    let pairs = |k: &str| -> Vec<(Py, Py)> {
                        c.get(k)
                            .map(|l| {
                                l.as_list()
                                    .iter()
                                    .filter(|p| p.as_list().len() >= 2)
                                    .map(|p| (p.as_list()[0].clone(), p.as_list()[1].clone()))
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    Ok(Watch {
                        sym: str_of(c, "sym")?,
                        venue: str_of(c, "venue")?,
                        pair: str_of(c, "pair")?,
                        orders: Orders {
                            rungs: pairs("rungs"),
                            sells: pairs("sells"),
                            cost: c.get("cost").cloned().unwrap_or(Py::None),
                        },
                    })
                })
                .collect::<Result<Vec<_>, Failure>>()?;
            let (table, doc, notes) =
                microstate::run(&coins, &utc_minute(now_epoch(args)?), &history::LiveFeed)
                    .map_err(Failure::Prices)?;
            eprint!("{notes}");
            r.save("microstate.json", &doc, 2)?;
            print!("{table}");
            Ok(())
        }
        "" => Err(other(format!("which study?\n\n{USAGE}"))),
        s => Err(other(format!("unknown study {s:?}\n\n{USAGE}"))),
    }
}

pub fn run(args: &Args) -> Result<(), Failure> {
    let sub = args.pos.first().map(String::as_str).unwrap_or("");
    match sub {
        "" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        "invariants" => cmd_invariants(),
        "replay" => cmd_replay(args),
        "window" | "sweep" | "monthly" => {
            let cfg = crate::load_config(args)?;
            match sub {
                "window" => cmd_window(args, &cfg),
                "sweep" => cmd_sweep(args, &cfg),
                _ => cmd_monthly(args, &cfg),
            }
        }
        other_sub => Err(other(format!("unknown backtest {other_sub:?}\n\n{USAGE}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(argv: &[&str]) -> Args {
        let v: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        Args::parse(&v).expect("parses")
    }

    #[test]
    fn backtest_takes_positional_words_and_value_flags() {
        let a = args(&[
            "backtest", "replay", "alt-top", "--out", "/x", "--days", "90",
        ]);
        assert_eq!(a.pos, ["replay", "alt-top"]);
        assert_eq!(a.get("out"), Some("/x"));
        assert_eq!(a.get("days"), Some("90"));
        // other commands still refuse stray words
        let v: Vec<String> = ["plan", "oops"].iter().map(|s| s.to_string()).collect();
        assert!(Args::parse(&v).is_err());
    }

    #[test]
    fn the_example_study_carries_every_section_the_replays_read() {
        let r = Replay {
            cache: PathBuf::from("/nonexistent"),
            out: PathBuf::from("/nonexistent"),
            study: loads(STUDY_EXAMPLE).expect("the example study parses"),
        };
        let coins = alt_coins(&r).ok().expect("confirm coins");
        assert_eq!(coins.len(), 2);
        assert_eq!(coins[0].rung1, Some((60000.0, 65000.0)));
        for key in ["current", "tops", "microstate"] {
            assert!(r.need(key).is_ok(), "example lacks {key}");
        }
        let sources = study_sources(&r);
        assert_eq!(
            sources,
            [
                "binance:BTCUSDT",
                "binance:ETHUSDT",
                "coinmetrics:btc_priceusd",
                "fng:history"
            ]
        );
        assert_eq!(
            r.file_for("llama:osmosis"),
            PathBuf::from("/nonexistent/llama_osmosis.json")
        );
    }

    #[test]
    fn the_utc_minute_matches_the_reference_stamp() {
        assert_eq!(utc_minute(1_788_000_000), "2026-08-29 10:40 UTC");
    }
}
