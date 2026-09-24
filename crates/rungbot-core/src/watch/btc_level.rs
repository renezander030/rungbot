//! The BTC line: a heads-up as BTC nears a price you set, an alert when it breaks it.
//!
//! Two lines, both yours and both optional (`0` disables one): `warn_usd` above
//! `alert_usd`. A band fires only when BTC moves *into* a worse band than the one last
//! recorded, so hovering at a line is one mail, not one per run. The way back out has 2%
//! of hysteresis: the recorded band only relaxes once BTC is more than 2% above the line,
//! and that is what re-arms it.
//!
//! It never halts or cancels anything: a wick through the line is the discount the
//! resting buys were placed for, and only a confirmed break is a human's call. The mail
//! says how to halt by hand.

use super::json::{obj, Json};
use super::pyfmt::comma;
use super::{upper, Hints};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Levels {
    /// The line itself. `0` disables the alert band.
    pub alert_usd: f64,
    /// The heads-up line above it. `0` disables the warn band.
    pub warn_usd: f64,
}

/// What one check decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    /// The state to write (`None` on a dry run, or when BTC's price could not be read).
    pub state: Option<Json>,
    /// `(subject, text)` to mail.
    pub fire: Option<(String, String)>,
}

fn severity(band: &str) -> u8 {
    match band {
        "alert" => 2,
        "warn" => 1,
        _ => 0,
    }
}

/// Decide. `stored` is the previous state file (`None` when missing or unreadable);
/// `reg` the cached regime reading, whose `btc.px` is the price checked.
pub fn check(
    reg: &Json,
    stored: Option<&Json>,
    lv: Levels,
    dry_run: bool,
    now: f64,
    hints: &Hints,
) -> Check {
    let btc = reg.get("btc");
    let Some(px) = btc.and_then(|b| b.get("px")).and_then(Json::to_float) else {
        return Check {
            state: None,
            fire: None,
        };
    };
    // An unrecognised recorded band reads as "ok" rather than stopping the run.
    let mut band = stored
        .and_then(|s| s.get("band"))
        .and_then(Json::as_str)
        .filter(|b| matches!(*b, "ok" | "warn" | "alert"))
        .unwrap_or("ok");
    // De-escalate first, with 2% hysteresis, so a hover at the line cannot re-fire.
    if band == "alert" && lv.alert_usd != 0.0 && px > lv.alert_usd * 1.02 {
        band = "warn";
    }
    if band == "warn" && lv.warn_usd != 0.0 && px > lv.warn_usd * 1.02 {
        band = "ok";
    }
    let cur = if lv.alert_usd != 0.0 && px <= lv.alert_usd {
        "alert"
    } else if lv.warn_usd != 0.0 && px <= lv.warn_usd {
        "warn"
    } else {
        "ok"
    };
    let fire = severity(cur) > severity(band);
    let state = (!dry_run).then(|| {
        obj(vec![
            ("band", if fire { cur } else { band }.into()),
            ("px", px.into()),
            ("ts", Json::Int(now as i64)),
        ])
    });
    if !fire {
        return Check { state, fire: None };
    }
    let sma200 = btc
        .and_then(|b| b.get("sma200"))
        .map(Json::py_str)
        .unwrap_or_else(|| "?".into());
    let label = upper(
        &reg.get("market")
            .map(Json::py_str)
            .unwrap_or_else(|| "?".into()),
    );
    let (p, a, w) = (comma(px, 0), comma(lv.alert_usd, 0), comma(lv.warn_usd, 0));
    let msg = if cur == "alert" {
        (
            format!("\u{1F6A8} BTC ${p} — broke your ${a} flip level"),
            format!(
                "BTC is ${p}, at/below your ${a} watch level (your flip line).\n\n\
                 YOUR CALL — wick vs confirmed break:\n  \
                 WICK (brief spike down, reclaims fast): do NOTHING. Your resting deploy \
                 limit-buys are meant to fill here — this is the discount you planned to buy.\n  \
                 CONFIRMED (daily close below and holds; your thesis flips bearish): halt by \
                 hand —\n    touch {halt}\n    {deploy} --cancel\n\n\
                 Regime {label}, BTC SMA200 ${sma200}. Fires once per crossing; re-arms after \
                 BTC recovers >2% above ${a}.",
                halt = hints.halt_file,
                deploy = hints.deploy_cmd,
            ),
        )
    } else {
        (
            format!("BTC ${p} — approaching your ${a} level"),
            format!(
                "BTC is ${p}, under your ${w} heads-up line and heading toward the ${a} flip \
                 level.\n\nNo action needed — your deploy ladder is resting and buys the dips \
                 as planned. Next email fires only if BTC reaches ${a}.\n\n\
                 Regime {label}, BTC SMA200 ${sma200}."
            ),
        )
    };
    Check {
        state,
        fire: Some(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(px: f64) -> Json {
        obj(vec![
            ("market", "chop".into()),
            ("btc", obj(vec![("px", px.into())])),
        ])
    }

    const LV: Levels = Levels {
        alert_usd: 30_000.0,
        warn_usd: 33_000.0,
    };

    #[test]
    fn once_per_crossing_and_rearmed_by_a_recovery() {
        let h = Hints::default();
        let c = check(&reg(32_000.0), None, LV, false, 0.0, &h);
        assert!(c.fire.as_ref().unwrap().0.contains("approaching"));
        let st = c.state.unwrap();
        assert!(check(&reg(32_500.0), Some(&st), LV, false, 0.0, &h)
            .fire
            .is_none());
        let back = check(&reg(34_000.0), Some(&st), LV, false, 0.0, &h);
        assert!(back.fire.is_none());
        let st = back.state.unwrap();
        assert_eq!(st.get("band").and_then(Json::as_str), Some("ok"));
        assert!(check(&reg(32_900.0), Some(&st), LV, false, 0.0, &h)
            .fire
            .is_some());
    }

    #[test]
    fn a_dry_run_writes_nothing_and_no_price_decides_nothing() {
        let h = Hints::default();
        assert!(check(&reg(1.0), None, LV, true, 0.0, &h).state.is_none());
        let none = check(&Json::obj(), None, LV, false, 0.0, &h);
        assert_eq!((none.state, none.fire), (None, None));
    }
}
