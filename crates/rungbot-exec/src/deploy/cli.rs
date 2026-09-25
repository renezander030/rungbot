//! The deploy layer's manual commands (`rungbot-exec deploy …`).
//!
//! * `status`: the state path, baselines, each venue's quote balance, open zones.
//! * `plan [usd]`: the zone plan for `usd` (default 100) per venue. Places nothing.
//! * `tranche <usd> <venue> [--only A,B]`: roll the venue's zones and ladder `usd` more;
//!   `tranche 0 <venue> --only A` re-ladders only those coins.
//! * `market <share%> <venue> <SYM>`: roll one coin's zones, buy `share%` of their budget
//!   at market and ladder the rest.
//! * `cancel [venue]`: cancel every resting deploy zone (on one venue).
//!
//! The three that place or cancel refuse while live trading is off or the halt file is
//! present (`cancel` works while halted, on purpose), and the caller holds the run lock.
//!
//! Two corrections to the reference: `market` with nothing to spend no longer sends a
//! $0 market buy, and a `tranche` of fresh money now raises the venue's baseline by the
//! new money it placed (never more than the balance shows above the baseline), so the
//! next run does not ladder the same money a second time.
//!
//! Inputs are checked before anything reaches a venue: amounts must be finite, a
//! `tranche` at least 0 (there is no manual withhold), a `market` share above 0 and at
//! most 100, and `cancel` refuses a venue it does not know.

use std::collections::BTreeSet;
use std::io::Write;

use serde_json::{json, Value};

use super::{load_state, save_state, Layer, Only, CANCEL_NOTE, VENUES};
use crate::journal::Order;
use crate::pyfmt::{fixed, g};

macro_rules! say {
    ($w:expr, $($t:tt)*) => {
        writeln!($w, $($t)*).map_err(|e| e.to_string())?
    };
}

fn print_results(layer: &Layer, out: &mut dyn Write) -> Result<(), String> {
    for r in &layer.results {
        let t = r
            .get("done")
            .or_else(|| r.get("warn"))
            .or_else(|| r.get("err"))
            .unwrap_or("None");
        say!(out, "{t}");
    }
    Ok(())
}

/// Print the results; a journal write that failed on the way makes the command fail.
fn finish(layer: &Layer, out: &mut dyn Write) -> Result<(), String> {
    print_results(layer, out)?;
    match layer.write_failed() {
        Some(e) => Err(format!("{e}; nothing more was placed")),
        None => Ok(()),
    }
}

fn venues_usage(what: &str) -> String {
    format!(
        "usage: rungbot-exec deploy {what}",
        what = what.replace("{V}", &VENUES.join("|"))
    )
}

/// `status`.
pub fn status(layer: &Layer, out: &mut dyn Write) -> Result<(), String> {
    let path = layer.cfg.deploy_state_path();
    let st = load_state(&path)?;
    let quotes = Layer::quotes(layer.cfg);
    say!(out, "state: {}", path.display());
    let stable = st.get("stable").cloned().unwrap_or_else(|| json!({}));
    let stable = if crate::pyfmt::truthy(Some(&stable)) {
        stable
    } else {
        json!({})
    };
    let ts = match st.get("ts") {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::Number(n)) => n
            .as_f64()
            .map(crate::pyfmt::float_repr)
            .unwrap_or_else(|| n.to_string()),
        Some(v) => crate::pyfmt::value_str(v),
    };
    say!(
        out,
        "baseline: {}  ts: {ts}",
        crate::pyfmt::dumps(&stable, None)
    );
    for exch in layer.clients() {
        let q = quotes.get(exch).cloned().unwrap_or_default();
        let b = layer
            .client(exch)?
            .balances_full()
            .map_err(|e| e.to_string())?
            .get(&q)
            .copied()
            .unwrap_or_default();
        say!(
            out,
            "{exch:8} {q}: free ${}  locked ${}  total ${}",
            fixed(b.free, 2),
            fixed(b.locked, 2),
            fixed(b.free + b.locked, 2)
        );
    }
    let open: Vec<&Order> = layer.j.open_orders(Some("deploy_buy"));
    say!(out, "open deploy zones: {}", open.len());
    for o in open {
        say!(
            out,
            "  {:5} ${} @ ${} ({}) status={}",
            o.sym,
            fixed(o.quote.unwrap_or(0.0), 2),
            g(o.price.unwrap_or(0.0), 6),
            o.note.as_deref().unwrap_or("None"),
            o.status
        );
    }
    Ok(())
}

/// `plan [usd]`.
pub fn plan(layer: &Layer, budget: f64, out: &mut dyn Write) -> Result<(), String> {
    let label = layer.regime_label();
    say!(
        out,
        "regime: {}  — zone plan for ${} per venue",
        label.to_uppercase(),
        fixed(budget, 2)
    );
    for exch in layer.clients() {
        let client = layer.client(exch)?;
        let vp = layer.venue_plan(client, exch, budget, &label, &None, &Default::default())?;
        say!(
            out,
            "\n{exch}{}:",
            if vp.fallback {
                " (equal split — alloc names no coin here)"
            } else {
                ""
            }
        );
        for (s, rungs) in &vp.plans {
            say!(out, "  {s:5} spot ${}", g(rungs[0].spot, 6));
            for r in rungs {
                say!(
                    out,
                    "        rung {}: ${} -> {} @ ${}  ({})",
                    r.rung,
                    fixed(r.usd, 2),
                    g(r.qty, 6),
                    g(r.price, 6),
                    r.note
                );
            }
        }
        for sk in &vp.skips {
            say!(out, "  skip {sk}");
        }
    }
    Ok(())
}

/// A command-line amount: a finite number (`nan`, `inf` and overflow are refused).
pub fn parse_num(s: &str, what: &str) -> Result<f64, String> {
    let x: f64 = s.trim().parse().map_err(|e| format!("{what}: {e}"))?;
    if !x.is_finite() {
        return Err(format!("{what}: {s:?} is not a finite number"));
    }
    Ok(x)
}

fn check_live(layer: &Layer) -> Result<(), String> {
    if Layer::blocked(layer.cfg).is_some() {
        return Err("blocked: LIVE_TRADING_ENABLED/halt file".into());
    }
    Ok(())
}

/// `tranche <usd> <venue> [--only A,B]`.
pub fn tranche(
    layer: &mut Layer,
    budget: f64,
    venue: &str,
    only: Only,
    out: &mut dyn Write,
) -> Result<(), String> {
    // The reference has no manual withhold: a negative tranche is refused, not laddered
    // lighter.
    if !(budget.is_finite() && budget >= 0.0) {
        return Err(format!(
            "tranche <usd> must be a number of at least 0, got {}",
            crate::pyfmt::float_repr(budget)
        ));
    }
    check_live(layer)?;
    if !VENUES.contains(&venue) {
        return Err(venues_usage("tranche <usd> <{V}>"));
    }
    let client = layer.client(venue)?;
    let quote = Layer::quotes(layer.cfg)
        .get(venue)
        .cloned()
        .unwrap_or_default();
    // New money not yet in the baseline, read before the tranche moves anything: only
    // that part of the budget can be laddered twice by the next run.
    let path = layer.cfg.deploy_state_path();
    let fresh = if budget > 0.0 {
        let st = load_state(&path)?;
        let base = st
            .get("stable")
            .and_then(|s| s.get(venue))
            .and_then(Value::as_f64);
        match (base, client.balances_full()) {
            (None, _) => 0.0,
            (Some(base), Ok(b)) => {
                let (explained, _) = layer.explained(&st, &[venue]);
                let x = b.get(&quote).copied().unwrap_or_default();
                (x.free + x.locked - (base + explained[venue])).max(0.0)
            }
            (Some(_), Err(e)) => {
                layer.warn(format!(
                    "deploy tranche: {venue} balances unreadable, baseline not raised: {e}"
                ));
                0.0
            }
        }
    } else {
        0.0
    };
    let ts = (layer.clock)();
    let (cids, rolled) = layer.deploy_tranche_rolled(client, venue, &quote, budget, ts, &only)?;
    layer.place_unplaced(client, &cids)?;
    // What reached the venue, less the rolled budget that was already in the baseline.
    let placed = rungbot_core::watch::pyfmt::sum(
        cids.iter()
            .filter_map(|c| layer.j.get(c))
            .filter(|o| o.order_id.as_deref().is_some_and(|s| !s.is_empty()))
            .map(|o| o.quote.unwrap_or(0.0)),
    );
    let bump = fresh.min(budget).min((placed - rolled).max(0.0));
    if bump > 0.0 {
        // The fresh money this laddered is deployed: count it in the baseline, or the
        // next run reads it as new capital and ladders it again.
        let mut st = load_state(&path)?;
        if let Some(b) = st
            .get_mut("stable")
            .and_then(Value::as_object_mut)
            .and_then(|m| m.get_mut(venue))
        {
            *b = json!(b.as_f64().unwrap_or(0.0) + bump);
            if let Err(e) = save_state(&path, &st) {
                layer.push(
                    None,
                    None,
                    false,
                    super::Lvl::Err,
                    format!(
                        "deploy state write failed, baseline not raised by ${}: {e}",
                        fixed(bump, 2)
                    ),
                );
            }
        }
    }
    finish(layer, out)
}

/// `market <share%> <venue> <SYM>`.
pub fn market(
    layer: &mut Layer,
    share_pct: f64,
    venue: &str,
    sym: &str,
    out: &mut dyn Write,
) -> Result<(), String> {
    if !(share_pct > 0.0 && share_pct <= 100.0) {
        return Err(format!(
            "--market share must be above 0 and at most 100, got {}",
            crate::pyfmt::float_repr(share_pct)
        ));
    }
    check_live(layer)?;
    if !VENUES.contains(&venue) {
        return Err(venues_usage("market <share%> <{V}> <SYM>"));
    }
    let client = layer.client(venue)?;
    let ts = (layer.clock)();
    let only: Only = Some(BTreeSet::from([sym.to_string()]));
    let rolled = layer.cancel_open_zones(client, venue, &only)?;
    let fresh = 0.0;
    let budget = rolled + fresh;
    let mkt = rungbot_core::watch::pyfmt::round(budget * share_pct / 100.0, 2);
    let mut rest = (budget - mkt - (mkt * 0.003).max(1.0)).max(0.0);
    let pair = Layer::pair_for(layer.cfg, venue, sym)?;
    if mkt <= 0.0 {
        layer.warn(format!(
            "market buy {sym} skipped: ${} to spend ({}% of ${} rolled from resting zones)",
            fixed(mkt, 2),
            g(share_pct, 6),
            fixed(budget, 2)
        ));
    } else {
        let prefix = if venue == "revx" { "depx" } else { "dep" };
        let cid = format!("{prefix}{sym}{}m", ts as i64);
        layer.j.record(Order {
            client_id: cid.clone(),
            sym: sym.into(),
            exch: venue.into(),
            pair: pair.clone(),
            side: "buy".into(),
            kind: "deploy_buy".into(),
            status: "pending".into(),
            quote: Some(mkt),
            rung: Some(0),
            ts: Some(ts),
            note: Some(format!(
                "market {}% of ${} tranche",
                g(share_pct, 6),
                fixed(budget, 2)
            )),
            ..Default::default()
        });
        let placed = match layer.save() {
            Ok(()) => client
                .market_buy(&pair, mkt, Some(&cid))
                .and_then(|raw| client.parse_order(&raw))
                .map_err(|e| e.to_string()),
            Err(e) => {
                // Not on disk, so not sent; nothing else is placed either.
                rest = 0.0;
                Err(format!("not placed, {e}"))
            }
        };
        match placed {
            Ok(lf) => {
                layer.j.update(&cid, |o| {
                    o.order_id = Some(lf.order_id.clone()).filter(|s| !s.is_empty());
                    o.status = "new".into();
                    o.last_error = None;
                });
                layer.save_after();
                layer.push(
                    Some(sym),
                    Some("buy"),
                    true,
                    super::Lvl::Done,
                    format!(
                        "MARKET BUY {sym} ${} ({}% of ${}) — core add",
                        fixed(mkt, 2),
                        g(share_pct, 6),
                        fixed(budget, 2)
                    ),
                );
            }
            Err(e) => {
                layer.j.update(&cid, |o| {
                    o.status = "error".into();
                    o.last_error = Some(e.clone());
                });
                layer.save_after();
                layer.push(
                    Some(sym),
                    None,
                    false,
                    super::Lvl::Warn,
                    format!("market buy {sym} ${} FAILED: {e}", fixed(mkt, 2)),
                );
                if layer.write_failed().is_none() {
                    rest = budget;
                }
            }
        }
    }
    if rest > 0.0 {
        let label = layer.regime_label();
        match layer.plan_coin(
            client,
            venue,
            &pair,
            rest,
            &label,
            &mut Default::default(),
            Some(sym),
            None,
        )? {
            Err(skip) => layer.push(
                Some(sym),
                None,
                false,
                super::Lvl::Warn,
                format!("zone leg skipped: {skip}"),
            ),
            Ok(rungs) => {
                let cids = layer.journal_and_place(venue, &[(sym.to_string(), rungs)], ts, None)?;
                layer.place_unplaced(client, &cids)?;
            }
        }
    }
    finish(layer, out)
}

/// `cancel [venue]`.
pub fn cancel(layer: &mut Layer, venue: Option<&str>, out: &mut dyn Write) -> Result<(), String> {
    // An unknown venue is refused: it must never widen to "cancel everything".
    if venue.is_some_and(|v| !VENUES.contains(&v)) {
        return Err(venues_usage("cancel [{V}]"));
    }
    let open: Vec<Order> = layer
        .j
        .open_orders(Some("deploy_buy"))
        .into_iter()
        .filter(|o| o.order_id.as_deref().is_some_and(|s| !s.is_empty()))
        .filter(|o| venue.is_none_or(|v| o.exch == v))
        .cloned()
        .collect();
    if open.is_empty() {
        say!(out, "no open deploy zones to cancel");
        return Ok(());
    }
    let clients = layer.clients();
    let mut n = 0;
    for o in &open {
        if !clients.contains(&o.exch.as_str()) {
            continue;
        }
        let c = layer.client(&o.exch)?;
        match layer.cancel_ours(c, o, CANCEL_NOTE)? {
            Ok(_) => {
                say!(
                    out,
                    "canceled {:5} ${} @ ${}",
                    o.sym,
                    fixed(o.quote.unwrap_or(0.0), 2),
                    g(o.price.unwrap_or(0.0), 6)
                );
                n += 1;
            }
            Err(e) => say!(
                out,
                "FAILED {} @ ${}: {e} (may have just filled)",
                o.sym,
                g(o.price.unwrap_or(0.0), 6)
            ),
        }
    }
    say!(
        out,
        "canceled {n}/{} deploy zones{}. Also: touch {} to freeze the bot.",
        open.len(),
        venue.map(|v| format!(" on {v}")).unwrap_or_default(),
        layer.cfg.halt_file.display()
    );
    match layer.write_failed() {
        Some(e) => Err(format!("{e}; nothing more was cancelled")),
        None => Ok(()),
    }
}
