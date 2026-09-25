//! The daily book audit: does the order journal agree with the venues?
//!
//! Per venue (Revolut X, then Gate):
//!
//! 1. every journal row that says it is open must still be listed by the venue;
//! 2. a row listed on both sides must have the same size (a resize at the venue leaves
//!    the journal's quote stale);
//! 3. every order the venue lists must be known to the journal (any status);
//! 4. stable locked at the venue must match the journal's open buys within max($5, 2%),
//!    anywhere between their remaining and their full size.
//!
//! Findings ride the housekeeping mail as warnings; the last outcome is written to the
//! audit state file for the dashboard banner. Read-only on the venues.
//!
//! One correction to the reference: on Revolut X, USDC the venue holds as locked is a
//! withdrawal on its way out (the Gate top-up), not cash in a resting buy, unless the
//! journal has an open USDC-quoted buy there. It no longer reads as a locked-cash gap.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use indexmap::IndexMap;
use rungbot_core::watch::pyfmt::{signed, sum as py_sum};
use serde::Serialize;

use crate::journal::{is_open_status, Journal, Order};
use crate::pyfmt::{fixed, g, head};
use crate::reconcile::VenueSource;
use crate::run::config::RunConfig;
use crate::run::hooks::{AuditHook, AuditReport, HookCtx};
use crate::run::RunResult;
use crate::venue::{Balance, ParsedOrder};

/// The venues audited, in order.
pub const VENUES: [&str; 2] = ["revx", "gate"];

/// The stable assets whose locked amount is compared, per venue.
pub fn stable_assets(exch: &str) -> &'static [&'static str] {
    match exch {
        "revx" => &["USD", "USDC"],
        "gate" => &["USDT", "USDC"],
        _ => &[],
    }
}

/// What the dashboard banner reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AuditState {
    pub ts: f64,
    pub ok: bool,
    pub findings: Vec<String>,
    /// Pairs checked per venue.
    pub checked: IndexMap<String, i64>,
}

fn or_none(s: &str) -> &str {
    if s.is_empty() {
        "None"
    } else {
        s
    }
}

/// The pairs to read on a venue: every journal pair there and every route to it.
pub fn pairs_for(cfg: &RunConfig, exch: &str, journal: &Journal) -> Vec<String> {
    let mut pairs: BTreeSet<String> = journal
        .orders
        .values()
        .filter(|o| o.exch == exch && !o.pair.is_empty())
        .map(|o| o.pair.clone())
        .collect();
    pairs.extend(
        cfg.routing
            .iter()
            .filter(|(_, r)| r.exch == exch && !r.pair.is_empty())
            .map(|(_, r)| r.pair.clone()),
    );
    pairs.into_iter().collect()
}

fn is_usdc_quoted(pair: &str) -> bool {
    pair.ends_with("/USDC") || pair.ends_with("-USDC") || pair.ends_with("_USDC")
}

/// The comparison for one venue: finding texts, empty when the books agree.
pub fn compare(
    exch: &str,
    journal: &Journal,
    venue_open_by_pair: &IndexMap<String, Vec<ParsedOrder>>,
    balances_full: &BTreeMap<String, Balance>,
) -> Vec<String> {
    let mut findings = Vec::new();
    let mut venue_ids: IndexMap<String, (String, ParsedOrder)> = IndexMap::new();
    for (pair, lst) in venue_open_by_pair {
        for x in lst {
            if !x.order_id.is_empty() {
                venue_ids.insert(x.order_id.clone(), (pair.clone(), x.clone()));
            }
        }
    }
    let open_here = |o: &&Order| {
        o.exch == exch
            && is_open_status(&o.status)
            && o.order_id.as_deref().is_some_and(|s| !s.is_empty())
    };
    for (cid, o) in &journal.orders {
        if !open_here(&o) {
            continue;
        }
        let oid = o.order_id.clone().unwrap_or_default();
        if venue_open_by_pair.contains_key(&o.pair) && !venue_ids.contains_key(&oid) {
            findings.push(format!(
                "{exch} {} {} {cid}: journal open, venue has no order {oid} (missed fill or cancel?)",
                or_none(&o.sym),
                or_none(&o.kind)
            ));
        }
    }
    let mut by_oid: BTreeMap<String, &Order> = BTreeMap::new();
    for o in journal.orders.values().filter(open_here) {
        by_oid.insert(o.order_id.clone().unwrap_or_default(), o);
    }
    for (oid, (_pair, x)) in &venue_ids {
        let Some(o) = by_oid.get(oid) else {
            continue;
        };
        let venue_q = x.qty * x.price.unwrap_or(0.0);
        let full_q = o.quote.unwrap_or(0.0);
        let jq = full_q - o.part_quote.unwrap_or(0.0);
        let gap = (venue_q - jq).abs().min((venue_q - full_q).abs());
        if venue_q > 0.0 && jq > 0.0 && gap > (0.01 * jq).max(1.0) {
            findings.push(format!(
                "{exch} {} rung {} {}: venue size ${} vs journal ${} ({}); resized at the venue, journal not updated",
                or_none(&o.sym),
                o.rung.map_or("None".to_string(), |r| r.to_string()),
                o.client_id,
                fixed(venue_q, 2),
                fixed(jq, 2),
                signed(venue_q - jq, 2)
            ));
        }
    }
    let known: BTreeSet<String> = journal
        .orders
        .values()
        .filter_map(|o| o.order_id.clone().filter(|s| !s.is_empty()))
        .collect();
    for (oid, (pair, x)) in &venue_ids {
        if !known.contains(oid) {
            let side = if x.side.is_empty() {
                "?".to_string()
            } else {
                x.side.to_lowercase()
            };
            findings.push(format!(
                "{exch} {pair} {side} {} @ {}: resting at the venue, not in the journal",
                g(x.qty, 6),
                g(x.price.unwrap_or(0.0), 6)
            ));
        }
    }
    let open_buys: Vec<&Order> = journal
        .orders
        .values()
        .filter(|o| o.exch == exch && is_open_status(&o.status))
        .filter(|o| o.side.is_empty() || o.side.to_lowercase() == "buy")
        .collect();
    let usdc_buys = open_buys.iter().any(|o| is_usdc_quoted(&o.pair));
    let mut locked = 0.0;
    for asset in stable_assets(exch) {
        // A revx USDC lock with no USDC-quoted buy behind it is a withdrawal in flight.
        if exch == "revx" && *asset == "USDC" && !usdc_buys {
            continue;
        }
        locked += balances_full.get(*asset).map_or(0.0, |b| b.locked);
    }
    let remaining = py_sum(
        open_buys
            .iter()
            .map(|o| o.quote.unwrap_or(0.0) - o.part_quote.unwrap_or(0.0)),
    );
    let full = py_sum(open_buys.iter().map(|o| o.quote.unwrap_or(0.0)));
    let tol = (0.02 * locked.max(full)).max(5.0);
    if locked < remaining - tol || locked > full + tol {
        findings.push(format!(
            "{exch}: ${} stable locked at the venue vs ${} (remaining) to ${} (full size) in journal open buys (tolerance ${})",
            fixed(locked, 2),
            fixed(remaining, 2),
            fixed(full, 2),
            fixed(tol, 2)
        ));
    }
    findings
}

/// Audit every venue with a client. Never fails: a venue that cannot be read is a
/// finding.
pub fn run(
    cfg: &RunConfig,
    venues: &dyn VenueSource,
    journal: &Journal,
    now: f64,
) -> (Vec<RunResult>, AuditState) {
    let mut checked: IndexMap<String, i64> = IndexMap::new();
    let mut findings = Vec::new();
    for exch in VENUES {
        let Ok(client) = venues.venue(exch) else {
            continue;
        };
        let read = (|| -> Result<Vec<String>, String> {
            let bal = client.balances_full().map_err(|e| e.to_string())?;
            let mut venue_open = IndexMap::new();
            for pair in pairs_for(cfg, exch, journal) {
                let lst = client.open_orders(&pair).map_err(|e| e.to_string())?;
                venue_open.insert(pair, lst);
            }
            checked.insert(exch.to_string(), venue_open.len() as i64);
            Ok(compare(exch, journal, &venue_open, &bal))
        })();
        match read {
            Ok(f) => findings.extend(f),
            Err(e) => findings.push(format!(
                "{exch}: audit could not read the venue ({})",
                head(&e, 100)
            )),
        }
    }
    let mut results: Vec<RunResult> = findings
        .iter()
        .map(|f| RunResult {
            hk: true,
            audit: true,
            warn: Some(format!("BOOK AUDIT: {f}")),
            ..Default::default()
        })
        .collect();
    if findings.is_empty() {
        results.push(RunResult {
            hk: true,
            audit: true,
            info: Some(format!(
                "BOOK AUDIT ok: {} pairs on {} venues agree with the journal",
                checked.values().sum::<i64>(),
                checked.len()
            )),
            ..Default::default()
        });
    }
    let state = AuditState {
        ts: now,
        ok: findings.is_empty(),
        findings,
        checked,
    };
    (results, state)
}

/// Write the audit state (tmp and rename).
pub fn save_state(path: &Path, st: &AuditState) -> Result<(), String> {
    crate::store::write_atomic(path, &crate::pyfmt::dumps(st, Some(1)))
}

/// Read the last audit state, `None` when there is none or it does not parse.
pub fn load_state(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The audit hook a live run calls once a day.
#[derive(Debug, Default, Clone, Copy)]
pub struct BookAudit;

impl AuditHook for BookAudit {
    fn run(&mut self, ctx: &mut HookCtx) -> Option<Result<AuditReport, String>> {
        let (results, state) = run(ctx.cfg, ctx.venues, ctx.journal, ctx.now);
        if let Err(e) = save_state(&ctx.cfg.audit_state_path(), &state) {
            eprintln!("WARN: could not write the audit state: {e}");
        }
        Some(Ok(AuditReport {
            results,
            ok: state.ok,
            findings: state.findings.len(),
            checked: state.checked.into_iter().collect(),
        }))
    }
}
