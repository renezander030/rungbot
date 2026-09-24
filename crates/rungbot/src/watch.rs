//! `rungbot watch <regime|btc|zone|froth|divergence|daily>`: the watchers, with their I/O.
//!
//! The decisions live in [`rungbot_core::watch`]; this module reads what they need
//! (cached regime, public candles and tickers, the executor's order journal, a balances
//! snapshot, the state files), sends what they decide through `rungbot-notify`, and
//! writes their state back. Every state file is written to a temporary file and renamed
//! into place, so a crash mid-write leaves the previous file intact.
//!
//! Keyless: nothing here signs a request. Rung data comes from the executor's journal
//! file and balances from a snapshot file, both read-only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rungbot_core::watch::btc_level::{self, Levels};
use rungbot_core::watch::divergence::{self, DivergenceConfig};
use rungbot_core::watch::froth;
use rungbot_core::watch::json::Json;
use rungbot_core::watch::pyfmt;
use rungbot_core::watch::regime_state::{self, Prior};
use rungbot_core::watch::regime_watch;
use rungbot_core::watch::zone::{self, ReserveConfig, ZoneConfig};
use rungbot_core::watch::Hints;
use rungbot_core::{regime::KLINE_DAYS, Venue};
use rungbot_notify::Notifier;

use crate::config_file::CliConfig;
use crate::{klines, tickers};

/// The `watch:` block. Every knob also has the environment override the original
/// watchers read, listed in [`apply_env`].
#[derive(Debug, Clone, PartialEq)]
pub struct WatchConfig {
    /// Where the watchers keep their state. Default: beside the ladder state file.
    pub state_dir: Option<PathBuf>,
    /// The executor's order journal (read-only): resting deploy orders.
    pub journal: Option<PathBuf>,
    /// A balances snapshot (read-only), `{"ts": epoch, "venues": {venue: {ASSET:
    /// {"free": n, "locked": n}}}}`.
    pub balances: Option<PathBuf>,
    /// A snapshot older than this many seconds is not used.
    pub balances_max_age_s: f64,
    /// The executor's persisted book (read-only): holdings and the sell-policy mode.
    pub book: Option<PathBuf>,
    /// An external market-report verdict, quoted in regime mails when present.
    pub verdict: Option<PathBuf>,
    /// The monthly backtest's expectation, for the divergence check.
    pub expectation: Option<PathBuf>,
    pub hints: Hints,
    pub regime_ttl_h: f64,
    pub hist_ttl_h: f64,
    pub hist_days: usize,
    pub confirm_days: i64,
    pub btc: Levels,
    pub zone: ZoneConfig,
    /// Capital weights per coin, for the Gate top-up hold-back.
    pub alloc: Vec<(String, f64)>,
    pub divergence: DivergenceConfig,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            state_dir: None,
            journal: None,
            balances: None,
            balances_max_age_s: 6.0 * 3600.0,
            book: None,
            verdict: None,
            expectation: None,
            hints: Hints::default(),
            regime_ttl_h: regime_state::TTL_H,
            hist_ttl_h: regime_state::HIST_TTL_H,
            hist_days: regime_state::HIST_DAYS,
            confirm_days: regime_state::CONFIRM_DAYS,
            btc: Levels::default(),
            zone: ZoneConfig::default(),
            alloc: Vec::new(),
            divergence: DivergenceConfig::default(),
        }
    }
}

/// `~/` expands to `$HOME`.
pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(rest)
        }
        None => PathBuf::from(p),
    }
}

fn env_num(key: &str) -> Result<Option<f64>, String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse::<f64>()
            .map(Some)
            .map_err(|e| format!("{key}: {e}")),
        _ => Ok(None),
    }
}

/// The environment overrides the original watchers honoured, applied over the file.
pub fn apply_env(mut w: WatchConfig) -> Result<WatchConfig, String> {
    macro_rules! num {
        ($key:literal, $slot:expr, $t:ty) => {
            if let Some(v) = env_num($key)? {
                $slot = v as $t;
            }
        };
    }
    num!("REGIME_TTL_H", w.regime_ttl_h, f64);
    num!("REGIME_HIST_TTL_H", w.hist_ttl_h, f64);
    num!("REGIME_HIST_DAYS", w.hist_days, usize);
    num!("REGIME_CONFIRM_DAYS", w.confirm_days, i64);
    num!("BTC_ALERT_USD", w.btc.alert_usd, f64);
    num!("BTC_WARN_USD", w.btc.warn_usd, f64);
    num!("ZONE_STALE_DAYS", w.zone.stale_days, f64);
    num!("ZONE_STALE_EXTRA_PP", w.zone.stale_extra_pp, f64);
    num!("DEPLOY_MIN_USD", w.zone.idle_min_usd, f64);
    num!("ZONE_ALT_RUN_TRIPWIRE", w.zone.alt_run_tripwire, i64);
    num!("ZONE_BOOK_STALE_S", w.zone.book_stale_s, f64);
    num!("DRIFT_BAND_PCT", w.divergence.drift_band_pct, f64);
    num!("FLOOR_MARGIN_PCT", w.divergence.floor_margin_pct, f64);
    num!("MIN_DAYS", w.divergence.min_days, f64);
    let path = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
    if let Some(p) = path("ORDER_JOURNAL") {
        w.journal = Some(expand(&p));
    }
    if let Some(p) = path("LADDER_STATE") {
        w.book = Some(expand(&p));
    }
    Ok(w)
}

/// Where each state file lives.
#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    pub regime: PathBuf,
    pub history: PathBuf,
    pub zone: PathBuf,
    pub froth: PathBuf,
    pub btc: PathBuf,
    pub divergence: PathBuf,
}

impl Paths {
    pub fn resolve(w: &WatchConfig, ladder_state: &Path) -> Paths {
        let dir = w.state_dir.clone().unwrap_or_else(|| {
            ladder_state
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        });
        let env = |k: &str, name: &str| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(|v| expand(&v))
                .unwrap_or_else(|| dir.join(name))
        };
        let regime = env("REGIME_STATE", "regime-state.json");
        Paths {
            history: regime.with_file_name("regime-history.json"),
            regime,
            zone: env("ZONE_STATE", "zone-state.json"),
            froth: env("FROTH_STATE", "froth-state.json"),
            btc: env("BTC_ALERT_STATE", "btc-alert-state.json"),
            divergence: env("DIVERGENCE_STATE", "divergence-state.json"),
        }
    }
}

// ---- files ---------------------------------------------------------------------------

/// What a JSON file held.
#[derive(Debug, Clone, PartialEq)]
pub enum FileRead {
    Missing,
    Unreadable(String),
    Parsed(Json),
}

impl FileRead {
    pub fn object(&self) -> Option<&Json> {
        match self {
            FileRead::Parsed(j) if j.is_obj() => Some(j),
            _ => None,
        }
    }
}

pub fn read_json(path: &Path) -> FileRead {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileRead::Missing,
        Err(e) => FileRead::Unreadable(e.to_string()),
        Ok(text) => match serde_json::from_str::<Json>(&text) {
            Ok(j) => FileRead::Parsed(j),
            Err(e) => FileRead::Unreadable(e.to_string()),
        },
    }
}

/// Write via a temporary file and a rename: a reader never sees half a file.
pub fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".into());
    let tmp = path.with_file_name(format!("{name}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

fn save(path: &Path, j: &Json, indent: Option<usize>, what: &str) -> Result<(), String> {
    write_atomic(path, &j.dumps(indent))
        .map_err(|e| format!("could not write {what} {}: {e}", path.display()))
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn to_json(v: serde_json::Value) -> Json {
    serde_json::from_value(v).unwrap_or_default()
}

// ---- market data ---------------------------------------------------------------------

/// Spot prices by `(venue, pair)`, from public tickers. Revolut X answers every pair in
/// one request, which is cached for the run.
/// Revolut X's whole ticker feed, `{pair: (price, 24h change %)}`.
type RevxFeed = Result<BTreeMap<String, (f64, f64)>, String>;

#[derive(Default)]
pub struct Spot {
    revx: Option<RevxFeed>,
}

impl Spot {
    pub fn price(&mut self, venue: &str, pair: &str) -> Result<f64, String> {
        match venue {
            "binance" => {
                let body = tickers::get_json(&format!(
                    "https://api.binance.com/api/v3/ticker/price?symbol={pair}"
                ))
                .map_err(|e| e.to_string())?;
                body.get("price")
                    .and_then(|p| match p {
                        serde_json::Value::String(s) => s.parse().ok(),
                        serde_json::Value::Number(n) => n.as_f64(),
                        _ => None,
                    })
                    .ok_or_else(|| format!("price {pair}: no price in the response"))
            }
            "gate" => tickers::gate(pair).map(|p| p.0).map_err(|e| e.to_string()),
            "revx" => {
                let all = self
                    .revx
                    .get_or_insert_with(|| tickers::revx_all().map_err(|e| e.to_string()));
                match all {
                    Ok(m) => m
                        .get(pair)
                        .map(|p| p.0)
                        .ok_or_else(|| format!("price {pair}: not in ticker feed")),
                    Err(e) => Err(e.clone()),
                }
            }
            other => Err(format!("no public spot price for venue {other:?}")),
        }
    }
}

fn coin_closes(cfg: &CliConfig, days: usize) -> Vec<(String, Result<Vec<f64>, String>)> {
    cfg.core
        .coins
        .iter()
        .map(|coin| {
            let r = klines::kline_source(coin, cfg.klines.get(&coin.symbol).map(String::as_str))
                .and_then(|(venue, pair)| {
                    klines::closes(venue, &pair, days).map_err(|e| e.to_string())
                });
            (coin.symbol.clone(), r)
        })
        .collect()
}

fn btc_closes(days: usize) -> Result<Vec<f64>, String> {
    klines::closes(Venue::Binance, "BTCUSDT", days).map_err(|e| e.to_string())
}

/// The cached regime reading, recomputed when older than the TTL (or forced).
pub fn get_regime(cfg: &CliConfig, paths: &Paths, force: bool) -> Json {
    let now = now();
    if !force {
        if let Some(c) = read_json(&paths.regime).object() {
            if regime_state::is_fresh(c, now, cfg.watch.regime_ttl_h) {
                return c.clone();
            }
        }
    }
    let reg = regime_state::compute_state(
        &coin_closes(cfg, KLINE_DAYS),
        &btc_closes(KLINE_DAYS),
        cfg.regime,
        now as i64,
    );
    if let Err(e) = write_atomic(&paths.regime, &reg.dumps(Some(2))) {
        eprintln!("WARN: could not cache regime state: {e}");
    }
    reg
}

/// The label history (cached for `hist_ttl_h`), saved when replayed.
pub fn label_history(cfg: &CliConfig, paths: &Paths, force: bool) -> Json {
    let prior = match read_json(&paths.history) {
        FileRead::Missing => Prior::Missing,
        FileRead::Unreadable(_) => Prior::Unreadable,
        FileRead::Parsed(j) => Prior::Parsed(j),
    };
    let days = cfg.watch.hist_days;
    let h = regime_state::label_history(
        &prior,
        now(),
        cfg.watch.hist_ttl_h,
        cfg.watch.confirm_days,
        force,
        || {
            let series = coin_closes(cfg, days)
                .into_iter()
                .filter_map(|(_, r)| r.ok())
                .collect();
            (series, btc_closes(days))
        },
    );
    if h.save {
        if let Err(e) = write_atomic(&paths.history, &h.json.dumps(None)) {
            eprintln!("WARN: could not write regime history: {e}");
        }
    }
    h.json
}

// ---- executor files -------------------------------------------------------------------

const OPEN: [&str; 6] = [
    "pending",
    "placed",
    "open",
    "new",
    "NEW",
    "PARTIALLY_FILLED",
];

/// The order journal: `{client_id: order}`. A missing file is an empty journal; one that
/// exists and cannot be read is an error, never an empty book.
pub fn load_journal(path: Option<&Path>) -> Result<Json, String> {
    let path = path.ok_or("no order journal configured (watch.journal)")?;
    match read_json(path) {
        FileRead::Missing => Ok(Json::obj()),
        FileRead::Parsed(j) if j.is_obj() => Ok(j),
        FileRead::Parsed(_) => Err(format!("{} holds no object", path.display())),
        FileRead::Unreadable(e) => Err(format!(
            "{} exists but cannot be read ({e}); refusing to read it as empty",
            path.display()
        )),
    }
}

/// Open `deploy_buy` orders, in journal order.
pub fn open_deploy(journal: &Json) -> Vec<&Json> {
    journal
        .entries()
        .iter()
        .map(|(_, o)| o)
        .filter(|o| {
            o.get("status")
                .and_then(Json::as_str)
                .is_some_and(|s| OPEN.contains(&s))
                && o.get("kind").and_then(Json::as_str) == Some("deploy_buy")
        })
        .collect()
}

const VENUES: [&str; 3] = ["binance", "gate", "revx"];

/// Balances per venue from the snapshot file: the three venues first (an error for one
/// the snapshot does not hold), then any other venue it lists.
pub fn load_balances(w: &WatchConfig, now: f64) -> Vec<(String, Result<Json, String>)> {
    let all_err = |msg: String| {
        VENUES
            .iter()
            .map(|v| (v.to_string(), Err(msg.clone())))
            .collect::<Vec<_>>()
    };
    let Some(path) = &w.balances else {
        return all_err("no balances snapshot configured (watch.balances)".into());
    };
    let snap = match read_json(path) {
        FileRead::Missing => return all_err(format!("no balances snapshot at {}", path.display())),
        FileRead::Unreadable(e) => return all_err(format!("unreadable balances snapshot: {e}")),
        FileRead::Parsed(j) => j,
    };
    if let Some(ts) = snap.get("ts").and_then(Json::num) {
        let age = now - ts;
        if age > w.balances_max_age_s {
            return all_err(format!(
                "balances snapshot is {}s old",
                pyfmt::fixed(age, 0)
            ));
        }
    }
    let venues = snap.get("venues").cloned().unwrap_or(snap);
    let one = |v: &Json| match v.get("error") {
        Some(e) => Err(e.py_str()),
        None if v.is_obj() => Ok(v.clone()),
        None => Err("not an object".to_string()),
    };
    let mut out: Vec<(String, Result<Json, String>)> = VENUES
        .iter()
        .map(|v| {
            let r = match venues.get(v) {
                Some(b) => one(b),
                None => Err(format!("{v} is not in the balances snapshot")),
            };
            (v.to_string(), r)
        })
        .collect();
    for (k, v) in venues.entries() {
        if k != "ts" && !VENUES.contains(&k.as_str()) {
            out.push((k.clone(), one(v)));
        }
    }
    out
}

// ---- runners ---------------------------------------------------------------------------

/// Flags every watcher understands.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    pub dry_run: bool,
    pub force: bool,
    pub preview: bool,
}

fn warn_if_silent(n: &Notifier) {
    if n.email.is_none() && n.telegram.is_none() {
        eprintln!("WARN: no email or telegram notify channel configured; nothing can be sent");
    }
}

/// `rungbot watch regime`: announce an unannounced label change, once per stage.
pub fn regime(cfg: &CliConfig, paths: &Paths, f: Flags) -> Result<i32, String> {
    let hist = label_history(cfg, paths, f.force);
    let plan = regime_watch::plan(regime_state::pending_shift(&hist), &hist, f.force);
    let shift = match plan {
        regime_watch::Plan::Quiet => {
            println!("{}", regime_watch::QUIET_LINE);
            return Ok(0);
        }
        regime_watch::Plan::Announce(s) => s,
    };
    let reg = get_regime(cfg, paths, false);
    let zone = load_journal(cfg.watch.journal.as_deref()).and_then(|j| {
        let mut spot = Spot::default();
        regime_watch::zone_picture(&open_deploy(&j), |v, p| spot.price(v, p))
    });
    let verdict = cfg
        .watch
        .verdict
        .as_deref()
        .map(read_json)
        .and_then(|r| r.object().cloned());
    let (subject, text) = regime_watch::build_message(
        &shift,
        &reg,
        zone,
        verdict.as_ref(),
        now(),
        &cfg.watch.hints,
    );
    if f.dry_run {
        println!("--- would send ---\nSubject: {subject}\n\n{text}");
        return Ok(0);
    }
    warn_if_silent(&cfg.notifier);
    let d = cfg.notifier.alert(&subject, &text);
    if d.any() {
        // Re-read (the cache) and mark, as the announcement just went out.
        let mut h = label_history(cfg, paths, false);
        regime_state::mark_notified(&mut h, &shift.label, &shift.stage);
        save(&paths.history, &h, None, "regime history")?;
    }
    println!("{}", d.log_line(&subject));
    Ok(0)
}

/// `rungbot watch btc`: the BTC line, email only.
pub fn btc(cfg: &CliConfig, paths: &Paths, f: Flags) -> Result<i32, String> {
    let reg = get_regime(cfg, paths, false);
    let stored = read_json(&paths.btc);
    let c = btc_level::check(
        &reg,
        stored.object(),
        cfg.watch.btc,
        f.dry_run,
        now(),
        &cfg.watch.hints,
    );
    if let Some(st) = &c.state {
        if let Err(e) = save(&paths.btc, st, None, "BTC line state") {
            eprintln!("WARN: {e}");
        }
    }
    let Some((subject, text)) = c.fire else {
        return Ok(0);
    };
    if f.dry_run {
        println!("\n--- Would send BTC-LEVEL email ---\nSubject: {subject}\n{text}");
        return Ok(0);
    }
    if cfg.notifier.send_email(&subject, &text, None) {
        println!("sent: btc-level | {subject}");
        Ok(0)
    } else {
        eprintln!("failed to send BTC-level email");
        Ok(1)
    }
}

fn reserve_of(cfg: &CliConfig) -> ReserveConfig {
    ReserveConfig {
        alloc: cfg.watch.alloc.clone(),
        routing: cfg
            .core
            .coins
            .iter()
            .map(|c| (c.symbol.clone(), c.venue.as_str().to_string()))
            .collect(),
    }
}

/// `rungbot watch zone`: RUN flips, stale rungs, idle cash.
pub fn zone(cfg: &CliConfig, paths: &Paths, f: Flags) -> Result<i32, String> {
    let w = &cfg.watch;
    let now = now();
    if f.preview {
        print!("{}", zone::preview(now, &w.zone, &w.hints));
        return Ok(0);
    }
    let state = read_json(&paths.zone)
        .object()
        .cloned()
        .unwrap_or_else(zone::empty_state);
    let reg = get_regime(cfg, paths, false);
    let journal = load_journal(w.journal.as_deref())?;
    let rows = open_deploy(&journal);
    let balances = load_balances(w, now);
    let book = w.book.as_deref().map(read_json);
    let reserve = reserve_of(cfg);
    let inp = zone::Input {
        now,
        state: &state,
        reg: &reg,
        deploy_rows: &rows,
        balances: &balances,
        book: book.as_ref().and_then(FileRead::object),
        reserve: &reserve,
        cfg: &w.zone,
    };
    let mut spot = Spot::default();
    let col = zone::collect(&inp, |v, p| spot.price(v, p));
    match zone::plan(&state, &col, f.force, now, &w.zone, &w.hints) {
        zone::Plan::Quiet { line, state: st } => {
            println!("{line}");
            if !f.dry_run {
                save(&paths.zone, &st, Some(1), "zone state")?;
            }
        }
        zone::Plan::Send { subject, text } => {
            if f.dry_run {
                println!("--- would send ---\nSubject: {subject}\n\n{text}");
                return Ok(0);
            }
            warn_if_silent(&cfg.notifier);
            let d = cfg.notifier.alert(&subject, &text);
            if d.any() {
                save(
                    &paths.zone,
                    &zone::after_delivery(&state, &col, now),
                    Some(1),
                    "zone state",
                )?;
            }
            println!("{}", d.log_line(&subject));
        }
    }
    Ok(0)
}

fn get_body(url: &str) -> Result<Json, String> {
    tickers::get_json(url)
        .map(to_json)
        .map_err(|e| e.to_string())
}

/// BTC's last `days` daily closes, paged 1,000 candles at a time from `days + 5` back.
fn btc_history(days: usize, now: f64) -> Result<Vec<f64>, String> {
    let mut start = (now * 1000.0) as i64 - (days as i64 + 5) * 86_400_000;
    let mut out: Vec<f64> = Vec::new();
    for _ in 0..10 {
        let rows = tickers::get_json(&format!(
            "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=1d&limit=1000&startTime={start}"
        ))
        .map_err(|e| e.to_string())?;
        let rows = rows.as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            break;
        }
        for k in &rows {
            let close = k.get(4).and_then(|c| match c {
                serde_json::Value::String(s) => s.parse().ok(),
                serde_json::Value::Number(n) => n.as_f64(),
                _ => None,
            });
            out.push(close.ok_or("a candle without a close")?);
        }
        if rows.len() < 1000 {
            break;
        }
        start = rows
            .last()
            .and_then(|r| r.get(0))
            .and_then(serde_json::Value::as_i64)
            .ok_or("a candle without an open time")?
            + 86_400_000;
    }
    Ok(out[out.len().saturating_sub(days)..].to_vec())
}

/// `rungbot watch froth`: crowding signals and the BTC blow-off arming.
pub fn froth(cfg: &CliConfig, paths: &Paths, f: Flags) -> Result<i32, String> {
    let now = now();
    let sig = froth::read_signals(
        &get_body("https://api.alternative.me/fng/?limit=7&format=json"),
        &get_body("https://fapi.binance.com/fapi/v1/fundingRate?symbol=BTCUSDT&limit=30"),
        &get_body(
            "https://fapi.binance.com/futures/data/openInterestHist?symbol=BTCUSDT&period=1d&limit=30",
        ),
        &btc_history(220, now),
    );
    let arm = klines::closes_with_times(Venue::Binance, "BTCUSDT", 800)
        .map_err(|e| e.to_string())
        .and_then(|(closes, stamps)| {
            let rows: Vec<(f64, f64)> = stamps.iter().map(|t| t * 1000.0).zip(closes).collect();
            froth::btc_arm(&rows, now)
        });
    let st = read_json(&paths.froth)
        .object()
        .cloned()
        .unwrap_or_else(Json::obj);
    match froth::plan(&sig, &st, arm, f.force, now) {
        froth::Plan::Unchanged { line, state } => {
            println!("{line}");
            if let (Some(s), false) = (state, f.dry_run) {
                save(&paths.froth, &s, Some(2), "froth state")?;
            }
        }
        froth::Plan::Send {
            subject,
            text,
            state,
        } => {
            if f.dry_run {
                println!("--- would send ---\nSubject: {subject}\n\n{text}");
                return Ok(0);
            }
            warn_if_silent(&cfg.notifier);
            let d = cfg.notifier.alert(&subject, &text);
            if d.any() {
                save(&paths.froth, &state, Some(2), "froth state")?;
            }
            println!("{}", d.log_line(&subject));
        }
    }
    Ok(0)
}

/// The live book from the balances snapshot: each coin summed over every venue, free +
/// locked, and every stable (USD, USDC, USDT) likewise.
fn live_book(cfg: &CliConfig, now: f64) -> Result<divergence::Book, String> {
    let bal = load_balances(&cfg.watch, now);
    let mut venues = Vec::new();
    for (v, b) in &bal {
        match b {
            Ok(j) => venues.push(j),
            // A venue the snapshot does not hold is simply not part of the book.
            Err(e) if e.ends_with("is not in the balances snapshot") => {}
            Err(e) => return Err(format!("{v}: {e}")),
        }
    }
    let tot = |j: &Json, asset: &str| -> f64 {
        let v = j.get(asset);
        let x = |k: &str| {
            v.and_then(|v| v.get(k))
                .and_then(Json::to_float)
                .unwrap_or(0.0)
        };
        x("free") + x("locked")
    };
    let held = Json::Obj(
        cfg.core
            .coins
            .iter()
            .map(|c| {
                let s = pyfmt::sum(venues.iter().map(|j| tot(j, &c.symbol)));
                (c.symbol.clone(), Json::Float(s))
            })
            .collect(),
    );
    let stable = pyfmt::sum(
        venues
            .iter()
            .flat_map(|j| ["USD", "USDC", "USDT"].map(|s| tot(j, s))),
    );
    Ok(divergence::Book { held, stable })
}

fn prices_now(cfg: &CliConfig) -> Result<Json, String> {
    let p = tickers::fetch(&cfg.core.coins, 3).map_err(|e| e.to_string())?;
    Ok(Json::Obj(
        cfg.core
            .coins
            .iter()
            .filter_map(|c| {
                p.get(&c.symbol)
                    .map(|x| (c.symbol.clone(), Json::Float(x.price)))
            })
            .collect(),
    ))
}

/// `rungbot watch divergence`: the live book against the backtest's promise.
pub fn divergence(cfg: &CliConfig, paths: &Paths, f: Flags) -> Result<i32, String> {
    let w = &cfg.watch;
    let expect = match w.expectation.as_deref().map(read_json) {
        None | Some(FileRead::Missing) => {
            println!("{}", divergence::NO_EXPECTATION);
            return Ok(0);
        }
        Some(FileRead::Unreadable(e)) => return Err(format!("backtest expectation: {e}")),
        Some(FileRead::Parsed(j)) => j,
    };
    let now = now();
    let book = live_book(cfg, now);
    let prices = if book.is_ok() {
        prices_now(cfg)
    } else {
        Ok(Json::obj())
    };
    let state = match read_json(&paths.divergence) {
        FileRead::Missing => None,
        FileRead::Parsed(j) if j.is_obj() => Some(j),
        FileRead::Parsed(_) | FileRead::Unreadable(_) => {
            return Err(format!("{} cannot be read", paths.divergence.display()))
        }
    };
    let regime = get_regime(cfg, paths, false)
        .get("market")
        .map(Json::py_str)
        .unwrap_or_else(|| "unknown".into());
    let dry = f.dry_run || std::env::var("DRY_RUN").as_deref() == Ok("1");
    let plan = divergence::plan(divergence::Input {
        now,
        expect: &expect,
        state: state.as_ref(),
        book,
        prices,
        regime,
        dry_run: dry,
        cfg: w.divergence,
        hints: &w.hints,
    });
    match plan {
        divergence::Plan::Done {
            code,
            stdout,
            stderr,
            state,
        } => {
            stdout.iter().for_each(|l| println!("{l}"));
            stderr.iter().for_each(|l| eprintln!("{l}"));
            if let Some(s) = state {
                save(&paths.divergence, &s, Some(2), "divergence state")?;
            }
            Ok(code)
        }
        divergence::Plan::Send {
            stdout,
            subject,
            body,
            state,
        } => {
            stdout.iter().for_each(|l| println!("{l}"));
            if cfg.notifier.send_email(&subject, &body, None) {
                println!("{}", divergence::SENT_LINE);
                save(&paths.divergence, &state, Some(2), "divergence state")?;
                Ok(0)
            } else {
                eprintln!("{}", divergence::FAILED_LINE);
                Ok(1)
            }
        }
    }
}

/// A watcher that did not run to the end: say so on Telegram, since its silence would
/// otherwise read as "nothing to report".
pub fn failure_notice(cfg: &CliConfig, what: &str, code: i32, detail: &str) {
    let text = format!("{what} FAILED (exit {code}) — {detail}");
    if let Some(tg) = &cfg.notifier.telegram {
        if let Err(e) = tg.send(&text) {
            eprintln!("WARN: could not report the failure on telegram: {e}");
        }
    }
}

/// Run a watcher, reporting a failure (an error or a nonzero exit) on Telegram.
pub fn run_reported(
    cfg: &CliConfig,
    what: &str,
    detail: &str,
    run: impl FnOnce() -> Result<i32, String>,
) -> i32 {
    let code = match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{what}: {e}");
            1
        }
    };
    if code != 0 {
        failure_notice(cfg, what, code, detail);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rungbot-watch-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_state_file_is_replaced_whole() {
        let d = tmp("atomic");
        let p = d.join("s.json");
        write_atomic(&p, "{\"a\": 1}").unwrap();
        write_atomic(&p, "{\"a\": 2}").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"a\": 2}");
        assert!(
            !d.join("s.json.tmp").exists(),
            "no temporary file is left behind"
        );
        assert_eq!(read_json(&d.join("missing.json")), FileRead::Missing);
        std::fs::write(d.join("bad.json"), "{nope").unwrap();
        assert!(matches!(
            read_json(&d.join("bad.json")),
            FileRead::Unreadable(_)
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_journal_that_cannot_be_read_is_an_error_not_an_empty_book() {
        let d = tmp("journal");
        let p = d.join("orders.json");
        assert_eq!(load_journal(Some(&p)).unwrap(), Json::obj());
        std::fs::write(&p, "[1, 2]").unwrap();
        assert!(load_journal(Some(&p)).is_err());
        std::fs::write(
            &p,
            r#"{"b": {"status": "open", "kind": "deploy_buy", "sym": "B"},
                "a": {"status": "filled", "kind": "deploy_buy"},
                "c": {"status": "NEW", "kind": "deploy_buy", "sym": "C"}}"#,
        )
        .unwrap();
        let j = load_journal(Some(&p)).unwrap();
        let syms: Vec<String> = open_deploy(&j)
            .iter()
            .map(|o| o.get("sym").unwrap().py_str())
            .collect();
        assert_eq!(syms, ["B", "C"], "journal order, open deploy orders only");
        assert!(load_journal(None).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A config whose state all lives in `d`, with every watcher's inputs on disk.
    fn setup(d: &Path, extra: &str) -> (CliConfig, Paths) {
        let yaml = format!(
            "coins:\n  AAA:\n    venue: gate\n    pair: AAA_USDT\n  BTC:\n    venue: binance\n    pair: BTCUSDT\n\
             watch:\n  state_dir: {dir}\n  journal: {dir}/journal.json\n  balances: {dir}/balances.json\n\
             \x20 expectation: {dir}/expect.json\n  btc:\n    alert_usd: 30000\n    warn_usd: 33000\n\
             \x20 zone:\n    alts: AAA\n{extra}",
            dir = d.display()
        );
        let cfg = crate::config_file::from_str(&yaml).expect("the test config parses");
        let paths = Paths::resolve(&cfg.watch, &d.join("state.json"));
        (cfg, paths)
    }

    fn fresh_regime(paths: &Paths, px: f64, now: f64) {
        let reg = format!(
            r#"{{"epoch": {now}, "market": "chop", "btc": {{"px": {px}, "sma100": 1.0, "sma200": 2.0}},
                "breadth_above_sma30": "1/2", "coins": {{"AAA": {{"running": true,
                "signals": {{"above_sma30": true, "ret30_strong": true, "fresh_30d_high": true,
                "higher_lows": false}}, "px": 1.0, "sma30": 0.9}}, "BTC": {{"running": false,
                "error": "feed down"}}}}}}"#
        );
        std::fs::write(&paths.regime, reg).unwrap();
    }

    const OFF: Flags = Flags {
        dry_run: false,
        force: false,
        preview: false,
    };

    #[test]
    fn the_watchers_run_offline_end_to_end() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_OFFLINE", Some("1")),
            ("REGIME_STATE", None),
            ("ZONE_STATE", None),
            ("FROTH_STATE", None),
            ("BTC_ALERT_STATE", None),
            ("DIVERGENCE_STATE", None),
            ("ORDER_JOURNAL", None),
            ("LADDER_STATE", None),
            ("BTC_ALERT_USD", None),
            ("BTC_WARN_USD", None),
            ("DRY_RUN", None),
        ]);
        let d = tmp("e2e");
        let (cfg, paths) = setup(&d, "");
        let now = now();
        fresh_regime(&paths, 32_000.0, now);

        // BTC line: under the warn line, no channel configured: state still records the
        // band, so the next run does not fire again.
        assert_eq!(
            btc(&cfg, &paths, OFF).unwrap(),
            1,
            "the mail could not go out"
        );
        let st = read_json(&paths.btc);
        assert_eq!(
            st.object()
                .and_then(|s| s.get("band"))
                .map(Json::py_str)
                .as_deref(),
            Some("warn")
        );
        assert_eq!(
            btc(&cfg, &paths, OFF).unwrap(),
            0,
            "the same band does not re-fire"
        );

        // Zone: first run records the RUN baseline quietly.
        std::fs::write(d.join("journal.json"), "{}").unwrap();
        assert_eq!(zone(&cfg, &paths, OFF).unwrap(), 0);
        let z = read_json(&paths.zone);
        let run = z.object().and_then(|s| s.get("run")).cloned().unwrap();
        assert_eq!(run.get("AAA"), Some(&Json::Bool(true)));
        // A journal that cannot be read stops the watcher instead of reading as empty.
        std::fs::write(d.join("journal.json"), "{broken").unwrap();
        assert!(zone(&cfg, &paths, OFF).is_err());

        // Regime: a fresh history with an announced label is quiet.
        let hist = format!(
            r#"{{"epoch": {now}, "labels": ["chop"], "label": "chop", "held_days": 1,
                "prev_label": null, "confirmed": false, "confirm_days": 14,
                "days_covered": 1, "notified": {{"chop": "provisional"}}}}"#
        );
        std::fs::write(&paths.history, &hist).unwrap();
        assert_eq!(regime(&cfg, &paths, OFF).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&paths.history).unwrap(), hist);

        // Froth: every source refused offline reads calm; nothing delivered, nothing kept.
        assert_eq!(froth(&cfg, &paths, OFF).unwrap(), 0);
        assert_eq!(read_json(&paths.froth), FileRead::Missing);

        // Divergence: no expectation is a skip; with one, offline balances fail loudly.
        assert_eq!(divergence(&cfg, &paths, OFF).unwrap(), 0);
        std::fs::write(d.join("expect.json"), r#"{"epoch": 1, "alpha_pct": 5.0}"#).unwrap();
        assert_eq!(divergence(&cfg, &paths, OFF).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn env_overrides_win_over_the_file() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("BTC_ALERT_USD", Some("123")),
            ("ZONE_STALE_DAYS", Some("3")),
            ("ORDER_JOURNAL", Some("/tmp/j.json")),
        ]);
        let d = tmp("env");
        let (cfg, _) = setup(&d, "");
        assert_eq!(cfg.watch.btc.alert_usd, 123.0);
        assert_eq!(cfg.watch.btc.warn_usd, 33_000.0);
        assert_eq!(cfg.watch.zone.stale_days, 3.0);
        assert_eq!(cfg.watch.journal.as_deref(), Some(Path::new("/tmp/j.json")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn balances_come_from_a_fresh_snapshot_only() {
        let d = tmp("bal");
        let p = d.join("balances.json");
        std::fs::write(
            &p,
            r#"{"ts": 1000, "venues": {"gate": {"USDT": {"free": 5, "locked": 1}},
                "kraken": {"USD": {"free": 1}}, "revx": {"error": "401"}}}"#,
        )
        .unwrap();
        let w = WatchConfig {
            balances: Some(p.clone()),
            ..Default::default()
        };
        let b = load_balances(&w, 1500.0);
        let names: Vec<&str> = b.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(names, ["binance", "gate", "revx", "kraken"]);
        assert!(b[0].1.is_err() && b[1].1.is_ok());
        assert_eq!(b[2].1, Err("401".into()));
        let old = load_balances(&w, 1000.0 + 7.0 * 3600.0);
        assert!(old
            .iter()
            .all(|(_, r)| r.as_ref().unwrap_err().contains("old")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
