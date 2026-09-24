//! The regime-shift mail: the flip, the evidence behind it, and what it means for the
//! resting deploy zones, priced against live spot.
//!
//! A bare "regime changed" is useless, so the message carries three things: how long the
//! new label has held, the data that produced it (BTC against its long averages, breadth,
//! each coin's RUN gate), and where the resting buy zones sit relative to spot, which is
//! what decides whether they will ever fill in the market the label describes.

use super::json::Json;
use super::pyfmt::{comma, fixed, ljust, rjust, sum as pysum};
use super::regime_state::Shift;
use super::textwrap::wrap;
use super::{upper, Hints};

/// Measured over 3,306 days of BTC dailies (2017-08 onwards), 16 events where the BTC leg
/// of the label held 14 straight days: how often a limit order that far below the
/// confirmation-day price ever filled within 90 days.
pub const FILL_STATS: [(&str, &str); 3] = [("-10%", "53%"), ("-20%", "40%"), ("-30%", "20%")];
/// The same events: median forward return of buying at market on the confirmation day.
pub const FWD_STATS: [(&str, &str); 3] =
    [("+30d", "+0.6%"), ("+90d", "+16.7%"), ("+180d", "+35.3%")];

const BUCKETS: [&str; 4] = [
    "within 10%",
    "10-20% below",
    "20-30% below",
    "over 30% below",
];

/// The resting deploy zones against spot: `(sym, % below spot, quote)` per priced order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ZonePicture {
    pub rows: Vec<(String, f64, f64)>,
    pub buckets: [f64; 4],
    pub total: f64,
}

/// Price every open deploy order with an order id against its pair's spot. A pair whose
/// spot cannot be read is skipped (and asked again for its next order).
pub fn zone_picture<F>(open_deploy: &[&Json], mut spot: F) -> Result<ZonePicture, String>
where
    F: FnMut(&str, &str) -> Result<f64, String>,
{
    let mut cache: Vec<(String, f64)> = Vec::new();
    let mut pic = ZonePicture::default();
    for o in open_deploy
        .iter()
        .filter(|o| o.get("order_id").is_some_and(Json::truthy))
    {
        let pair = o.get("pair").and_then(Json::as_str).unwrap_or_default();
        let px = match cache.iter().find(|(p, _)| p == pair) {
            Some((_, v)) => *v,
            None => {
                let exch = o.get("exch").and_then(Json::as_str).unwrap_or_default();
                match spot(exch, pair) {
                    Ok(v) => {
                        cache.push((pair.to_string(), v));
                        v
                    }
                    Err(_) => continue,
                }
            }
        };
        let price = o
            .get("price")
            .and_then(Json::to_float)
            .ok_or_else(|| format!("order {pair} has no price"))?;
        let quote = o.get("quote").and_then(Json::to_float).unwrap_or(0.0);
        let sym = o.get("sym").map(Json::py_str).unwrap_or_default();
        pic.rows.push((sym, (1.0 - price / px) * 100.0, quote));
    }
    pic.total = pysum(pic.rows.iter().map(|r| r.2));
    for (_, d, q) in &pic.rows {
        let k = if *d < 10.0 {
            0
        } else if *d < 20.0 {
            1
        } else if *d < 30.0 {
            2
        } else {
            3
        };
        pic.buckets[k] += q;
    }
    Ok(pic)
}

fn money0(reg_btc: Option<&Json>, key: &str) -> String {
    comma(
        reg_btc
            .and_then(|b| b.get(key))
            .and_then(Json::to_float)
            .unwrap_or(0.0),
        0,
    )
}

/// The external market-report verdict block. `verdict` is the parsed file, `None` when
/// it is missing or unreadable.
fn verdict_block(verdict: Option<&Json>, now: f64) -> Option<Vec<String>> {
    let v = verdict.filter(|v| v.is_obj())?;
    let epoch = match v.get("epoch") {
        None => 0.0,
        Some(e) => e.num()?,
    };
    let text = match v.get("verdict") {
        None => String::new(),
        Some(t) => t.as_str()?.to_string(),
    };
    let s = |k: &str| v.get(k).map(Json::py_str).unwrap_or_else(|| "None".into());
    let age = (now - epoch) / 86400.0;
    Some(vec![
        format!(
            "YOUR MARKET REPORT READ ({}, {}d old)",
            s("date"),
            fixed(age, 0)
        ),
        format!(
            "  Confidence bottom is in: {}/10   ({} of 31 signals green)",
            s("confidence"),
            s("green_signals")
        ),
        format!("  {}", wrap(&text, 76).join("\n  ")),
        "  This is the broader model. Where it disagrees with the trend label above,".into(),
        "  it is the one with valuation, on-chain, funding and sentiment in it.".into(),
        String::new(),
    ])
}

/// `(subject, text)` for a shift. `zone` is [`zone_picture`]'s result, `verdict` the
/// parsed market-report verdict file if there is one.
pub fn build_message(
    shift: &Shift,
    reg: &Json,
    zone: Result<ZonePicture, String>,
    verdict: Option<&Json>,
    now: f64,
    hints: &Hints,
) -> (String, String) {
    let lab = upper(&shift.label);
    let prev = upper(shift.prev_label.as_deref().unwrap_or("unknown"));
    let stage = upper(&shift.stage);
    let mut l: Vec<String> = vec![
        format!(
            "REGIME {stage}: {prev} -> {lab} (held {}d, confirms at {}d)",
            shift.held_days, shift.confirm_days
        ),
        String::new(),
    ];

    let btc = reg.get("btc");
    l.push("WHAT THE DATA SAYS".into());
    l.push(format!(
        "  BTC ${} vs SMA100 ${} / SMA200 ${}",
        money0(btc, "px"),
        money0(btc, "sma100"),
        money0(btc, "sma200")
    ));
    l.push(format!(
        "  breadth {} coins above their own 30d SMA",
        reg.get("breadth_above_sma30")
            .map(Json::py_str)
            .unwrap_or_else(|| "?".into())
    ));
    l.push(
        "  (that is the WHOLE model: BTC trend + breadth. No valuation, flows, on-chain, \
         funding or sentiment. One trend filter, not a cycle call.)"
            .into(),
    );
    l.push(String::new());
    l.push(
        "  per-coin RUN gate (4 signs: above 30d SMA, +25% 30d, fresh 30d high, higher lows)"
            .into(),
    );
    let mut coins: Vec<&(String, Json)> = reg
        .get("coins")
        .map(Json::entries)
        .unwrap_or_default()
        .iter()
        .collect();
    coins.sort_by(|a, b| a.0.cmp(&b.0));
    for (sym, c) in coins {
        if let Some(err) = c.get("error").filter(|e| e.truthy()) {
            let e: String = err.py_str().chars().take(60).collect();
            l.push(format!("    {} ERROR {e}", ljust(sym, 5)));
            continue;
        }
        let hits = c
            .get("signals")
            .map(Json::entries)
            .unwrap_or_default()
            .iter()
            .filter(|(_, v)| *v == Json::Bool(true))
            .count();
        let run = if c.get("running").is_some_and(Json::truthy) {
            "RUN "
        } else {
            "----"
        };
        l.push(format!("    {} {run} {hits}/4", ljust(sym, 5)));
    }
    l.push(String::new());

    match zone {
        Ok(z) => {
            l.push("WHAT IT MEANS FOR YOUR ORDERS".into());
            l.push(format!(
                "  {} resting deploy zones, ${}, priced below spot:",
                z.rows.len(),
                comma(z.total, 2)
            ));
            for (k, v) in BUCKETS.iter().zip(z.buckets) {
                let pct = if z.total != 0.0 {
                    v / z.total * 100.0
                } else {
                    0.0
                };
                l.push(format!(
                    "    {} ${}  {}%",
                    ljust(k, 15),
                    rjust(&comma(v, 2), 9),
                    rjust(&fixed(pct, 1), 5)
                ));
            }
            let mut deep: Vec<&(String, f64, f64)> =
                z.rows.iter().filter(|r| r.1 >= 20.0).collect();
            // Python's sort is stable; so is sort_by.
            deep.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            if !deep.is_empty() {
                l.push(String::new());
                l.push("  deepest rungs (least likely to fill in a climb):".into());
                for (s, d, q) in deep.into_iter().take(5) {
                    l.push(format!(
                        "    {} -{}%  ${}",
                        ljust(s, 5),
                        rjust(&fixed(*d, 1), 5),
                        rjust(&comma(*q, 2), 9)
                    ));
                }
            }
            l.push(String::new());
        }
        Err(e) => {
            l.push(format!("  (zone read failed: {e})"));
            l.push(String::new());
        }
    }

    if shift.label == "bull" {
        let join = |s: &[(&str, &str)]| {
            s.iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join("   ")
        };
        l.extend([
            "HISTORICAL BASE RATES (BTC, 16 confirmations since 2017 - thin sample)".to_string(),
            "  buying at market on the confirmation day returned, median:".into(),
            format!("    {}", join(&FWD_STATS)),
            "  a limit order below that price ever filled within 90d:".into(),
            format!("    {}", join(&FILL_STATS)),
            "  Read: the edge is real at 90-180d and absent at 30d. Deep rungs are the".into(),
            "  ones that go unfilled in exactly the scenario this label predicts.".into(),
            String::new(),
        ]);
    }

    match verdict_block(verdict, now) {
        Some(block) => l.extend(block),
        None => l.extend([
            "YOUR MARKET REPORT READ".to_string(),
            "  not available — the market report has not written its verdict file yet".into(),
            String::new(),
        ]),
    }

    let d = &hints.deploy_cmd;
    l.extend([
        "OPTIONS (nothing has been changed)".to_string(),
        format!("  inspect     {d} --status"),
        "  re-anchor   edit the deploy zone depths in the config, then".into(),
        format!("              {d} --tranche 0 <venue> [--only SYM]  (cancels those zones and"),
        "              re-ladders their budget at today's spot; a bare --cancel leaves the".into(),
        "              cash idle because total-vs-baseline does not grow)".into(),
        format!("  free cash   {d} --cancel <venue>   (cancels zones, frees the stable)"),
        "  hold        do nothing; the ladder keeps working the current rungs".into(),
        String::new(),
    ]);
    l.push(format!(
        "Label history: {}d reconstructed from daily closes.",
        shift.days_covered
    ));
    let subject = format!("REGIME {stage}: {prev} -> {lab} ({}d)", shift.held_days);
    (subject, l.join("\n"))
}

/// What a run of the regime watcher does.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Nothing unannounced: print `no unannounced regime shift.`
    Quiet,
    /// Announce this shift (or, on a dry run, print it).
    Announce(Shift),
}

/// `pending` is [`super::regime_state::pending_shift`]'s answer on the current history;
/// `--force` re-announces the current stage when nothing is pending.
pub fn plan(pending: Option<Shift>, history: &Json, force: bool) -> Plan {
    match pending {
        Some(s) => Plan::Announce(s),
        None if force => Plan::Announce(Shift::current(history)),
        None => Plan::Quiet,
    }
}

pub const QUIET_LINE: &str = "no unannounced regime shift.";
