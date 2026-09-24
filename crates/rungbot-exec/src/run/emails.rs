//! The four mails a run sends: signals, housekeeping, errors and the BTC level alert.
//! Subjects and bodies are fixed formats; the golden test holds them byte for byte.

use super::signals::{Row, Signal};
use super::RunResult;
use crate::pyfmt;

/// `f"{x:,.{prec}f}"`: fixed decimals with thousands separators.
pub fn comma(x: f64, prec: usize) -> String {
    let s = format!("{:.prec$}", x.abs());
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f.to_string())),
        None => (s.clone(), None),
    };
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

/// A price for a person: 6 decimals under 1, 4 above, trailing zeros dropped.
pub fn fmt_price(v: Option<f64>) -> String {
    let Some(v) = v else {
        return "n/a".into();
    };
    let s = if v < 1.0 { comma(v, 6) } else { comma(v, 4) };
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn pct1(v: Option<f64>) -> String {
    v.map_or("n/a".into(), |x| format!("{x:+.1}%"))
}

/// What the signal mail needs besides the signals.
#[derive(Debug, Clone)]
pub struct MailCtx<'a> {
    pub trade_mode: &'a str,
    pub regime_note: &'a str,
    pub first_pct: f64,
    pub step_pct: f64,
    pub target_pct: f64,
    pub min_core_pct: f64,
    /// The per-coin base bag in dollars, when a bag is set.
    pub base_usd: Option<f64>,
    /// `YYYY-MM-DD HH:MM TZ`.
    pub when: &'a str,
}

/// `2025-09-16 05:20 UTC`.
pub fn when(now: f64) -> String {
    let (y, m, d, h, mi, _) = rungbot_core::time::civil(now);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02} UTC")
}

pub fn mode_banner(mode: &str) -> &'static str {
    match mode {
        "dry" => "DRY-RUN auto-trade - these are the orders it WOULD place. None placed.",
        "live" => "LIVE auto-trade - market orders were placed (see EXECUTION).",
        _ => "NOTIFICATION ONLY - react manually. No orders placed.",
    }
}

fn buy_size_text(r: &Signal, c: &MailCtx) -> String {
    let pct = r.pct;
    let more = if r.new_rungs.contains(&1) {
        ""
    } else {
        " more"
    };
    let cap = r.cap_pct.unwrap_or(0.0);
    let (amt, room) = match c.base_usd {
        Some(base) => (
            format!(" = ${}", comma(pct / 100.0 * base, 0)),
            format!(
                "bag ${}, now ${} committed",
                comma(cap / 100.0 * base, 0),
                comma(r.ledger_pct / 100.0 * base, 0)
            ),
        ),
        None => (
            String::new(),
            format!("bag {cap:.0}% of base, {:.0}% committed", r.ledger_pct),
        ),
    };
    let flag = if r.capped { " [BAG FULL]" } else { "" };
    format!(
        "buy ~{pct:.0}%{more} of {}'s bag{amt} ({room}){flag}",
        r.sym
    )
}

fn sell_size_text(r: &Signal, c: &MailCtx) -> String {
    let pct = r.pct;
    if let Some(p) = r.policy.as_deref().filter(|p| !p.is_empty()) {
        return format!("sell ~{pct:.0}% of the position -- bull policy: {p}");
    }
    let more = if r.new_rungs.contains(&1) {
        ""
    } else {
        " more"
    };
    let flag = if r.capped { " [CORE FLOOR]" } else { "" };
    format!(
        "sell ~{pct:.0}%{more} of the position ({:.0}% sold so far, keep >= {:.0}% core){flag}",
        r.ledger_pct, c.min_core_pct
    )
}

/// The text tag and message of an execution line: plan, done, skip, warn, err.
fn exec_text(res: &RunResult) -> (&'static str, String) {
    let tag = if res.get("plan").is_some() {
        "PLAN "
    } else if res.get("done").is_some() {
        "DONE "
    } else if res.get("skip").is_some() {
        "SKIP "
    } else if res.get("warn").is_some() {
        "WARN "
    } else {
        "ERR  "
    };
    (tag, res.message())
}

fn sym_prefix(res: &RunResult) -> String {
    match res.sym.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}: "),
        None => String::new(),
    }
}

/// The signal mail: `(subject, text, html)`. The caller appends the dedupe suffix.
pub fn build_signals_email(
    buys: &[&Signal],
    sells: &[&Signal],
    rows: &[Row],
    execution: &[RunResult],
    c: &MailCtx,
) -> (String, String, String) {
    let subject = if !buys.is_empty() && !sells.is_empty() {
        format!(
            "SIGNAL: {} dip-buy + {} take-profit",
            buys.len(),
            sells.len()
        )
    } else if let Some(top) = buys.first() {
        format!(
            "DIP-BUY {} {:+.1}% 24h (rung {})",
            top.sym,
            top.chg.unwrap_or(0.0),
            top.rung
        )
    } else {
        let top = sells[0];
        format!(
            "TAKE-PROFIT {} {:+.1}% vs entry (rung {})",
            top.sym,
            top.pnl.unwrap_or(0.0),
            top.rung
        )
    };

    let banner = mode_banner(c.trade_mode);
    let mut lines: Vec<String> = vec![banner.to_string()];
    if !c.regime_note.is_empty() {
        lines.push(c.regime_note.to_string());
    }
    lines.push(String::new());
    if !execution.is_empty() {
        lines.push("== EXECUTION ==".into());
        for res in execution {
            let (tag, msg) = exec_text(res);
            lines.push(format!("  {tag}{}{msg}", sym_prefix(res)));
        }
        lines.push(String::new());
    }
    if !buys.is_empty() {
        lines.push(format!(
            "== DIP-BUY ({}) :: 24h <= -{:.0}%, then -{:.0}% steps ==",
            buys.len(),
            c.first_pct,
            c.step_pct
        ));
        for r in buys {
            let sell_lvl = r.entry.map(|e| e * (1.0 + c.target_pct / 100.0));
            lines.push(format!(
                "  {:5} {:+6.1}% 24h  ${}  [rung {} @ -{:.0}% / 24h]",
                r.sym,
                r.chg.unwrap_or(0.0),
                fmt_price(Some(r.usd)),
                r.rung,
                r.threshold
            ));
            lines.push(format!("        action: {}", buy_size_text(r, c)));
            lines.push(match r.pnl {
                Some(pnl) => format!(
                    "        monitored sell starts at entry +{:.0}% = ${}  |  entry ${}, P&L {pnl:+.1}%",
                    c.target_pct,
                    fmt_price(sell_lvl),
                    fmt_price(r.entry)
                ),
                None => "        no entry on file".into(),
            });
        }
        lines.push(String::new());
    }
    if !sells.is_empty() {
        lines.push(format!(
            "== TAKE-PROFIT ({}) :: price >= entry +{:.0}%, then +{:.0}% steps ==",
            sells.len(),
            c.first_pct,
            c.step_pct
        ));
        for r in sells {
            lines.push(format!(
                "  {:5} {:+6.1}% vs entry  ${}  [rung {} @ entry +{:.0}%]",
                r.sym,
                r.pnl.unwrap_or(0.0),
                fmt_price(Some(r.usd)),
                r.rung,
                r.threshold
            ));
            lines.push(format!("        action: {}", sell_size_text(r, c)));
            lines.push(match r.chg {
                Some(chg) => format!("        entry ${}  |  24h {chg:+.1}%", fmt_price(r.entry)),
                None => format!("        entry ${}", fmt_price(r.entry)),
            });
        }
        lines.push(String::new());
    }
    lines.push("Full watchlist (coin | 24h | price | entry | +10% target | P&L):".into());
    for r in rows {
        let mut tail = String::new();
        if r.committed_pct > 0.0 {
            tail = format!("  committed {:.0}%", r.committed_pct);
            if r.over_budget {
                tail.push_str(" OVER-BUDGET");
            }
        }
        if !r.win_dir.is_empty() {
            tail.push_str(&format!(
                "  [24h {}-window: opposite side frozen]",
                r.win_dir
            ));
        }
        if r.breaker {
            tail.push_str("  [BREAKER: buys frozen]");
        }
        if r.run {
            tail.push_str("  [RUN: trailing]");
        }
        lines.push(format!(
            "  {:5} {:>8}  ${}  entry ${}  tgt ${}  P&L {}{tail}",
            r.sym,
            pct1(r.chg),
            fmt_price(Some(r.usd)),
            fmt_price(r.entry),
            fmt_price(r.target),
            pct1(r.pnl)
        ));
    }
    lines.push(String::new());
    lines.push(format!(
        "Checked: {}  |  venue price + 24h change (Binance/Gate); sells vs entry",
        c.when
    ));
    let text = lines.join("\n");

    // ---- HTML
    let sig_rows = |items: &[&Signal], buy: bool| -> String {
        let mut out = String::new();
        for r in items {
            let (color, metric, thr, act) = if buy {
                let sell_lvl = r.entry.map(|e| e * (1.0 + c.target_pct / 100.0));
                (
                    "#d73027",
                    format!("{:+.1}% 24h", r.chg.unwrap_or(0.0)),
                    format!("rung {} @ -{:.0}% / 24h", r.rung, r.threshold),
                    format!(
                        "{}; sell starts entry +{:.0}% = ${}",
                        buy_size_text(r, c),
                        c.target_pct,
                        fmt_price(sell_lvl)
                    ),
                )
            } else {
                (
                    "#1a9850",
                    format!("{:+.1}% vs entry", r.pnl.unwrap_or(0.0)),
                    format!("rung {} @ entry +{:.0}%", r.rung, r.threshold),
                    sell_size_text(r, c),
                )
            };
            out.push_str(&format!(
                "<tr><td style='padding:6px 10px;font-weight:600'>{}</td>\
                 <td style='padding:6px 10px;color:{color};font-weight:600'>{metric}</td>\
                 <td style='padding:6px 10px'>${}</td>\
                 <td style='padding:6px 10px;color:#666'>{thr}</td>\
                 <td style='padding:6px 10px'>{act}</td>\
                 <td style='padding:6px 10px;color:#666'>P&L {}</td></tr>",
                r.sym,
                fmt_price(Some(r.usd)),
                pct1(r.pnl)
            ));
        }
        out
    };
    let mut sections = String::new();
    if !buys.is_empty() {
        sections.push_str(&format!(
            "<h3 style='margin:18px 0 4px;color:#d73027'>Dip-buy ({}) \
             &mdash; 24h &le; -{:.0}%, then -{:.0}% steps</h3>\
             <table style='border-collapse:collapse;width:100%;font-size:14px'><tbody>{}</tbody></table>",
            buys.len(),
            c.first_pct,
            c.step_pct,
            sig_rows(buys, true)
        ));
    }
    if !sells.is_empty() {
        sections.push_str(&format!(
            "<h3 style='margin:18px 0 4px;color:#1a9850'>Take-profit ({}) \
             &mdash; price &ge; entry +{:.0}%, then +{:.0}% steps</h3>\
             <table style='border-collapse:collapse;width:100%;font-size:14px'><tbody>{}</tbody></table>",
            sells.len(),
            c.first_pct,
            c.step_pct,
            sig_rows(sells, false)
        ));
    }
    let mut full = String::new();
    for r in rows {
        let mut pnl = pct1(r.pnl);
        if r.committed_pct > 0.0 {
            let mark = if r.over_budget {
                "OVER-BUDGET".to_string()
            } else {
                format!("committed {:.0}%", r.committed_pct)
            };
            pnl.push_str(&format!(" <span style='color:#888'>({mark})</span>"));
        }
        let chg_color = if r.chg.unwrap_or(0.0) >= 0.0 {
            "#1a9850"
        } else {
            "#d73027"
        };
        full.push_str(&format!(
            "<tr><td style='padding:5px 10px;font-weight:600'>{}</td>\
             <td style='padding:5px 10px;color:{chg_color}'>{}</td>\
             <td style='padding:5px 10px'>${}</td>\
             <td style='padding:5px 10px;color:#666'>${}</td>\
             <td style='padding:5px 10px;color:#666'>${}</td>\
             <td style='padding:5px 10px'>{pnl}</td></tr>",
            r.sym,
            pct1(r.chg),
            fmt_price(Some(r.usd)),
            fmt_price(r.entry),
            fmt_price(r.target)
        ));
    }
    let mut exec_html = String::new();
    if !execution.is_empty() {
        let mut rows_html = String::new();
        for res in execution {
            let (color, tag) = if res.get("done").is_some() {
                ("#1a9850", "DONE")
            } else if res.get("err").is_some() {
                ("#d73027", "ERR")
            } else if res.get("warn").is_some() {
                ("#f0a030", "WARN")
            } else if res.get("skip").is_some() {
                ("#999", "SKIP")
            } else {
                ("#0b66c3", "PLAN")
            };
            rows_html.push_str(&format!(
                "<tr><td style='padding:4px 10px;color:{color};font-weight:600'>{tag}</td>\
                 <td style='padding:4px 10px;font-weight:600'>{}</td>\
                 <td style='padding:4px 10px'>{}</td></tr>",
                res.sym.as_deref().unwrap_or(""),
                res.message()
            ));
        }
        exec_html = format!(
            "<h3 style='margin:18px 0 4px'>Execution</h3>\
             <table style='border-collapse:collapse;width:100%;font-size:13px'><tbody>{rows_html}</tbody></table>"
        );
    }
    let note = if c.regime_note.is_empty() {
        String::new()
    } else {
        format!("<br>{}", c.regime_note)
    };
    let html = format!(
        "<div style='font-family:system-ui,sans-serif;max-width:680px'>\
         <p style='margin:0 0 4px;padding:6px 10px;background:#fff7e6;border-left:3px solid #f0a030;\
         font-size:13px;color:#7a5a10'>{banner}{note}</p>\
         {exec_html}{sections}\
         <h3 style='margin:18px 0 4px'>Full watchlist</h3>\
         <table style='border-collapse:collapse;width:100%;font-size:13px'>\
         <thead><tr style='border-bottom:2px solid #ddd;text-align:left;color:#888'>\
         <th style='padding:5px 10px'>Coin</th><th style='padding:5px 10px'>24h</th>\
         <th style='padding:5px 10px'>Price</th><th style='padding:5px 10px'>Entry</th>\
         <th style='padding:5px 10px'>+10% tgt</th><th style='padding:5px 10px'>P&L</th></tr></thead>\
         <tbody>{full}</tbody></table>\
         <p style='margin:16px 0 0;color:#888;font-size:12px'>Checked {} | \
         venue price + 24h change (Binance/Gate); sells measured vs entry</p>\
         </div>",
        c.when
    );
    (subject, text, html)
}

/// The quiet-run mail: no new rung, but housekeeping did something worth knowing.
pub fn build_housekeeping_email(noteworthy: &[&RunResult], when: &str) -> (String, String) {
    let fills = noteworthy
        .iter()
        .filter(|r| r.get("done").is_some())
        .count();
    let warns = noteworthy
        .iter()
        .filter(|r| r.get("warn").is_some() || r.get("err").is_some())
        .count();
    let parts: Vec<String> = [
        (fills > 0).then(|| format!("{fills} update(s)")),
        (warns > 0).then(|| format!("{warns} warning(s)")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let subject = format!("POSITIONS: {}", parts.join(", "));
    let mut lines = vec![
        "Housekeeping on a quiet run (no new ladder rung):".to_string(),
        String::new(),
    ];
    for res in noteworthy {
        let (tag, msg) = if let Some(d) = res.get("done") {
            ("DONE ", d)
        } else if let Some(w) = res.get("warn") {
            ("WARN ", w)
        } else {
            ("ERR  ", res.get("err").unwrap_or(""))
        };
        lines.push(format!("  {tag}{}{msg}", sym_prefix(res)));
    }
    lines.push(String::new());
    lines.push(format!("Checked: {when}"));
    (subject, lines.join("\n"))
}

/// The error mail.
pub fn build_error_email(
    errors: &[String],
    when: &str,
    name: &str,
    log_hint: &str,
) -> (String, String) {
    let subject = format!("{name} ERROR ({})", errors.len());
    let list: Vec<String> = errors.iter().map(|e| format!("  - {e}")).collect();
    let text = format!(
        "{name} hit error(s):\n\n{}\n\nChecked: {when}\nLog: {log_hint}",
        list.join("\n")
    );
    (subject, text)
}

/// What the level alert prints for the regime's SMA200: the number as the cache holds
/// it, or `?`.
pub fn value_text(v: Option<&serde_json::Value>) -> String {
    match v {
        None => "?".into(),
        Some(serde_json::Value::Number(n)) => match n.as_i64() {
            Some(i) if !n.is_f64() => i.to_string(),
            _ => pyfmt::float_repr(n.as_f64().unwrap_or(0.0)),
        },
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) => "None".into(),
        Some(serde_json::Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(other) => other.to_string(),
    }
}

/// The level alert's `(subject, text)`: `alert` broke the line, `warn` approaches it.
#[allow(clippy::too_many_arguments)]
pub fn build_btc_email(
    band: &str,
    px: f64,
    alert_usd: f64,
    warn_usd: f64,
    label: &str,
    sma200: &str,
    line_name: &str,
    halt_file: &str,
) -> (String, String) {
    if band == "alert" {
        let subject = format!(
            "\u{1F6A8} BTC ${} — broke your ${} flip level",
            comma(px, 0),
            comma(alert_usd, 0)
        );
        let text = format!(
            "BTC is ${}, at/below your ${} watch level ({line_name}).\n\n\
             YOUR CALL — wick vs confirmed break:\n  \
             WICK (brief spike down, reclaims fast): do NOTHING. Your resting deploy \
             limit-buys are meant to fill here — this is the discount you planned to buy.\n  \
             CONFIRMED (daily close below and holds; the thesis flips bearish): halt \
             by hand —\n    \
             touch {halt_file}\n    \
             rungbot-exec deploy --cancel\n\n\
             Regime {label}, BTC SMA200 ${sma200}. Fires once per crossing; re-arms \
             after BTC recovers >2% above ${}.",
            comma(px, 0),
            comma(alert_usd, 0),
            comma(alert_usd, 0)
        );
        (subject, text)
    } else {
        let subject = format!(
            "BTC ${} — approaching your ${} level",
            comma(px, 0),
            comma(alert_usd, 0)
        );
        let text = format!(
            "BTC is ${}, under your ${} heads-up line and heading toward the ${} flip level.\n\n\
             No action needed — your deploy ladder is resting and buys the dips as \
             planned. Next email fires only if BTC reaches ${}.\n\n\
             Regime {label}, BTC SMA200 ${sma200}.",
            comma(px, 0),
            comma(warn_usd, 0),
            comma(alert_usd, 0),
            comma(alert_usd, 0)
        );
        (subject, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_print_as_the_reference_printed_them() {
        assert_eq!(fmt_price(Some(0.0012345)), "0.001234");
        assert_eq!(fmt_price(Some(1234.5)), "1,234.5");
        assert_eq!(fmt_price(Some(2.0)), "2");
        assert_eq!(fmt_price(None), "n/a");
        assert_eq!(comma(1234567.891, 0), "1,234,568");
        assert_eq!(comma(-1234.5, 2), "-1,234.50");
        assert_eq!(comma(999.9996, 3), "1,000.000");
    }

    #[test]
    fn the_error_mail_lists_every_error() {
        let (s, t) = build_error_email(&["a".into(), "b".into()], "W", "rungbot", "L");
        assert_eq!(s, "rungbot ERROR (2)");
        assert_eq!(
            t,
            "rungbot hit error(s):\n\n  - a\n  - b\n\nChecked: W\nLog: L"
        );
    }
}
