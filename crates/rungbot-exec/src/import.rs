//! Importing an order journal written by another implementation of this journal format.
//!
//! The source directory holds `orders-journal.json` (`{client_id: row}`) and, optionally,
//! `orders-archive.jsonl` and the live run state: `ladder-state.live.json`,
//! `pnl-ledger.json`, `ttl-warned.json`, the decision log `decisions.jsonl`, the
//! signal-mail dedupe `signal-notices.json`, the level alert's `btc-alert-state.json`, the
//! deploy layer's `deploy-state.live.json` (written as `deploy-state.json`) and the last
//! book audit `audit-state.json`. Every other state file the runtime reads is copied as
//! it is, after a check that it parses: the dry and off ladder states, the regime cache
//! and history, the froth, zone and divergence state, the market verdict, the monthly
//! backtest's expectation, the research ledger and indexes, the dashboard's manual fills,
//! targets and caches, the fill-odds candle cache, and the daily audit and archive
//! markers (their modification time is what counts). [`mapping_lines`] prints where each
//! file goes and which known files are left out, and why. The run state is written where
//! the run config reads it ([`Targets::from_config`]), else beside the imported journal
//! under the names [`crate::store`] uses. Every row is read into an [`Order`]; fields this crate does not
//! model are carried along unchanged. The import then checks itself: each row is written
//! back out and compared with the source, where `null` and an absent field count as the
//! same and `5` and `5.0` are the same number. Anything else that differs is reported.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::Value;

use crate::housekeeping::{LadderState, PnlLedger, TtlWarned};
use crate::journal::{Journal, Order};
use crate::store;

pub const JOURNAL_FILE: &str = "orders-journal.json";
pub const ARCHIVE_FILE: &str = "orders-archive.jsonl";
pub const LADDER_SOURCE: &str = "ladder-state.live.json";
pub const PNL_SOURCE: &str = "pnl-ledger.json";
pub const TTL_SOURCE: &str = "ttl-warned.json";
pub const DECISIONS_FILE: &str = "decisions.jsonl";
pub const NOTICES_FILE: &str = "signal-notices.json";
pub const BTC_ALERT_FILE: &str = "btc-alert-state.json";
/// The deploy layer's state in the source directory, and its name beside the journal.
pub const DEPLOY_SOURCE: &str = "deploy-state.live.json";
pub const DEPLOY_FILE: &str = "deploy-state.json";
/// The last book audit (the dashboard banner reads it).
pub const AUDIT_FILE: &str = "audit-state.json";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportReport {
    pub rows: usize,
    pub open: usize,
    pub by_status: BTreeMap<String, usize>,
    pub by_kind: BTreeMap<String, usize>,
    pub by_venue: BTreeMap<String, usize>,
    /// Fields kept verbatim because no [`Order`] field models them, with their counts.
    pub unmodelled: BTreeMap<String, usize>,
    pub archive_rows: usize,
    /// Coins in the ladder state, and whether it carries a balance snapshot; `None`
    /// without a ladder state file.
    pub ladder_coins: Option<(usize, bool)>,
    pub pnl_records: Option<usize>,
    pub ttl_flags: Option<usize>,
    pub decision_lines: Option<usize>,
    pub notices: Option<usize>,
    pub btc_band: Option<String>,
    /// Venues with a deploy baseline, and whether a top-up is marked in flight.
    pub deploy_venues: Option<(usize, bool)>,
    /// Whether the last audit was clean, and its finding count.
    pub audit: Option<(bool, usize)>,
    /// Rows whose re-serialised form differs from the source beyond null/absent and
    /// int/float, with the first difference.
    pub mismatches: Vec<String>,
}

impl ImportReport {
    pub fn lines(&self) -> Vec<String> {
        let fmt = |m: &BTreeMap<String, usize>| {
            m.iter()
                .map(|(k, v)| format!("{} {v}", if k.is_empty() { "(none)" } else { k }))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut out = vec![
            format!("rows: {} ({} open)", self.rows, self.open),
            format!("status: {}", fmt(&self.by_status)),
            format!("kind: {}", fmt(&self.by_kind)),
            format!("venue: {}", fmt(&self.by_venue)),
            format!("archive rows: {}", self.archive_rows),
        ];
        if let Some((n, snap)) = self.ladder_coins {
            out.push(format!(
                "ladder state: {n} coin(s), {}",
                if snap {
                    "with a balance snapshot"
                } else {
                    "no balance snapshot (the first run writes one)"
                }
            ));
        }
        if let Some(n) = self.pnl_records {
            out.push(format!("pnl ledger: {n} record(s)"));
        }
        if let Some(n) = self.ttl_flags {
            out.push(format!("stale-order flags: {n}"));
        }
        if let Some(n) = self.decision_lines {
            out.push(format!("decision log: {n} line(s)"));
        }
        if let Some(n) = self.notices {
            out.push(format!("signal notices: {n}"));
        }
        if let Some(b) = &self.btc_band {
            out.push(format!("BTC level state: {b}"));
        }
        if let Some((n, inflight)) = self.deploy_venues {
            out.push(format!(
                "deploy state: {n} venue baseline(s){}",
                if inflight { ", a top-up in flight" } else { "" }
            ));
        }
        if let Some((ok, n)) = self.audit {
            out.push(format!(
                "book audit: {}",
                if ok {
                    "clean".to_string()
                } else {
                    format!("{n} finding(s)")
                }
            ));
        }
        if !self.unmodelled.is_empty() {
            out.push(format!("kept verbatim: {}", fmt(&self.unmodelled)));
        }
        if self.mismatches.is_empty() {
            out.push("round trip: every row reads back as written".into());
        } else {
            out.push(format!(
                "round trip: {} row(s) differ",
                self.mismatches.len()
            ));
            out.extend(self.mismatches.iter().map(|m| format!("  {m}")));
        }
        out
    }
}

/// What an import read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Imported {
    pub journal: Journal,
    pub archive: Vec<Order>,
    pub ladder: Option<LadderState>,
    pub pnl: Option<PnlLedger>,
    pub ttl: Option<TtlWarned>,
    /// The decision log, verbatim (every line checked to be JSON).
    pub decisions: Option<String>,
    pub notices: Option<rungbot_notify::signal_notices::NoticeState>,
    pub btc_alert: Option<Value>,
    pub deploy: Option<serde_json::Map<String, Value>>,
    pub audit: Option<Value>,
    /// Every other state file, copied byte for byte after it was checked to parse.
    pub extra: Vec<Extra>,
    /// Source files left out, with the reason.
    pub skipped: Vec<(String, String)>,
    pub report: ImportReport,
}

/// Where a copied file goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    LadderDry,
    LadderOff,
    Regime,
    RegimeHistory,
    Froth,
    AuditMarker,
    ArchiveMarker,
    Watch,
    Research,
    Dashboard,
    Candles,
}

impl Slot {
    pub fn path(self, t: &Targets, name: &str) -> PathBuf {
        match self {
            Slot::LadderDry => t.ladder_dry.clone(),
            Slot::LadderOff => t.ladder_off.clone(),
            Slot::Regime => t.regime.clone(),
            Slot::RegimeHistory => t.regime_history.clone(),
            Slot::Froth => t.froth.clone(),
            Slot::AuditMarker => t.audit_marker.clone(),
            Slot::ArchiveMarker => t.archive_marker.clone(),
            Slot::Watch => t.watch_dir.join(name),
            Slot::Research => t.research_dir.join(name),
            Slot::Dashboard => t.dashboard_dir.join(name),
            Slot::Candles => t.candles_dir.join(name),
        }
    }
}

/// One state file copied as it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Extra {
    /// Relative to the source directory.
    pub source: String,
    /// The file name at the target.
    pub name: String,
    pub slot: Slot,
    pub body: Body,
}

/// How a copied file is checked before it is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Json,
    Text,
    Marker,
}

/// The state files copied as they are: `(source path, slot, check)`. Each is read by
/// this runtime under the same format.
const COPIES: [(&str, Slot, Check); 19] = [
    ("ladder-state.dry.json", Slot::LadderDry, Check::Json),
    ("ladder-state.json", Slot::LadderOff, Check::Json),
    ("regime-state.json", Slot::Regime, Check::Json),
    ("regime-history.json", Slot::RegimeHistory, Check::Json),
    ("froth-state.json", Slot::Froth, Check::Json),
    ("audit-state.json.last", Slot::AuditMarker, Check::Marker),
    (
        "orders-archive.jsonl.last",
        Slot::ArchiveMarker,
        Check::Marker,
    ),
    ("zone-state.json", Slot::Watch, Check::Json),
    ("divergence-state.json", Slot::Watch, Check::Json),
    ("market-verdict.json", Slot::Watch, Check::Json),
    ("backtest-expectation.json", Slot::Watch, Check::Json),
    ("opportunity-ledger.json", Slot::Research, Check::Json),
    ("value-index.json", Slot::Research, Check::Json),
    ("unlock-index.json", Slot::Research, Check::Json),
    ("theses.yaml", Slot::Research, Check::Text),
    ("dashboard/manual-fills.json", Slot::Dashboard, Check::Json),
    (
        "dashboard/wallet-targets.json",
        Slot::Dashboard,
        Check::Json,
    ),
    (
        "dashboard/.deploy-status.json",
        Slot::Dashboard,
        Check::Json,
    ),
    ("dashboard/.revx-cache.json", Slot::Dashboard, Check::Json),
];

/// Dashboard state also copied (the wallet watcher's).
const DASH_EXTRA: [&str; 2] = [".wallets-alert-state.json", ".wallets-last-good.json"];

/// Source files that are known and left out on purpose.
const SKIPPED: [(&str, &str); 7] = [
    ("revx-journal.jsonl", "not read by any job of the source"),
    ("revx-state.json", "not read by any job of the source"),
    (".run.lock", "the run lock; each runtime takes its own"),
    (
        "dashboard/.pairinfo-cache.json",
        "a cache this runtime does not use",
    ),
    (
        "dashboard/public",
        "the collector's outputs, rewritten by every snapshot",
    ),
    (
        "dashboard/.dex-trader-cache",
        "another bot's cache, not part of this runtime",
    ),
    (
        "deploy-state.dry.json",
        "a dry-mode deploy state; the deploy layer runs live only",
    ),
];

fn copy_one(dir: &Path, rel: &str, slot: Slot, check: Check) -> Result<Option<Extra>, String> {
    let p = dir.join(rel);
    if !p.exists() {
        return Ok(None);
    }
    let name = Path::new(rel)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let body = match check {
        Check::Marker => Body::Marker(
            std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .map_err(|e| format!("{}: {e}", p.display()))?,
        ),
        Check::Json | Check::Text => {
            let t = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            if check == Check::Json {
                serde_json::from_str::<Value>(&t).map_err(|e| format!("{}: {e}", p.display()))?;
            }
            Body::Text(t)
        }
    };
    Ok(Some(Extra {
        source: rel.to_string(),
        name,
        slot,
        body,
    }))
}

/// Every copied file present in `dir`, and the known files left out.
/// The copied files, and the known files left out with the reason.
type ExtraFiles = (Vec<Extra>, Vec<(String, String)>);

fn read_extra(dir: &Path) -> Result<ExtraFiles, String> {
    let mut extra = Vec::new();
    for (rel, slot, check) in COPIES {
        extra.extend(copy_one(dir, rel, slot, check)?);
    }
    for n in DASH_EXTRA {
        extra.extend(copy_one(
            dir,
            &format!("dashboard/{n}"),
            Slot::Dashboard,
            Check::Json,
        )?);
    }
    let cache = dir.join("replay").join("cache");
    if let Ok(rd) = std::fs::read_dir(&cache) {
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".json"))
            .collect();
        names.sort();
        for n in names {
            extra.extend(copy_one(
                dir,
                &format!("replay/cache/{n}"),
                Slot::Candles,
                Check::Json,
            )?);
        }
    }
    let mut skipped: Vec<(String, String)> = SKIPPED
        .iter()
        .filter(|(n, _)| dir.join(n).exists())
        .map(|(n, w)| (n.to_string(), w.to_string()))
        .collect();
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut baks: Vec<String> = rd
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.contains(".json.bak"))
            .collect();
        baks.sort();
        skipped.extend(baks.into_iter().map(|n| (n, "a backup copy".to_string())));
    }
    Ok((extra, skipped))
}

/// Read `dir`'s journal and archive. A missing journal is an error here: importing
/// nothing is never what was meant.
pub fn read_dir(dir: &Path) -> Result<Imported, String> {
    let jpath = dir.join(JOURNAL_FILE);
    if !jpath.exists() {
        return Err(format!("no {JOURNAL_FILE} in {}", dir.display()));
    }
    let ordered = store::read_state_ordered(&jpath)?;
    let source: serde_json::Map<String, Value> = ordered.clone().into_iter().collect();
    let journal =
        store::journal_from_rows(ordered).map_err(|e| format!("{}: {e}", jpath.display()))?;
    let archive = store::read_archive(&dir.join(ARCHIVE_FILE))?;
    let (ladder, pnl, ttl) = read_run_state(dir)?;
    let (decisions, notices, btc_alert) = read_run_logs(dir)?;
    let deploy = if dir.join(DEPLOY_SOURCE).exists() {
        Some(store::read_state(&dir.join(DEPLOY_SOURCE))?)
    } else {
        None
    };
    let audit = if dir.join(AUDIT_FILE).exists() {
        Some(Value::Object(store::read_state(&dir.join(AUDIT_FILE))?))
    } else {
        None
    };
    let (extra, skipped) = read_extra(dir)?;

    let mut r = ImportReport {
        rows: journal.orders.len(),
        open: journal.open_orders(None).len(),
        archive_rows: archive.len(),
        ladder_coins: ladder.as_ref().map(|l| {
            (
                l.keys().filter(|k| !k.starts_with('_')).count(),
                l.get("_bal").is_some_and(|b| b.get("booked").is_some()),
            )
        }),
        pnl_records: pnl.as_ref().map(Vec::len),
        ttl_flags: ttl.as_ref().map(IndexMap::len),
        decision_lines: decisions.as_ref().map(|d| d.lines().count()),
        notices: notices.as_ref().map(|n| n.len()),
        btc_band: btc_alert.as_ref().map(|b| {
            b.get("band")
                .and_then(Value::as_str)
                .unwrap_or("ok")
                .to_string()
        }),
        deploy_venues: deploy.as_ref().map(|d| {
            (
                d.get("stable")
                    .and_then(Value::as_object)
                    .map_or(0, |m| m.len()),
                d.get("inflight")
                    .is_some_and(|v| crate::pyfmt::truthy(Some(v))),
            )
        }),
        audit: audit.as_ref().map(|a| {
            (
                a.get("ok").and_then(Value::as_bool).unwrap_or(false),
                a.get("findings")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len),
            )
        }),
        ..Default::default()
    };
    for o in journal.orders.values() {
        *r.by_status.entry(o.status.clone()).or_default() += 1;
        *r.by_kind.entry(o.kind.clone()).or_default() += 1;
        *r.by_venue.entry(o.exch.clone()).or_default() += 1;
        for k in o.extra.keys() {
            *r.unmodelled.entry(k.clone()).or_default() += 1;
        }
    }
    for (cid, src) in &source {
        let ours = journal
            .get(cid)
            .map(|o| serde_json::to_value(o).expect("an order serialises"))
            .unwrap_or(Value::Null);
        if let Some(d) = first_difference(src, &ours, cid) {
            r.mismatches.push(d);
        }
    }
    Ok(Imported {
        journal,
        archive,
        ladder,
        pnl,
        ttl,
        decisions,
        notices,
        btc_alert,
        deploy,
        audit,
        extra,
        skipped,
        report: r,
    })
}

type RunLogs = (
    Option<String>,
    Option<rungbot_notify::signal_notices::NoticeState>,
    Option<Value>,
);

/// The decision log, the notice dedupe and the level-alert state, when present. Each
/// must read cleanly.
fn read_run_logs(dir: &Path) -> Result<RunLogs, String> {
    let read = |name: &str| -> Result<Option<(PathBuf, String)>, String> {
        let p = dir.join(name);
        if !p.exists() {
            return Ok(None);
        }
        let t = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(Some((p, t)))
    };
    let decisions = match read(DECISIONS_FILE)? {
        Some((p, t)) => {
            for (i, line) in t.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
                serde_json::from_str::<Value>(line)
                    .map_err(|e| format!("{} line {}: {e}", p.display(), i + 1))?;
            }
            Some(t)
        }
        None => None,
    };
    let notices = match read(NOTICES_FILE)? {
        Some((p, t)) => Some(
            serde_json::from_str::<rungbot_notify::signal_notices::NoticeState>(&t)
                .map_err(|e| format!("{}: {e}", p.display()))?,
        ),
        None => None,
    };
    let btc_alert = match read(BTC_ALERT_FILE)? {
        Some((p, t)) => match serde_json::from_str::<Value>(&t) {
            Ok(v @ Value::Object(_)) => Some(v),
            Ok(_) => return Err(format!("{}: expected an object", p.display())),
            Err(e) => return Err(format!("{}: {e}", p.display())),
        },
        None => None,
    };
    Ok((decisions, notices, btc_alert))
}

type RunState = (Option<LadderState>, Option<PnlLedger>, Option<TtlWarned>);

/// The run-state files that exist in `dir`. Each must read cleanly: an import that
/// silently drops a ladder state or a ledger would start the new runtime from nothing.
fn read_run_state(dir: &Path) -> Result<RunState, String> {
    let exists = |name: &str| dir.join(name).exists();
    let ladder = if exists(LADDER_SOURCE) {
        Some(store::load_ladder(&dir.join(LADDER_SOURCE))?)
    } else {
        None
    };
    let pnl = if exists(PNL_SOURCE) {
        let p = dir.join(PNL_SOURCE);
        let raw = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Array(v)) => Some(v),
            Ok(_) => return Err(format!("{}: expected a list of records", p.display())),
            Err(e) => return Err(format!("{}: {e}", p.display())),
        }
    } else {
        None
    };
    let ttl = if exists(TTL_SOURCE) {
        let p = dir.join(TTL_SOURCE);
        let raw = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let m: IndexMap<String, Value> =
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", p.display()))?;
        let mut t = TtlWarned::new();
        for (k, v) in m {
            let f = v
                .as_f64()
                .ok_or_else(|| format!("{}: {k} is not a timestamp", p.display()))?;
            t.insert(k, f);
        }
        Some(t)
    } else {
        None
    };
    Ok((ladder, pnl, ttl))
}

/// Where an import writes each kind of state.
#[derive(Debug, Clone, PartialEq)]
pub struct Targets {
    pub journal: PathBuf,
    pub archive: PathBuf,
    /// The ladder state per trade mode: `live`, `dry`, `off`.
    pub ladder_live: PathBuf,
    pub ladder_dry: PathBuf,
    pub ladder_off: PathBuf,
    pub pnl: PathBuf,
    pub ttl: PathBuf,
    pub decisions: PathBuf,
    pub notices: PathBuf,
    pub btc_alert: PathBuf,
    pub deploy: PathBuf,
    pub audit: PathBuf,
    pub audit_marker: PathBuf,
    pub archive_marker: PathBuf,
    pub regime: PathBuf,
    pub regime_history: PathBuf,
    pub froth: PathBuf,
    /// The watchers' own state (`rungbot watch`: zone, divergence), the market verdict
    /// the regime watch quotes and the monthly backtest's expectation: the state dir,
    /// where `watch.state_dir` and `rungbot backtest monthly` look by default.
    pub watch_dir: PathBuf,
    /// The research ledger and indexes.
    pub research_dir: PathBuf,
    /// The dashboard collector's inputs and caches.
    pub dashboard_dir: PathBuf,
    /// Daily candles the fill-odds report reads.
    pub candles_dir: PathBuf,
}

impl Targets {
    /// Every file in the journal's directory, under the names a run config uses by
    /// default.
    pub fn beside(journal: &Path) -> Targets {
        let dir = journal
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let sib = |n: &str| store::sibling(journal, n);
        let archive = store::archive_path(journal);
        Targets {
            journal: journal.to_path_buf(),
            archive_marker: marker(&archive),
            archive,
            ladder_live: sib(store::LADDER_FILE),
            ladder_dry: sib("ladder-state.dry.json"),
            ladder_off: sib("ladder-state.off.json"),
            pnl: sib(store::PNL_FILE),
            ttl: sib(store::TTL_FILE),
            decisions: sib(DECISIONS_FILE),
            notices: sib(NOTICES_FILE),
            btc_alert: sib(BTC_ALERT_FILE),
            deploy: sib(DEPLOY_FILE),
            audit_marker: marker(&sib(AUDIT_FILE)),
            audit: sib(AUDIT_FILE),
            regime: sib("regime-state.json"),
            regime_history: sib("regime-history.json"),
            froth: sib("froth-state.json"),
            watch_dir: dir.clone(),
            research_dir: dir.join("research"),
            dashboard_dir: dir.join("dashboard"),
            candles_dir: dir.join("replay").join("cache"),
        }
    }

    /// The paths a run config reads: its single-file overrides count.
    pub fn from_config(cfg: &crate::run::config::RunConfig) -> Targets {
        let mut t = Targets::beside(&cfg.journal_path());
        let per_mode = |mode: &str| {
            let mut c = cfg.clone();
            c.trade_mode = mode.into();
            c.ladder_path()
        };
        t.ladder_live = per_mode("live");
        t.ladder_dry = per_mode("dry");
        t.ladder_off = per_mode("off");
        t.pnl = cfg.pnl_path();
        t.ttl = cfg.ttl_path();
        t.decisions = cfg.decisions_path();
        t.notices = cfg.notices_path();
        t.btc_alert = cfg.btc_alert_path();
        t.deploy = cfg.deploy_state_path();
        t.audit = cfg.audit_state_path();
        t.audit_marker = cfg.audit_marker_path();
        t.regime = cfg.regime_path();
        t.regime_history = cfg.regime_history_path();
        t.froth = cfg.froth_path();
        t.watch_dir = cfg.state_dir.clone();
        t.research_dir = cfg.state_dir.join("research");
        t.candles_dir = cfg.fillodds_cache_path();
        if let Ok(d) = crate::dashboard::DashConfig::from_run(cfg) {
            t.dashboard_dir = d.work_dir;
        } else {
            t.dashboard_dir = cfg.state_dir.join("dashboard");
        }
        t
    }
}

fn marker(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".last");
    PathBuf::from(s)
}

/// What one target receives.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Text(String),
    /// A daily marker: an empty file whose modification time is the source's.
    Marker(std::time::SystemTime),
}

/// One file an import writes.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The source file, relative to the source directory.
    pub source: String,
    pub target: PathBuf,
    pub body: Body,
}

/// A target against what is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Nothing there yet (or an empty file, or a journal without rows).
    New,
    /// Already holds exactly this: re-importing is a no-op.
    Same,
    /// Holds something else: replaced only with `--force`.
    Differs,
}

impl Step {
    pub fn status(&self) -> Status {
        let Ok(meta) = std::fs::metadata(&self.target) else {
            return Status::New;
        };
        match &self.body {
            Body::Marker(t) => match meta.modified() {
                Ok(m) if secs(m).abs_diff(secs(*t)) <= 1 => Status::Same,
                _ => Status::Differs,
            },
            Body::Text(text) => {
                if meta.len() == 0 {
                    return Status::New;
                }
                match std::fs::read_to_string(&self.target) {
                    Ok(cur) if cur == *text => Status::Same,
                    // A journal file without a row holds nothing to lose.
                    Ok(_)
                        if self.source == JOURNAL_FILE
                            && store::load_journal(&self.target)
                                .is_ok_and(|j| j.orders.is_empty()) =>
                    {
                        Status::New
                    }
                    _ => Status::Differs,
                }
            }
        }
    }
}

fn secs(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Every file of an import, in write order: the journal last, so a journal on disk
/// means the rest is there too.
pub fn plan(imported: &Imported, t: &Targets) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut add = |source: &str, target: &Path, body: Body| {
        steps.push(Step {
            source: source.to_string(),
            target: target.to_path_buf(),
            body,
        })
    };
    let has_archive = std::fs::metadata(&t.archive).is_ok_and(|m| m.len() > 0);
    if !imported.archive.is_empty() || has_archive {
        let mut text = String::new();
        for o in &imported.archive {
            text.push_str(&store::archive_line(o));
            text.push('\n');
        }
        add(ARCHIVE_FILE, &t.archive, Body::Text(text));
    }
    if let Some(l) = &imported.ladder {
        add(
            LADDER_SOURCE,
            &t.ladder_live,
            Body::Text(crate::pyfmt::dumps(l, Some(2))),
        );
    }
    if let Some(p) = &imported.pnl {
        add(
            PNL_SOURCE,
            &t.pnl,
            Body::Text(crate::pyfmt::dumps(p, Some(2))),
        );
    }
    if let Some(x) = &imported.ttl {
        add(
            TTL_SOURCE,
            &t.ttl,
            Body::Text(crate::pyfmt::dumps(x, Some(2))),
        );
    }
    if let Some(d) = &imported.decisions {
        add(DECISIONS_FILE, &t.decisions, Body::Text(d.clone()));
    }
    if let Some(n) = &imported.notices {
        add(
            NOTICES_FILE,
            &t.notices,
            Body::Text(rungbot_notify::signal_notices::to_json(n)),
        );
    }
    if let Some(b) = &imported.btc_alert {
        add(
            BTC_ALERT_FILE,
            &t.btc_alert,
            Body::Text(crate::pyfmt::dumps(b, None)),
        );
    }
    if let Some(d) = &imported.deploy {
        add(
            DEPLOY_SOURCE,
            &t.deploy,
            Body::Text(crate::pyfmt::dumps(d, Some(2))),
        );
    }
    if let Some(a) = &imported.audit {
        add(
            AUDIT_FILE,
            &t.audit,
            Body::Text(crate::pyfmt::dumps(a, Some(1))),
        );
    }
    for x in &imported.extra {
        add(&x.source, &x.slot.path(t, &x.name), x.body.clone());
    }
    add(
        JOURNAL_FILE,
        &t.journal,
        Body::Text(store::journal_text(&imported.journal)),
    );
    steps
}

/// The import as a table: each source file, where it goes and what happens there; then
/// the source files it leaves out, and why.
pub fn mapping_lines(imported: &Imported, t: &Targets) -> Vec<String> {
    let mut out = vec!["mapping (source -> target: status):".to_string()];
    for s in plan(imported, t) {
        let st = match s.status() {
            Status::New => "new",
            Status::Same => "unchanged",
            Status::Differs => "differs, replaced only with --force",
        };
        out.push(format!("  {} -> {}: {st}", s.source, s.target.display()));
    }
    for (name, why) in &imported.skipped {
        out.push(format!("  {name}: not imported ({why})"));
    }
    out.push(
        "  halt and sell-arm files: not copied; `import-cex DIR --config` carries their paths, \
         so both runtimes obey the same files"
            .to_string(),
    );
    out
}

/// Write an import. Refuses, before writing anything, when a target already holds
/// something else, unless `force`; a target that already holds exactly what the import
/// would write is left alone, so a second import of the same source is a no-op.
/// Returns the steps written.
pub fn write_to(imported: &Imported, t: &Targets, force: bool) -> Result<Vec<Step>, String> {
    let steps = plan(imported, t);
    if !force {
        if let Some(s) = steps.iter().find(|s| s.status() == Status::Differs) {
            let what = if s.source == JOURNAL_FILE {
                format!(
                    "{} already holds {} row(s)",
                    s.target.display(),
                    store::load_journal(&s.target).map_or(0, |j| j.orders.len())
                )
            } else {
                format!("{} already exists", s.target.display())
            };
            return Err(format!("{what}; pass --force to replace it"));
        }
    }
    let mut written = Vec::new();
    for s in steps {
        if s.status() == Status::Same {
            continue;
        }
        match &s.body {
            Body::Text(text) => store::write_atomic(&s.target, text)?,
            Body::Marker(t) => {
                if let Some(p) = s.target.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
                }
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&s.target)
                    .map_err(|e| format!("{}: {e}", s.target.display()))?;
                f.set_modified(*t)
                    .map_err(|e| format!("{}: {e}", s.target.display()))?;
            }
        }
        written.push(s);
    }
    Ok(written)
}

/// Write an import to `journal_path`, with every other file beside it (see
/// [`Targets::beside`]). Returns the archive's path.
pub fn write(imported: &Imported, journal_path: &Path, force: bool) -> Result<PathBuf, String> {
    let t = Targets::beside(journal_path);
    write_to(imported, &t, force)?;
    Ok(t.archive)
}

/// The first way `ours` differs from `src`, ignoring null-vs-absent and int-vs-float.
pub fn first_difference(src: &Value, ours: &Value, at: &str) -> Option<String> {
    match (src, ours) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a
                .iter()
                .chain(b.iter())
                .filter(|(_, v)| !v.is_null())
                .map(|(k, _)| k)
                .collect();
            for k in keys {
                let (x, y) = (
                    a.get(k).unwrap_or(&Value::Null),
                    b.get(k).unwrap_or(&Value::Null),
                );
                if let Some(d) = first_difference(x, y, &format!("{at}.{k}")) {
                    return Some(d);
                }
            }
            None
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(format!("{at}: {} items, read back {}", a.len(), b.len()));
            }
            a.iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (x, y))| first_difference(x, y, &format!("{at}[{i}]")))
        }
        (Value::Number(a), Value::Number(b)) if a.as_f64() == b.as_f64() => None,
        (a, b) if a == b => None,
        (a, b) => Some(format!("{at}: {a} read back as {b}")),
    }
}
