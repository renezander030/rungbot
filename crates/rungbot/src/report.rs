//! Human-readable output. JSON is the other half and lives in `main`.
//!
//! Sizes are percentages on purpose. A buy is a % of that coin's base share of your dry
//! powder; a sell is a % of the position you hold. rungbot does not know your balances
//! and never asks for them, so it cannot print dollar amounts and does not pretend to.

use rungbot_core::{decisions::Kind, Config, Decision, Outcome, Regime, Row, Trade};

pub fn fmt_price(v: Option<f64>) -> String {
    let Some(v) = v else { return "-".into() };
    if v >= 1000.0 {
        let s = format!("{v:.0}");
        return group_thousands(&s);
    }
    if v >= 1.0 {
        return group_thousands(&format!("{v:.2}"));
    }
    if v >= 0.01 {
        return format!("{v:.4}");
    }
    format!("{v:.6}")
}

fn group_thousands(s: &str) -> String {
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    match frac {
        Some(f) => format!("{out}.{f}"),
        None => out,
    }
}

fn pct(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:+.1}%"),
        None => "-".into(),
    }
}

fn flags(r: &Row) -> String {
    let mut out: Vec<&str> = Vec::new();
    if r.breaker {
        out.push("BREAKER");
    }
    let lock;
    if !r.win_dir.is_empty() {
        lock = format!("{}-locked", r.win_dir);
        out.push(&lock);
    }
    if r.over_budget {
        out.push("over-budget");
    }
    if r.trailing {
        out.push("trail");
    }
    out.join(" ")
}

fn buy_line(t: &Trade) -> String {
    let capped = if t.capped { " (capped)" } else { "" };
    format!(
        "  {:<6} {:>8} 24h  ->  rung {} (-{}%)  buy {:.0}% of base{capped}   @ {}",
        t.row.sym,
        pct(t.row.chg),
        t.rung,
        trim_num(t.threshold),
        t.pct,
        fmt_price(Some(t.row.price))
    )
}

fn sell_line(t: &Trade) -> String {
    let capped = if t.capped { " (capped by core)" } else { "" };
    format!(
        "  {:<6} {:>8} P&L  ->  rung {} (+{}%)  sell {:.0}% of position{capped}   @ {}",
        t.row.sym,
        pct(t.row.pnl),
        t.rung,
        trim_num(t.threshold),
        t.pct,
        fmt_price(Some(t.row.price))
    )
}

/// `%g`-style: 10 not 10.0, but 12.5 stays 12.5.
fn trim_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{:.0}", v)
    } else {
        format!("{v}")
    }
}

pub fn render(
    out: &Outcome,
    cfg: &Config,
    regime: Option<&Regime>,
    log: &[Decision],
    now_iso: &str,
) -> String {
    let s = &cfg.settings;
    let mut l: Vec<String> = Vec::new();
    l.push(format!("rungbot plan — {now_iso}"));
    l.push(format!(
        "bands {}/{} · core {}% · window {}h · knife floor -{}% · trail {}",
        trim_num(s.bands.first_pct),
        trim_num(s.bands.step_pct),
        trim_num(s.min_core_pct),
        trim_num(s.window_hours),
        trim_num(s.buy_floor_pct),
        if s.trail == rungbot_core::Trail::On {
            "on"
        } else {
            "off"
        },
    ));
    if let Some(r) = regime {
        l.push(format!(
            "market {} · breadth {}/{} above 30d SMA · running: {}",
            r.market.as_str(),
            r.breadth_above_sma30,
            r.coins.len(),
            if r.running_syms().is_empty() {
                "none".to_string()
            } else {
                r.running_syms().join(", ")
            }
        ));
    }
    l.push(String::new());

    if out.buys.is_empty() {
        l.push("BUY — nothing crossed a new dip rung".into());
    } else {
        l.push(format!(
            "BUY — {} coin(s) crossed a new dip rung",
            out.buys.len()
        ));
        l.extend(out.buys.iter().map(buy_line));
    }
    l.push(String::new());

    if out.sells.is_empty() {
        l.push("SELL — nothing crossed a new profit rung".into());
    } else {
        l.push(format!(
            "SELL — {} coin(s) crossed a new profit rung",
            out.sells.len()
        ));
        l.extend(out.sells.iter().map(sell_line));
    }
    l.push(String::new());

    l.push(format!(
        "{:<7}{:>12}{:>9}{:>12}{:>9}{:>12}{:>7}  FLAGS",
        "COIN", "PRICE", "24H", "ENTRY", "P&L", "TARGET", "USED"
    ));
    for r in &out.rows {
        let line = format!(
            "{:<7}{:>12}{:>9}{:>12}{:>9}{:>12}{:>6.0}%  {}",
            r.sym,
            fmt_price(Some(r.price)),
            pct(r.chg),
            fmt_price(r.entry),
            pct(r.pnl),
            fmt_price(r.target),
            r.committed_pct,
            flags(r)
        );
        l.push(line.trim_end().to_string());
    }

    let holds: Vec<&Decision> = log.iter().filter(|d| d.kind == Kind::Hold).collect();
    if !holds.is_empty() {
        l.push(String::new());
        l.push("WHY NOTHING HAPPENED".into());
        for d in holds {
            l.push(format!("  {:<7} {}", d.sym, d.detail));
        }
    }

    for r in &out.rows {
        if let Some(p) = &r.policy {
            l.push(format!("  {p}"));
        }
    }

    if !out.errors.is_empty() {
        l.push(String::new());
        l.push("NOTES".into());
        l.extend(out.errors.iter().map(|e| format!("  ! {e}")));
    }

    l.push(String::new());
    l.push("Notify-only. rungbot holds no keys and places no orders.".into());
    l.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_are_grouped_and_scaled_by_magnitude() {
        assert_eq!(fmt_price(Some(80878.0)), "80,878");
        assert_eq!(fmt_price(Some(2606.4)), "2,606");
        assert_eq!(fmt_price(Some(108.5)), "108.50");
        assert_eq!(fmt_price(Some(0.044)), "0.0440");
        assert_eq!(fmt_price(Some(0.00123)), "0.001230");
        assert_eq!(fmt_price(None), "-");
    }

    #[test]
    fn percentages_always_carry_a_sign() {
        assert_eq!(pct(Some(12.34)), "+12.3%");
        assert_eq!(pct(Some(-1.0)), "-1.0%");
        assert_eq!(pct(None), "-");
    }
}
