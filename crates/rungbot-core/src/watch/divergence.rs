//! Divergence check: is the live book still earning the edge the backtest promised?
//!
//! The steering signal is not the calendar but whether live trading earns what the last
//! backtest expected. Two guards, both read-only:
//!
//! * **Edge lost** (relative): realized alpha since a baseline, the live book against a
//!   buy-and-hold of the baseline book, both valued at today's prices, falls more than
//!   `drift_band_pct` under the backtest's alpha prorated to the elapsed days.
//! * **Drawdown beyond backtest** (absolute): the live book's return since the baseline
//!   falls more than `floor_margin_pct` below the worst window the backtest ever showed.
//!
//! The baseline is re-taken whenever a new backtest expectation appears, so the
//! comparison stays out of sample. Nothing is judged before `min_days`.
//!
//! One change from the original: it mailed every daily run while a guard held. Here each
//! guard mails once per baseline, and re-arms when it clears. The guards already mailed
//! are kept in the state file as `alerted`.

use super::json::{obj, Json};
use super::pyfmt::{comma, fixed, signed, sum as pysum};
use super::{upper, Hints};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DivergenceConfig {
    /// Relative band: alpha this many points under the expectation trips "edge lost".
    pub drift_band_pct: f64,
    /// Absolute margin under the backtest's worst window that trips "drawdown".
    pub floor_margin_pct: f64,
    /// Days after the baseline before anything is judged.
    pub min_days: f64,
}

impl Default for DivergenceConfig {
    fn default() -> Self {
        DivergenceConfig {
            drift_band_pct: 5.0,
            floor_margin_pct: 3.0,
            min_days: 7.0,
        }
    }
}

pub const NO_EXPECTATION: &str =
    "No backtest-expectation.json yet -- monthly backtest hasn't run. Skipping.";

/// The live book: units held per coin (every venue, free + locked) and total stable.
#[derive(Debug, Clone, PartialEq)]
pub struct Book {
    pub held: Json,
    pub stable: f64,
}

pub struct Input<'a> {
    pub now: f64,
    pub expect: &'a Json,
    /// The previous state, `None` when there is none.
    pub state: Option<&'a Json>,
    pub book: Result<Book, String>,
    /// `{symbol: usd}` at today's prices.
    pub prices: Result<Json, String>,
    /// The cached regime label, `unknown` when unreadable.
    pub regime: String,
    pub dry_run: bool,
    pub cfg: DivergenceConfig,
    pub hints: &'a Hints,
}

/// What a run decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Finished: print, write `state` if any, exit with `code`.
    Done {
        code: i32,
        stdout: Vec<String>,
        stderr: Vec<String>,
        state: Option<Json>,
    },
    /// Print `stdout`, then mail. On delivery print `Sent divergence alert.` and write
    /// `state`; on failure print `Failed to send alert email.` to stderr and exit 1.
    Send {
        stdout: Vec<String>,
        subject: String,
        body: String,
        state: Json,
    },
}

fn done(code: i32, stdout: Vec<String>, stderr: Vec<String>, state: Option<Json>) -> Plan {
    Plan::Done {
        code,
        stdout,
        stderr,
        state,
    }
}

/// `sum(held.get(s, 0.0) * px[s] for s in px) + stable`.
pub fn value(held: &Json, stable: f64, px: &Json) -> f64 {
    let terms = px.entries().iter().map(|(s, p)| {
        held.get(s).and_then(Json::to_float).unwrap_or(0.0) * p.to_float().unwrap_or(0.0)
    });
    pysum(terms) + stable
}

fn floor_text(floor: Option<f64>) -> String {
    match floor {
        None => "n/a".into(),
        Some(f) => format!("{}%", signed(f, 2)),
    }
}

/// Decide a run once the expectation file exists and has been parsed.
pub fn plan(inp: Input<'_>) -> Plan {
    let (book, px) = match (inp.book, inp.prices) {
        (Ok(b), Ok(p)) => (b, p),
        (Err(e), _) | (_, Err(e)) => {
            return done(
                1,
                vec![],
                vec![format!("Live book/price fetch failed: {e}")],
                None,
            )
        }
    };
    let Some(expect_epoch) = inp.expect.get("epoch") else {
        return done(
            1,
            vec![],
            vec!["backtest expectation has no `epoch`".into()],
            None,
        );
    };
    let rebase = match inp.state {
        None => true,
        Some(st) => {
            !st.get("expect_epoch")
                .unwrap_or(&Json::Null)
                .py_eq(expect_epoch)
                || !st.contains_key("px0")
        }
    };
    if rebase {
        let st = obj(vec![
            ("epoch", Json::Int(inp.now as i64)),
            ("expect_epoch", expect_epoch.clone()),
            ("held0", book.held.clone()),
            ("stable0", book.stable.into()),
            ("px0", px.clone()),
        ]);
        return done(
            0,
            vec![format!(
                "Baselined divergence state to current book (expect_epoch={}).",
                expect_epoch.py_str()
            )],
            vec![],
            Some(st),
        );
    }
    let st = inp.state.expect("rebase covers a missing state");
    let base_epoch = st.get("epoch").and_then(Json::to_float).unwrap_or(0.0);
    let elapsed = (inp.now - base_epoch) / 86400.0;
    let cfg = inp.cfg;
    if elapsed < cfg.min_days {
        return done(
            0,
            vec![format!(
                "Only {}d since baseline (< {}d) -- too early to judge.",
                fixed(elapsed, 1),
                fixed(cfg.min_days, 0)
            )],
            vec![],
            None,
        );
    }
    let empty = Json::obj();
    let held0 = st.get("held0").unwrap_or(&empty);
    let stable0 = st.get("stable0").and_then(Json::to_float).unwrap_or(0.0);
    let px0 = st.get("px0").unwrap_or(&empty);
    let bh_now = value(held0, stable0, &px);
    let strat_now = value(&book.held, book.stable, &px);
    let base_val = value(held0, stable0, px0);
    if bh_now <= 0.0 || base_val <= 0.0 {
        return done(
            1,
            vec![],
            vec!["Baseline book value is zero -- cannot compute returns.".into()],
            None,
        );
    }

    let realized_alpha = (strat_now / bh_now - 1.0) * 100.0;
    let full_alpha = inp
        .expect
        .get("alpha_pct")
        .and_then(Json::to_float)
        .unwrap_or(0.0);
    let anchor = [inp.expect.get("anchor_days"), inp.expect.get("window_days")]
        .into_iter()
        .flatten()
        .find(|v| v.truthy())
        .cloned()
        .unwrap_or(Json::Int(28));
    let anchor_days = anchor.to_float().unwrap_or(28.0);
    let ratio = elapsed / anchor_days;
    let frac = if ratio < 1.0 { ratio } else { 1.0 };
    let expected = full_alpha * frac;
    let drift = realized_alpha - expected;

    let realized_return = (strat_now / base_val - 1.0) * 100.0;
    let floor = inp
        .expect
        .get("strat_return_floor_pct")
        .filter(|f| !f.is_null())
        .and_then(Json::to_float);
    let floor_breach = floor.is_some_and(|f| realized_return < f - cfg.floor_margin_pct);

    let line = format!(
        "realized alpha {}% vs expected {}% ({}% {}d prorated x{}, drift {}pp) | \
         realized return {}% vs floor {} over {}d | regime {}",
        signed(realized_alpha, 2),
        signed(expected, 2),
        signed(full_alpha, 1),
        anchor.py_str(),
        fixed(frac, 2),
        signed(drift, 2),
        signed(realized_return, 2),
        floor_text(floor),
        fixed(elapsed, 0),
        inp.regime
    );
    let mut stdout = vec![line.clone()];

    let mut alarms: Vec<(&str, String)> = Vec::new();
    if drift <= -cfg.drift_band_pct {
        alarms.push((
            "edge_lost",
            format!(
                "EDGE LOST: alpha {}% is {}pp under the backtested {}% (band {}pp).",
                signed(realized_alpha, 2),
                fixed(-drift, 1),
                signed(expected, 2),
                fixed(cfg.drift_band_pct, 0)
            ),
        ));
    }
    if floor_breach {
        alarms.push((
            "drawdown",
            format!(
                "DRAWDOWN BEYOND BACKTEST: live return {}% is below the worst backtested window \
                 ({}%) by more than {}pp. This is the surprise the floor guard exists to catch \
                 early.",
                signed(realized_return, 2),
                signed(floor.unwrap_or(0.0), 2),
                fixed(cfg.floor_margin_pct, 0)
            ),
        ));
    }

    // Once per guard per baseline; a guard that clears re-arms.
    let before: Vec<String> = st
        .get("alerted")
        .map(|a| a.items().iter().map(Json::py_str).collect())
        .unwrap_or_default();
    let kinds: Vec<&str> = alarms.iter().map(|(k, _)| *k).collect();
    let still: Vec<String> = before
        .iter()
        .filter(|k| kinds.contains(&k.as_str()))
        .cloned()
        .collect();
    let with_alerted = |list: Vec<String>| {
        let mut s = st.clone();
        if list.is_empty() {
            s.remove("alerted");
        } else {
            s.set(
                "alerted",
                Json::Arr(list.into_iter().map(Json::from).collect()),
            );
        }
        s
    };
    let changed_state = |list: &Vec<String>| (list != &before).then(|| with_alerted(list.clone()));

    if alarms.is_empty() {
        let state = if inp.dry_run {
            None
        } else {
            changed_state(&still)
        };
        return done(0, stdout, vec![], state);
    }
    let fresh = kinds.iter().any(|k| !before.iter().any(|b| b == k));
    if !fresh {
        stdout.push(format!(
            "divergence alert already sent for this baseline ({}); not sending again.",
            kinds.join(", ")
        ));
        let state = if inp.dry_run {
            None
        } else {
            changed_state(&still)
        };
        return done(0, stdout, vec![], state);
    }

    let floor_s = floor_text(floor);
    let body = format!(
        "Live crypto strategy has tripped a steering guard.\n\n{}\n\n{line}\n\n  \
         buy & hold (baseline book, valued now): ${}\n  \
         strategy   (live book, valued now):     ${}\n  \
         baseline book value (at baseline):      ${}\n  \
         expected alpha / floor (last backtest): {}% / {floor_s}\n\n\
         Steering action: re-run the backtest and investigate -- regime shift, slippage, or \
         overfit bands. Current regime label: {}. If BULL, the ladder lagging buy & hold is \
         the EXPECTED failure mode -- check trailing on running coins rather than the bands. \
         Consider `touch {}` if the drawdown guard tripped. Baseline set {}d ago; re-baselines \
         next monthly run.\n\n\
         (Read-only check. Deposits/withdrawals since baseline would skew these numbers.)",
        alarms
            .iter()
            .map(|(_, a)| format!("- {a}"))
            .collect::<Vec<_>>()
            .join("\n"),
        comma(bh_now, 2),
        comma(strat_now, 2),
        comma(base_val, 2),
        signed(expected, 2),
        upper(&inp.regime),
        inp.hints.halt_file,
        fixed(elapsed, 0),
    );
    let subject = if floor_breach {
        "Crypto strategy: DRAWDOWN beyond backtest"
    } else {
        "Crypto strategy drift: live below backtest"
    }
    .to_string();
    if inp.dry_run {
        stdout.push(format!(
            "--- Would send alert ---\nSubject: {subject}\n{body}"
        ));
        return done(0, stdout, vec![], None);
    }
    Plan::Send {
        stdout,
        subject,
        body,
        state: with_alerted(kinds.iter().map(|k| k.to_string()).collect()),
    }
}

pub const SENT_LINE: &str = "Sent divergence alert.";
pub const FAILED_LINE: &str = "Failed to send alert email.";
