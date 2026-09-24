//! Several trailing windows, their variant sweeps and per-coin band probes, rolled into
//! one verdict.
//!
//! One window is one regime; a verdict drawn from a single lucky month is noise. This runs
//! the ladder as configured over every window (28 / 90 / 180 days by default), sweeps the
//! variants on each, probes alternative bands one coin at a time on the longest window,
//! and writes an *expectation*: the alpha the live config should earn plus the worst
//! strategy return seen, so a daily divergence check can steer before a surprise.
//!
//! It never trades and never changes a tunable. A variant has to win in the longest
//! window **and** repeat the next month before anyone should touch the live bands.
//!
//! The verdict is built by reading the window and sweep reports back with the same
//! patterns the reference used, so the reports themselves stay the contract.

use rungbot_core::regime::RegimeConfig;
use rungbot_core::{Bands, Config};

use crate::py::{ff, fs, round, Py};
use crate::sweep::{self, SweepOpts};
use crate::window::{self, WindowOpts};
use crate::{Book, History};

/// Per-coin band probes, tried on the anchor window.
pub const PERCOIN_GRID: [(f64, f64); 2] = [(10.0, 5.0), (20.0, 10.0)];

#[derive(Debug, Clone)]
pub struct MonthlyOpts {
    pub windows: Vec<i64>,
    /// A variant must beat the live config by more than this many points to flag REVIEW.
    pub drift_band_pct: f64,
    pub percoin: bool,
    /// Only per-coin probes gaining at least this many alpha points are recommended.
    pub percoin_min_gain: f64,
    pub max_order_usd: f64,
    /// Dry-powder override for the sweep only, like the reference's `BT_BAG`.
    pub sweep_bag: Option<f64>,
    pub regime: RegimeConfig,
    /// Written into the expectation as `epoch`. The core reads no clock; neither does this.
    pub now: i64,
}

impl Default for MonthlyOpts {
    fn default() -> Self {
        MonthlyOpts {
            windows: vec![28, 90, 180],
            drift_band_pct: 5.0,
            percoin: true,
            percoin_min_gain: 2.0,
            max_order_usd: 50.0,
            sweep_bag: None,
            regime: RegimeConfig::default(),
            now: 0,
        }
    }
}

/// Price history for one window, or the line the window run prints when it has none.
pub type WindowHistory = Result<History, String>;

#[derive(Debug, Clone)]
pub struct MonthlyReport {
    pub subject: String,
    pub body: String,
    /// `backtest-expectation.json`, when at least one window produced a verdict.
    pub expectation: Option<String>,
    pub windows_ok: usize,
}

impl MonthlyReport {
    /// What a dry run prints instead of sending.
    pub fn dry_run_text(&self) -> String {
        format!(
            "--- Would send ---\nSubject: {}\n\n{}\n",
            self.subject, self.body
        )
    }
}

// ------------------------------------------------------------------ report parsing

fn skip_ws(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_whitespace())
}

#[derive(Clone, Copy, PartialEq)]
enum Sign {
    /// `[+-]?`
    Optional,
    /// `-?`
    MinusOnly,
    /// `[+-]`
    Required,
}

/// `<sign>\d+(?:\.\d+)?` (or `\d+\.\d+` when `frac_required`) at the start of `s`:
/// the number and the rest.
fn number(s: &str, sign: Sign, frac_required: bool) -> Option<(f64, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    let has_sign = i < b.len() && (b[i] == b'-' || (b[i] == b'+' && sign != Sign::MinusOnly));
    if has_sign {
        i += 1;
    } else if sign == Sign::Required {
        return None;
    }
    let d0 = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == d0 {
        return None;
    }
    if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    } else if frac_required {
        return None;
    }
    s[..i].parse::<f64>().ok().map(|v| (v, &s[i..]))
}

/// `STRATEGY vs BUY&HOLD:\s*([+-]?\d+(?:\.\d+)?)%`
pub fn parse_alpha(text: &str) -> Option<f64> {
    let mut rest = text;
    while let Some(p) = rest.find("STRATEGY vs BUY&HOLD:") {
        let after = skip_ws(&rest[p + "STRATEGY vs BUY&HOLD:".len()..]);
        if let Some((v, r)) = number(after, Sign::Optional, false) {
            if r.starts_with('%') {
                return Some(v);
            }
        }
        rest = &rest[p + 1..];
    }
    None
}

/// `VALUE @ end, STRATEGY:.*?\(([+-]?\d+(?:\.\d+)?)%\)`
pub fn parse_strat_return(text: &str) -> Option<f64> {
    let mut rest = text;
    while let Some(p) = rest.find("VALUE @ end, STRATEGY:") {
        let line = &rest[p + "VALUE @ end, STRATEGY:".len()..];
        let line = line.split('\n').next().unwrap_or("");
        for (k, _) in line.match_indices('(') {
            if let Some((v, r)) = number(&line[k + 1..], Sign::Optional, false) {
                if r.starts_with("%)") {
                    return Some(v);
                }
            }
        }
        rest = &rest[p + 1..];
    }
    None
}

/// `^(.{1,24}?)\s+(-?\d+\.\d+)%\s+(-?\d+\.\d+)%\s+([+-]\d+\.\d+)%` on one line.
fn variant_line(line: &str) -> Option<(String, f64)> {
    let idx: Vec<usize> = line
        .char_indices()
        .map(|(i, _)| i)
        .chain([line.len()])
        .collect();
    for take in 1..=24usize {
        if take >= idx.len() {
            break;
        }
        let name = &line[..idx[take]];
        let rest = &line[idx[take]..];
        let r = rest.trim_start_matches(|c: char| c.is_whitespace());
        if r.len() == rest.len() {
            continue;
        }
        let Some((_, r)) = number(r, Sign::MinusOnly, true).filter(|(_, r)| r.starts_with('%'))
        else {
            continue;
        };
        let r2 = &r[1..];
        let r = r2.trim_start_matches(|c: char| c.is_whitespace());
        if r.len() == r2.len() {
            continue;
        }
        let Some((_, r)) = number(r, Sign::MinusOnly, true).filter(|(_, r)| r.starts_with('%'))
        else {
            continue;
        };
        let r2 = &r[1..];
        let r = r2.trim_start_matches(|c: char| c.is_whitespace());
        if r.len() == r2.len() {
            continue;
        }
        let Some((alpha, _)) = number(r, Sign::Required, true).filter(|(_, r)| r.starts_with('%'))
        else {
            continue;
        };
        return Some((name.trim().to_string(), alpha));
    }
    None
}

/// The variant with the best alpha in a sweep report.
pub fn best_variant(sweep_text: &str) -> Option<(String, f64)> {
    let mut best: Option<(String, f64)> = None;
    for line in sweep_text.split('\n') {
        if let Some((name, alpha)) = variant_line(line) {
            if best.as_ref().is_none_or(|b| alpha > b.1) {
                best = Some((name, alpha));
            }
        }
    }
    best
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `^(?:\w+(?:\.\w+)*)?(?:Error|Exception)(?::.*)?$`
fn error_line(line: &str) -> bool {
    let head = match line.find(':') {
        Some(p) => &line[..p],
        None => line,
    };
    for suffix in ["Error", "Exception"] {
        if let Some(prefix) = head.strip_suffix(suffix) {
            if prefix.is_empty()
                || prefix
                    .split('.')
                    .all(|part| !part.is_empty() && part.chars().all(is_word))
            {
                return true;
            }
        }
    }
    false
}

/// Why a window produced no verdict: quote the actual error, never guess a cause.
pub fn failure_reason(text: &str) -> String {
    let clip = |s: &str| s.trim().chars().take(140).collect::<String>();
    let lines: Vec<&str> = text.split('\n').collect();
    if let Some(l) = lines.iter().rev().find(|l| error_line(l)) {
        return clip(l);
    }
    for l in &lines {
        let hit = (l.starts_with("history ") && {
            let rest = &l["history ".len()..];
            let w: String = rest.chars().take_while(|c| is_word(*c)).collect();
            !w.is_empty() && rest[w.len()..].starts_with(" failed")
        }) || l.starts_with("balances fetch failed")
            || l.starts_with("client init failed");
        if hit {
            return l.trim().chars().take(140).collect();
        }
    }
    "no P&L line in output".into()
}

// ------------------------------------------------------------------ the run

struct WinResult {
    alpha: Option<f64>,
    strat_return: Option<f64>,
    best_name: Option<String>,
    best_alpha: Option<f64>,
    reason: Option<String>,
}

fn run_window(
    cfg: &Config,
    hist: &WindowHistory,
    book: &Book,
    days: i64,
    opts: &MonthlyOpts,
) -> (String, Option<Book>) {
    let wo = WindowOpts {
        days,
        max_order_usd: opts.max_order_usd,
        bag: None,
    };
    let r = window::run(cfg, hist.as_ref().map_err(|e| e.clone()), book, &wo);
    (r.text.trim().to_string(), r.book)
}

fn fmt_cfg(b: Bands) -> String {
    format!(
        "{}/{}",
        rungbot_core::fmt::g(b.first_pct),
        rungbot_core::fmt::g(b.step_pct)
    )
}

/// Run every window, the sweeps and the probes; build the verdict.
///
/// `history_for(days)` supplies each window's prices; the book is the start book every
/// window and sweep replays.
pub fn run(
    cfg: &Config,
    book: &Book,
    history_for: &dyn Fn(i64) -> WindowHistory,
    opts: &MonthlyOpts,
) -> MonthlyReport {
    let bands = cfg.settings.bands;
    let cfg_label = fmt_cfg(bands);
    let mut results: Vec<(i64, WinResult)> = Vec::new();
    let mut sections: Vec<String> = Vec::new();
    let mut hist_cache: Vec<(i64, WindowHistory)> = Vec::new();

    for &days in &opts.windows {
        let hist = history_for(days);
        let (bt, book_out) = run_window(cfg, &hist, book, days, opts);
        let so = SweepOpts {
            days,
            bag: opts.sweep_bag,
            regime: opts.regime,
        };
        // With no cache the reference sweep dies on the missing file; keep its last line.
        let sweep_hist = hist.as_ref().map_err(|_| {
            format!(
                "FileNotFoundError: [Errno 2] No such file or directory: 'bt-hist-{days}d.json'"
            )
        });
        let sw = match sweep::run(cfg, sweep_hist, book_out.as_ref(), &so) {
            Ok(t) => t.trim().to_string(),
            Err(e) => format!("[stderr]\n{e}").trim().to_string(),
        };
        let alpha = parse_alpha(&bt);
        let bv = best_variant(&sw);
        results.push((
            days,
            WinResult {
                alpha,
                strat_return: parse_strat_return(&bt),
                best_name: bv.as_ref().map(|b| b.0.clone()),
                best_alpha: bv.as_ref().map(|b| b.1),
                reason: if alpha.is_some() {
                    None
                } else {
                    Some(failure_reason(&bt))
                },
            },
        ));
        let eq = "=".repeat(60);
        let dash = "-".repeat(60);
        sections.push(format!(
            "{eq}\n[{days}d] AS CONFIGURED\n{eq}\n{bt}\n\n{dash}\n[{days}d] VARIANT SWEEP\n{dash}\n{sw}\n"
        ));
        hist_cache.push((days, hist));
    }

    let ok: Vec<&(i64, WinResult)> = results.iter().filter(|(_, r)| r.alpha.is_some()).collect();
    let anchor = ok.iter().map(|(d, _)| *d).max();

    // --- per-coin band probes on the anchor window (recommendation only) ---
    let mut percoin: Vec<(String, f64, f64, f64)> = Vec::new();
    if let (Some(anchor), true) = (anchor, opts.percoin) {
        let base_alpha = ok
            .iter()
            .find(|(d, _)| *d == anchor)
            .and_then(|(_, r)| r.alpha)
            .expect("anchor has an alpha");
        let hist = &hist_cache
            .iter()
            .find(|(d, _)| *d == anchor)
            .expect("anchor window ran")
            .1;
        for coin in &cfg.coins {
            let mut best: Option<(f64, f64, f64)> = None;
            for (f, s) in PERCOIN_GRID {
                if (f, s) == (bands.first_pct, bands.step_pct) {
                    continue;
                }
                let mut probe = cfg.clone();
                for c in probe.coins.iter_mut() {
                    c.bands = if c.symbol == coin.symbol {
                        Some(Bands {
                            first_pct: f,
                            step_pct: s,
                        })
                    } else {
                        None
                    };
                }
                let (out, _) = run_window(&probe, hist, book, anchor, opts);
                let Some(alpha) = parse_alpha(&out) else {
                    continue;
                };
                let delta = alpha - base_alpha;
                if best.is_none_or(|b| delta > b.2) {
                    best = Some((f, s, delta));
                }
            }
            if let Some((f, s, d)) = best {
                percoin.push((coin.symbol.clone(), f, s, round(d, 2)));
            }
        }
    }

    // --- expectation: anchor on the longest good window; floor = worst strat return ---
    let floor = ok
        .iter()
        .filter_map(|(_, r)| r.strat_return)
        .fold(None, |m: Option<f64>, v| {
            Some(m.map_or(v, |m| if v < m { v } else { m }))
        });
    let expectation = anchor.map(|anchor| {
        let a = ok.iter().find(|(d, _)| *d == anchor).expect("anchor");
        let mut windows = Py::dict();
        for (d, r) in &ok {
            windows.set(
                d.to_string(),
                crate::pydict![("alpha_pct", r.alpha), ("strat_return_pct", r.strat_return)],
            );
        }
        let recs = Py::List(
            percoin
                .iter()
                .map(|(s, f, st, d)| {
                    crate::pydict![
                        ("sym", s.as_str()),
                        ("first_pct", *f),
                        ("step_pct", *st),
                        ("alpha_delta_pp", *d)
                    ]
                })
                .collect(),
        );
        crate::pydict![
            ("epoch", opts.now),
            ("anchor_days", anchor),
            ("alpha_pct", a.1.alpha),
            ("strat_return_floor_pct", floor),
            ("windows", windows),
            (
                "config",
                crate::pydict![("first_pct", bands.first_pct), ("step_pct", bands.step_pct)]
            ),
            ("percoin_recommendations", recs),
        ]
        .json(Some(2))
    });

    // --- cross-window table + verdict ---
    let mut rows = vec![format!(
        "{} {} {} {} {}",
        fs("window", ">7"),
        fs("alpha", ">8"),
        fs("strat%", ">8"),
        fs("best variant", "<22"),
        fs("bestα", ">7")
    )];
    for &d in &opts.windows {
        let r = &results.iter().find(|(dd, _)| *dd == d).expect("ran").1;
        let label = format!("{d}d");
        if let Some(alpha) = r.alpha {
            rows.push(format!(
                "{} {}% {}% {} {}%",
                fs(&label, ">7"),
                ff(alpha, ">+7.1f"),
                ff(r.strat_return.unwrap_or(0.0), ">+7.1f"),
                fs(r.best_name.as_deref().unwrap_or("-"), "<22"),
                ff(r.best_alpha.unwrap_or(0.0), ">+6.1f")
            ));
        } else {
            rows.push(format!(
                "{}   FAILED: {} -- see raw output",
                fs(&label, ">7"),
                r.reason.as_deref().unwrap_or("")
            ));
        }
    }
    let table = rows.join("\n");

    let drift = opts.drift_band_pct;
    let verdict = match anchor {
        None => {
            let reasons = opts
                .windows
                .iter()
                .map(|d| {
                    let r = &results.iter().find(|(dd, _)| dd == d).expect("ran").1;
                    format!("{d}d {}", r.reason.as_deref().unwrap_or("None"))
                })
                .collect::<Vec<_>>()
                .join("; ");
            format!("BROKEN: no window produced a P&L verdict ({reasons}). See raw output.")
        }
        Some(anchor) => {
            let a = &ok.iter().find(|(d, _)| *d == anchor).expect("anchor").1;
            let a_alpha = a.alpha.expect("ok");
            let mut v = match (a.best_alpha, a.best_name.as_deref()) {
                (Some(ba), name) if ba > a_alpha + drift => format!(
                    "REVIEW: in the {anchor}d window, variant '{}' scored {}% vs the live {cfg_label} at {}% (>{}pp better). Confirm it also wins next month before changing live bands -- do not act on one reading.",
                    name.unwrap_or("None"),
                    ff(ba, "+.1f"),
                    ff(a_alpha, "+.1f"),
                    ff(drift, ".0f")
                ),
                _ => format!(
                    "HOLD: live {cfg_label} earns {}% alpha over the {anchor}d window and no variant beats it by >{}pp there. Keep the live bands as-is.",
                    ff(a_alpha, "+.1f"),
                    ff(drift, ".0f")
                ),
            };
            v.push_str(&format!(
                " Downside floor recorded: worst strategy return across windows = {}% (early-warning guard for the divergence check).",
                ff(floor.expect("ok windows have a strategy return"), "+.1f")
            ));
            v
        }
    };

    // --- per-coin recommendation block (advice only, never auto-applied) ---
    let winners: Vec<&(String, f64, f64, f64)> = percoin
        .iter()
        .filter(|r| r.3 >= opts.percoin_min_gain)
        .collect();
    let pc_section = if !winners.is_empty() {
        let lines: Vec<String> = winners
            .iter()
            .map(|(s, f, st, d)| {
                format!(
                    "  {} -> {}/{} ({}pp alpha vs live {cfg_label} on the anchor window)",
                    fs(s, "5"),
                    ff(*f, ".0f"),
                    ff(*st, ".0f"),
                    ff(*d, "+.1f")
                )
            })
            .collect();
        let bands_json = Py::Dict(
            winners
                .iter()
                .map(|(s, f, st, _)| {
                    (
                        Py::from(s.as_str()),
                        Py::List(vec![Py::Float(*f), Py::Float(*st)]),
                    )
                })
                .collect(),
        )
        .json(None);
        format!(
            "PER-COIN BAND PROBES (anchor window; recommendation only)\n{}\n  Apply ONLY after this repeats next month: set these per-coin bands in the config: {bands_json}.\n\n",
            lines.join("\n")
        )
    } else if !percoin.is_empty() {
        format!(
            "PER-COIN BAND PROBES: no coin beats the live {cfg_label} bands by >={}pp on the anchor window -- keep global bands.\n\n",
            ff(opts.percoin_min_gain, ".0f")
        )
    } else {
        String::new()
    };

    let windows_label = opts
        .windows
        .iter()
        .map(|w| format!("{w}d"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        "Monthly crypto position-manager backtest (live config {cfg_label}), windows: {windows_label}.\n\n\
         VERDICT: {verdict}\n\n\
         CROSS-WINDOW SUMMARY\n{table}\n\n\
         {pc_section}\
         Reminder: a winner must hold in the LONGEST window AND repeat next month before you touch live bands. One window is one regime.\n\n\
         {}",
        sections.join("\n")
    );

    let failed = opts
        .windows
        .iter()
        .filter(|d| {
            results
                .iter()
                .find(|(dd, _)| dd == *d)
                .is_some_and(|(_, r)| r.alpha.is_none())
        })
        .count();
    let mut subject = match anchor {
        Some(a) => {
            let alpha = ok
                .iter()
                .find(|(d, _)| *d == a)
                .and_then(|(_, r)| r.alpha)
                .expect("ok");
            format!(
                "Crypto backtest (monthly): {a}d alpha {}%",
                ff(alpha, "+.1f")
            )
        }
        None => "Crypto backtest (monthly): check needed".to_string(),
    };
    if failed > 0 {
        // A dead window must be visible from the inbox: the verdict is drawn from
        // whatever survived, so a silent partial run reads as a full one.
        subject.push_str(&format!(
            " -- {failed}/{} windows FAILED",
            opts.windows.len()
        ));
    }

    MonthlyReport {
        subject,
        body,
        expectation,
        windows_ok: ok.len(),
    }
}
