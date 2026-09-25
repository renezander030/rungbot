//! `rungbot-exec shadow-diff`: compare two decision logs run by run.
//!
//! The reference log comes from the implementation being replaced, the other from a
//! shadow run of this one on the same input state. Both use the decision-log format of
//! [`crate::run::decisions`]: one line per outcome, then a `run` line, all lines of a
//! run sharing its `ts`.
//!
//! Runs are paired by time: each reference run with the nearest unpaired run of the
//! other log within `window` seconds. Within a pair, records match on their structure:
//! source (`ladder`, `hk`, `deploy`), kind, symbol, side and the text with its numbers
//! taken out. The numbers are then compared: a rung, a count or a day number exactly,
//! an amount or a price (a `$` or `~` before it, or a fraction) within the price-delta
//! tolerance, a percentage within the tolerance in points. The run line's signal counts
//! must be equal.
//!
//! What does not match is a difference. A difference the table of known, intentional
//! divergences ([`KNOWN`]) accounts for is EXPLAINED, with its reason; any other is
//! UNEXPLAINED, and the command exits 1.

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// One known, intentional divergence from the reference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Known {
    pub id: &'static str,
    pub reason: &'static str,
    /// Can it change a line of the run's decision log? Those that cannot are listed for
    /// completeness: a shadow diff never meets them.
    pub in_decisions: bool,
}

/// Every divergence from the reference that is on purpose: its bugs, ported fixed.
pub const KNOWN: [Known; 11] = [
    Known {
        id: "market-zero",
        reason: "a manual market deploy with nothing to spend sends a $0 market buy in the \
                 reference; this runtime sends none",
        in_decisions: true,
    },
    Known {
        id: "tranche-baseline",
        reason: "a manual tranche of fresh money does not raise the reference's baseline, so \
                 its next run ladders the same money again; this runtime raises the \
                 baseline by the new money placed, capped at the balance above it",
        in_decisions: true,
    },
    Known {
        id: "audit-withdrawal",
        reason: "the reference's book audit counts a stable withdrawal on its way out of \
                 the venue as locked in resting buys; this runtime reads it as in flight",
        in_decisions: true,
    },
    Known {
        id: "rollback-window",
        reason: "when a dip-ladder signal rolls back, the reference leaves the trade window \
                 it opened (win_dir/win_until) in place, which holds back the coin's next \
                 signal; this runtime restores the window with the rungs",
        in_decisions: true,
    },
    Known {
        id: "fillodds-partial",
        reason: "fill odds counted a partly filled rung at its full size (report only)",
        in_decisions: false,
    },
    Known {
        id: "coingecko-id",
        reason: "the wallet and scenario collectors used different CoinGecko ids for one \
                 coin (dashboard only)",
        in_decisions: false,
    },
    Known {
        id: "divergence-dedupe",
        reason: "the divergence mail had no dedupe (watcher mail only)",
        in_decisions: false,
    },
    Known {
        id: "regime-return",
        reason: "a confirmed regime label that came back stayed silent (watcher mail only)",
        in_decisions: false,
    },
    Known {
        id: "atomic-state",
        reason: "froth, zone, audit and divergence state were written in place, not by \
                 tmp-and-rename (file safety only)",
        in_decisions: false,
    },
    Known {
        id: "offline-switch",
        reason: "signed Revolut X calls skipped the offline switch (tests and CI only)",
        in_decisions: false,
    },
    Known {
        id: "signals-follow",
        reason: "the run's signal count differs only by signals an explained difference \
                 accounts for",
        in_decisions: true,
    },
];

fn known(id: &str) -> &'static Known {
    KNOWN.iter().find(|k| k.id == id).expect("a known id")
}

/// One decision-log line.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub ts: f64,
    pub run: String,
    pub sym: Option<String>,
    pub side: Option<String>,
    pub src: String,
    pub kind: String,
    pub text: String,
}

impl Record {
    fn from_json(v: &Value) -> Result<Record, String> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Ok(Record {
            ts: v
                .get("ts")
                .and_then(Value::as_f64)
                .ok_or("a line without a numeric ts")?,
            run: s("run").unwrap_or_default(),
            sym: s("sym").filter(|x| !x.is_empty()),
            side: s("side").filter(|x| !x.is_empty()),
            src: s("src").unwrap_or_default(),
            kind: s("kind").unwrap_or_default(),
            text: s("text").unwrap_or_default(),
        })
    }

    fn label(&self) -> String {
        format!(
            "{} {} {} {} {:?}",
            self.src,
            self.kind,
            self.sym.as_deref().unwrap_or("-"),
            self.side.as_deref().unwrap_or("-"),
            self.text
        )
    }
}

/// One run: its outcome lines and its closing `run` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub ts: f64,
    pub run: String,
    pub records: Vec<Record>,
    /// `(buy, sell)` from the run line, when there is one.
    pub signals: Option<(u64, u64)>,
}

fn signals_of(text: &str) -> Option<(u64, u64)> {
    let rest = text.strip_prefix("signals buy=")?;
    let (b, rest) = rest.split_once(" sell=")?;
    let s: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((b.parse().ok()?, s.parse().ok()?))
}

/// The runs of a decision log. Lines that do not parse are an error: a log the diff
/// cannot read is not a clean diff.
pub fn read_runs(text: &str) -> Result<Vec<Run>, String> {
    let mut runs: Vec<Run> = Vec::new();
    let mut open: Option<Run> = None;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        let r = Record::from_json(&v).map_err(|e| format!("line {}: {e}", i + 1))?;
        if open.as_ref().is_some_and(|o| o.ts != r.ts) {
            runs.extend(open.take());
        }
        let cur = open.get_or_insert_with(|| Run {
            ts: r.ts,
            run: r.run.clone(),
            records: Vec::new(),
            signals: None,
        });
        if r.kind == "run" {
            cur.signals = signals_of(&r.text);
            runs.extend(open.take());
        } else {
            cur.records.push(r);
        }
    }
    runs.extend(open);
    Ok(runs)
}

/// A number in a text, and whether it compares within the tolerance.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Num {
    /// A rung, a count, a day number: must be equal.
    Exact(f64),
    /// An amount or a price.
    Amount(f64),
    /// A percentage, compared in points.
    Pct(f64),
}

/// A text with its numbers taken out (`#`), and the numbers.
fn shape(text: &str) -> (String, Vec<Num>) {
    let b: Vec<char> = text.chars().collect();
    let mut tpl = String::new();
    let mut nums = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let starts = c.is_ascii_digit()
            || ((c == '-' || c == '+') && b.get(i + 1).is_some_and(char::is_ascii_digit));
        let prev = if i > 0 { Some(b[i - 1]) } else { None };
        // A digit inside a word (`v4`, `USDT2`) is part of the word.
        if !starts || prev.is_some_and(|p| p.is_ascii_alphabetic() || p == '_') {
            tpl.push(c);
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && (b[j].is_ascii_digit() || b[j] == '.' || b[j] == ',') {
            j += 1;
        }
        if j < b.len() && (b[j] == 'e' || b[j] == 'E') {
            let k = j + 1 + usize::from(matches!(b.get(j + 1), Some('-' | '+')));
            if b.get(k).is_some_and(char::is_ascii_digit) {
                j = k;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
            }
        }
        // A trailing dot ends a sentence, not a number.
        while j > i + 1 && matches!(b[j - 1], '.' | ',') {
            j -= 1;
        }
        let raw: String = b[i..j].iter().filter(|c| **c != ',').collect();
        let Ok(v) = raw.parse::<f64>() else {
            tpl.push(c);
            i += 1;
            continue;
        };
        let money = matches!(prev, Some('$' | '~'));
        let pct = b.get(j) == Some(&'%');
        let fractional = raw.contains('.') || raw.contains('e') || raw.contains('E');
        nums.push(if pct {
            Num::Pct(v)
        } else if money || fractional {
            Num::Amount(v)
        } else {
            Num::Exact(v)
        });
        tpl.push('#');
        i = j;
    }
    (tpl, nums)
}

/// The comparison's knobs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tolerance {
    /// Relative, in percent, for amounts and prices; in points for percentages.
    pub pct: f64,
    /// Absolute floor for amounts (a cent by default).
    pub abs: f64,
    /// Seconds between two runs that still pair.
    pub window_s: f64,
}

impl Default for Tolerance {
    fn default() -> Self {
        Tolerance {
            pct: 2.0,
            abs: 0.01,
            window_s: 1200.0,
        }
    }
}

fn close(a: &Num, b: &Num, t: &Tolerance) -> bool {
    match (a, b) {
        (Num::Exact(x), Num::Exact(y)) => x == y,
        (Num::Amount(x), Num::Amount(y)) => {
            (x - y).abs() <= t.abs.max(t.pct / 100.0 * x.abs().max(y.abs()))
        }
        (Num::Pct(x), Num::Pct(y)) => (x - y).abs() <= t.pct.max(t.abs),
        _ => false,
    }
}

/// Where a difference sits.
#[derive(Debug, Clone, PartialEq)]
pub enum Where {
    /// A reference run with no run of ours near it.
    UnpairedReference,
    /// A run of ours with no reference run near it.
    UnpairedOurs,
    /// A line only the reference wrote.
    ReferenceOnly(Record),
    /// A line only this runtime wrote.
    OursOnly(Record),
    /// The same line with numbers outside the tolerance.
    Amounts(Record, Record),
    /// The run line's signal counts.
    Signals((u64, u64), (u64, u64)),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Difference {
    /// The reference run's minute, else ours.
    pub run: String,
    pub at: Where,
    /// The known divergence that explains it.
    pub explained: Option<&'static Known>,
}

impl Difference {
    pub fn line(&self) -> String {
        let what = match &self.at {
            Where::UnpairedReference => "a reference run with no shadow run near it".into(),
            Where::UnpairedOurs => "a shadow run with no reference run near it".into(),
            Where::ReferenceOnly(r) => format!("reference only: {}", r.label()),
            Where::OursOnly(r) => format!("shadow only: {}", r.label()),
            Where::Amounts(a, b) => format!("amounts differ: {} | shadow {:?}", a.label(), b.text),
            Where::Signals(a, b) => format!(
                "signals differ: reference buy={} sell={}, shadow buy={} sell={}",
                a.0, a.1, b.0, b.1
            ),
        };
        match self.explained {
            Some(k) => format!("EXPLAINED [{}] {} {what} -- {}", k.id, self.run, k.reason),
            None => format!("UNEXPLAINED {} {what}", self.run),
        }
    }

    pub fn to_json(&self) -> Value {
        let rec = |r: &Record| json!({"src": r.src, "kind": r.kind, "sym": r.sym, "side": r.side, "text": r.text});
        let (kind, detail) = match &self.at {
            Where::UnpairedReference => ("unpaired_reference", Value::Null),
            Where::UnpairedOurs => ("unpaired_shadow", Value::Null),
            Where::ReferenceOnly(r) => ("reference_only", rec(r)),
            Where::OursOnly(r) => ("shadow_only", rec(r)),
            Where::Amounts(a, b) => ("amounts", json!({"reference": rec(a), "shadow": rec(b)})),
            Where::Signals(a, b) => (
                "signals",
                json!({"reference": [a.0, a.1], "shadow": [b.0, b.1]}),
            ),
        };
        json!({"run": self.run, "kind": kind, "detail": detail,
               "explained": self.explained.map(|k| k.id),
               "reason": self.explained.map(|k| k.reason)})
    }
}

/// The whole comparison.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub pairs: usize,
    pub matched: usize,
    pub differences: Vec<Difference>,
}

impl Report {
    pub fn unexplained(&self) -> usize {
        self.differences
            .iter()
            .filter(|d| d.explained.is_none())
            .count()
    }

    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self.differences.iter().map(Difference::line).collect();
        let explained = self.differences.len() - self.unexplained();
        out.push(format!(
            "shadow-diff: {} run pair(s), {} line(s) matched, {} explained, {} unexplained",
            self.pairs,
            self.matched,
            explained,
            self.unexplained()
        ));
        out
    }

    pub fn to_json(&self) -> Value {
        json!({"pairs": self.pairs, "matched": self.matched,
               "unexplained": self.unexplained(),
               "differences": self.differences.iter().map(Difference::to_json).collect::<Vec<_>>()})
    }
}

/// What the comparison is told.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub tolerance: Tolerance,
    /// Only runs at or after this epoch second are compared.
    pub since: Option<f64>,
    /// Text rewrites applied to the reference's lines first: the reference named itself
    /// (its script, its log) where this runtime names itself.
    pub rename: Vec<(String, String)>,
}

fn key(r: &Record) -> (String, String, Option<String>, Option<String>, String) {
    (
        r.src.clone(),
        r.kind.clone(),
        r.sym.clone(),
        r.side.clone(),
        shape(&r.text).0,
    )
}

fn nums_close(a: &Record, b: &Record, t: &Tolerance) -> bool {
    let (x, y) = (shape(&a.text).1, shape(&b.text).1);
    x.len() == y.len() && x.iter().zip(&y).all(|(p, q)| close(p, q, t))
}

/// The first `$` amount in a text.
fn first_amount(text: &str) -> Option<f64> {
    let (_, nums) = shape(text);
    nums.into_iter().find_map(|n| match n {
        Num::Amount(v) => Some(v),
        _ => None,
    })
}

/// Amounts after `$` only (not `~`), in order.
fn dollar_amounts(text: &str) -> Vec<f64> {
    text.split('$')
        .skip(1)
        .filter_map(|t| first_amount(&format!("${t}")))
        .collect()
}

/// The known divergence behind one difference, given the reference's full history
/// (`history`, runs before this one included) and this pair's other differences.
fn explain(d: &Where, history: &[Run], ref_ts: f64) -> Option<&'static Known> {
    let reference_side = match d {
        Where::ReferenceOnly(r) => Some(r),
        Where::Amounts(r, _) => Some(r),
        _ => None,
    };
    if let Some(r) = reference_side {
        let t = &r.text;
        // A $0 market buy the reference sent.
        if t.to_lowercase().contains("market buy")
            && dollar_amounts(t).first().is_some_and(|x| *x == 0.0)
        {
            return Some(known("market-zero"));
        }
        // The audit counting a stable withdrawal in flight as locked in buys.
        if r.src == "hk"
            && r.kind == "warn"
            && t.contains("AUDIT: revx:")
            && t.contains("stable locked at the venue")
        {
            return Some(known("audit-withdrawal"));
        }
        // A deploy tranche of money the reference's baseline never took in.
        if r.src == "deploy" && t.contains(" tranche on ") && t.contains(" fresh ") {
            let theirs = dollar_amounts(t).first().copied().unwrap_or(0.0);
            let ours = match d {
                Where::Amounts(_, o) => dollar_amounts(&o.text).first().copied().unwrap_or(0.0),
                _ => 0.0,
            };
            if theirs > ours {
                return Some(known("tranche-baseline"));
            }
        }
    }
    // A ladder signal on one side only, after the reference rolled back a signal of the
    // same coin and left its trade window open.
    let ladder = match d {
        Where::ReferenceOnly(r) | Where::OursOnly(r) if r.src == "ladder" => Some(r),
        _ => None,
    };
    if let Some(r) = ladder {
        let last = history
            .iter()
            .filter(|run| run.ts < ref_ts)
            .flat_map(|run| run.records.iter())
            .rev()
            .find(|x| x.src == "ladder" && x.sym == r.sym);
        if last.is_some_and(|x| x.kind == "skip" || x.kind == "err") {
            return Some(known("rollback-window"));
        }
    }
    None
}

fn renamed(r: &Record, rename: &[(String, String)]) -> Record {
    let mut r = r.clone();
    for (a, b) in rename {
        r.text = r.text.replace(a.as_str(), b);
    }
    r
}

/// Compare one pair of runs.
fn compare_pair(reference: &Run, ours: &Run, history: &[Run], opts: &Options, report: &mut Report) {
    let theirs: Vec<Record> = reference
        .records
        .iter()
        .map(|r| renamed(r, &opts.rename))
        .collect();
    let mut left: Vec<Option<&Record>> = ours.records.iter().map(Some).collect();
    let mut diffs: Vec<Where> = Vec::new();
    let mut near: Vec<(Record, usize)> = Vec::new();
    for r in &theirs {
        let k = key(r);
        let same_key: Vec<usize> = left
            .iter()
            .enumerate()
            .filter(|(_, o)| o.is_some_and(|o| key(o) == k))
            .map(|(i, _)| i)
            .collect();
        if let Some(&i) = same_key
            .iter()
            .find(|&&i| nums_close(r, left[i].expect("unmatched"), &opts.tolerance))
        {
            left[i] = None;
            report.matched += 1;
        } else if let Some(&i) = same_key.first() {
            near.push((r.clone(), i));
            left[i] = None;
        } else {
            diffs.push(Where::ReferenceOnly(r.clone()));
        }
    }
    for (r, i) in near {
        diffs.push(Where::Amounts(r, ours.records[i].clone()));
    }
    for o in left.into_iter().flatten() {
        diffs.push(Where::OursOnly(o.clone()));
    }
    let mut out: Vec<Difference> = diffs
        .into_iter()
        .map(|at| Difference {
            run: reference.run.clone(),
            explained: explain(&at, history, reference.ts),
            at,
        })
        .collect();
    // The zone lines of a tranche the reference should not have made go with it.
    let extra_tranche = out
        .iter()
        .any(|d| d.explained.is_some_and(|k| k.id == "tranche-baseline"));
    if extra_tranche {
        for d in out.iter_mut().filter(|d| d.explained.is_none()) {
            if let Where::ReferenceOnly(r) = &d.at {
                if r.src == "deploy" && r.kind == "done" && r.text.contains("[spot $") {
                    d.explained = Some(known("tranche-baseline"));
                }
            }
        }
    }
    if let (Some(a), Some(b)) = (reference.signals, ours.signals) {
        if a != b {
            // Explained only when every ladder line on one side is itself explained.
            let ladder: Vec<&Difference> = out
                .iter()
                .filter(|d| match &d.at {
                    Where::ReferenceOnly(r) | Where::OursOnly(r) => r.src == "ladder",
                    _ => false,
                })
                .collect();
            let follows = !ladder.is_empty() && ladder.iter().all(|d| d.explained.is_some());
            out.push(Difference {
                run: reference.run.clone(),
                at: Where::Signals(a, b),
                explained: follows.then(|| known("signals-follow")),
            });
        }
    }
    report.differences.extend(out);
}

/// Compare the runs of two logs.
pub fn compare(reference: &str, ours: &str, opts: &Options) -> Result<Report, String> {
    let all_ref = read_runs(reference).map_err(|e| format!("reference log: {e}"))?;
    let all_ours = read_runs(ours).map_err(|e| format!("shadow log: {e}"))?;
    let since = opts.since.unwrap_or(f64::NEG_INFINITY);
    let refs: Vec<&Run> = all_ref.iter().filter(|r| r.ts >= since).collect();
    let mut mine: Vec<Option<&Run>> = all_ours
        .iter()
        .filter(|r| r.ts >= since)
        .map(Some)
        .collect();
    let mut report = Report::default();
    for r in refs {
        let best = mine
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.map(|o| (i, (o.ts - r.ts).abs())))
            .filter(|(_, d)| *d <= opts.tolerance.window_s)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((i, _)) => {
                let o = mine[i].take().expect("unpaired");
                report.pairs += 1;
                compare_pair(r, o, &all_ref, opts, &mut report);
            }
            None => report.differences.push(Difference {
                run: r.run.clone(),
                at: Where::UnpairedReference,
                explained: None,
            }),
        }
    }
    for o in mine.into_iter().flatten() {
        report.differences.push(Difference {
            run: o.run.clone(),
            at: Where::UnpairedOurs,
            explained: None,
        });
    }
    Ok(report)
}

/// The known divergences as lines, for `shadow-diff --list`.
pub fn known_lines() -> Vec<String> {
    KNOWN
        .iter()
        .map(|k| {
            format!(
                "{:18} {} {}",
                k.id,
                if k.in_decisions {
                    "[decisions]"
                } else {
                    "[elsewhere]"
                },
                k.reason
            )
        })
        .collect()
}

/// `a=b,c=d` as rewrite pairs.
pub fn parse_rename(s: &str) -> Result<Vec<(String, String)>, String> {
    s.split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            p.split_once('=')
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .ok_or_else(|| format!("--rename {p:?}: expected OLD=NEW"))
        })
        .collect()
}

/// Group counts by explanation id, for a one-line summary.
pub fn by_reason(report: &Report) -> BTreeMap<&'static str, usize> {
    let mut m = BTreeMap::new();
    for d in &report.differences {
        *m.entry(d.explained.map_or("unexplained", |k| k.id))
            .or_default() += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(
        ts: f64,
        sym: Option<&str>,
        side: Option<&str>,
        src: &str,
        kind: &str,
        text: &str,
    ) -> String {
        json!({"ts": ts, "run": format!("R{ts}"), "sym": sym, "side": side, "src": src,
               "kind": kind, "text": text, "committed": false})
        .to_string()
    }

    fn run_line(ts: f64, buy: u64, sell: u64) -> String {
        json!({"ts": ts, "run": format!("R{ts}"), "sym": null, "side": null, "src": "run",
               "kind": "run", "mode": "live",
               "text": format!("signals buy={buy} sell={sell}; results done=0 err=0 skip=0 warn=0 plan=0"),
               "committed": false})
        .to_string()
    }

    fn log(lines: &[String]) -> String {
        lines.join("\n") + "\n"
    }

    fn opts() -> Options {
        Options::default()
    }

    #[test]
    fn numbers_split_into_exact_amounts_and_percentages() {
        let (t, n) = shape("rung 2: BUY ~$12.50 of AAA @ $0.25 (-8.1%) 28d low, v4 api.");
        assert_eq!(t, "rung #: BUY ~$# of AAA @ $# (#%) #d low, v4 api.");
        assert_eq!(
            n,
            vec![
                Num::Exact(2.0),
                Num::Amount(12.5),
                Num::Amount(0.25),
                Num::Pct(-8.1),
                Num::Exact(28.0)
            ]
        );
        assert_eq!(shape("drift 1e-05 left").1, vec![Num::Amount(1e-5)]);
    }

    #[test]
    fn identical_logs_have_no_difference() {
        let l = log(&[
            line(
                100.0,
                Some("AAA"),
                Some("buy"),
                "ladder",
                "done",
                "BUY ~$10.00 filled, rung 1",
            ),
            run_line(100.0, 1, 0),
        ]);
        let r = compare(&l, &l, &opts()).unwrap();
        assert_eq!((r.pairs, r.matched, r.unexplained()), (1, 1, 0));
        assert!(r.differences.is_empty());
    }

    #[test]
    fn a_price_a_minute_apart_is_within_tolerance_a_rung_is_not() {
        let a = log(&[
            line(
                100.0,
                Some("AAA"),
                Some("buy"),
                "ladder",
                "done",
                "BUY ~$10.00 @ $2.000, rung 1",
            ),
            run_line(100.0, 1, 0),
        ]);
        let b = log(&[
            line(
                160.0,
                Some("AAA"),
                Some("buy"),
                "ladder",
                "done",
                "BUY ~$10.00 @ $2.010, rung 1",
            ),
            run_line(160.0, 1, 0),
        ]);
        let r = compare(&a, &b, &opts()).unwrap();
        assert_eq!((r.matched, r.unexplained()), (1, 0));
        let c = b.replace("rung 1", "rung 2");
        let r = compare(&a, &c, &opts()).unwrap();
        assert_eq!(r.unexplained(), 1);
        assert!(matches!(r.differences[0].at, Where::Amounts(..)));
        let d = b.replace("$2.010", "$2.200");
        assert_eq!(compare(&a, &d, &opts()).unwrap().unexplained(), 1);
    }

    #[test]
    fn a_line_on_one_side_is_unexplained_and_the_signal_count_too() {
        let a = log(&[run_line(100.0, 0, 0)]);
        let b = log(&[
            line(
                100.0,
                Some("AAA"),
                Some("buy"),
                "ladder",
                "done",
                "BUY ~$10.00 filled",
            ),
            run_line(100.0, 1, 0),
        ]);
        let r = compare(&a, &b, &opts()).unwrap();
        assert_eq!(r.unexplained(), 2);
        assert!(r
            .lines()
            .iter()
            .any(|l| l.starts_with("UNEXPLAINED R100 shadow only")));
    }

    #[test]
    fn runs_pair_within_the_window_only() {
        let a = log(&[run_line(100.0, 0, 0)]);
        let b = log(&[run_line(5000.0, 0, 0)]);
        let r = compare(&a, &b, &opts()).unwrap();
        assert_eq!(r.pairs, 0);
        assert_eq!(r.unexplained(), 2);
        let r = compare(
            &a,
            &b,
            &Options {
                since: Some(200.0),
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(r.unexplained(), 1, "only ours is after --since");
    }

    #[test]
    fn the_audit_withdrawal_warning_is_explained() {
        let a = log(&[
            line(100.0, None, None, "hk", "warn",
                 "BOOK AUDIT: revx: $50.00 stable locked at the venue vs $0.00 in journal open buys (diff $+50.00, tolerance $2.00)"),
            run_line(100.0, 0, 0),
        ]);
        let b = log(&[run_line(100.0, 0, 0)]);
        let r = compare(&a, &b, &opts()).unwrap();
        assert_eq!(r.unexplained(), 0);
        assert_eq!(r.differences[0].explained.unwrap().id, "audit-withdrawal");
    }

    #[test]
    fn a_zero_market_buy_is_explained() {
        let a = log(&[
            line(
                100.0,
                Some("AAA"),
                Some("buy"),
                "deploy",
                "done",
                "market buy AAA $0.00 of $0.00 budget",
            ),
            run_line(100.0, 0, 0),
        ]);
        let b = log(&[run_line(100.0, 0, 0)]);
        let r = compare(&a, &b, &opts()).unwrap();
        assert_eq!(r.differences[0].explained.unwrap().id, "market-zero");
    }

    #[test]
    fn a_tranche_the_baseline_should_have_absorbed_is_explained_with_its_zones() {
        let a = log(&[
            line(100.0, None, None, "deploy", "done",
                 "AUTO tranche on revx: $40.00 fresh USD + $0.00 rolled from unfilled zones -> 2 coins, regime CHOP"),
            line(100.0, Some("AAA"), Some("buy"), "deploy", "done",
                 "AAA revx [spot $2.00]: $20.00 @ $1.80 (-10%)"),
            run_line(100.0, 0, 0),
        ]);
        let b = log(&[run_line(100.0, 0, 0)]);
        let r = compare(&a, &b, &opts()).unwrap();
        assert_eq!(r.unexplained(), 0, "{:?}", r.lines());
        assert!(r
            .differences
            .iter()
            .all(|d| d.explained.unwrap().id == "tranche-baseline"));
        // A smaller reference tranche is not this bug.
        let c = log(&[
            line(100.0, None, None, "deploy", "done",
                 "AUTO tranche on revx: $50.00 fresh USD + $0.00 rolled from unfilled zones -> 2 coins, regime CHOP"),
            run_line(100.0, 0, 0),
        ]);
        let r = compare(&a.replace("$40.00 fresh", "$10.00 fresh"), &c, &opts()).unwrap();
        assert!(r.unexplained() > 0);
    }

    #[test]
    fn a_signal_held_back_by_a_stale_window_is_explained() {
        let a = log(&[
            line(
                50.0,
                Some("AAA"),
                Some("buy"),
                "ladder",
                "skip",
                "$1.00 USDT < min $3.00",
            ),
            run_line(50.0, 1, 0),
            run_line(100.0, 0, 0),
        ]);
        let b = log(&[
            line(
                100.0,
                Some("AAA"),
                Some("buy"),
                "ladder",
                "done",
                "BUY ~$10.00 filled",
            ),
            run_line(100.0, 1, 0),
        ]);
        let r = compare(
            &a,
            &b,
            &Options {
                since: Some(90.0),
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(r.unexplained(), 0, "{:?}", r.lines());
        let ids: Vec<&str> = r
            .differences
            .iter()
            .map(|d| d.explained.unwrap().id)
            .collect();
        assert_eq!(ids, ["rollback-window", "signals-follow"]);
    }

    #[test]
    fn renames_apply_to_the_reference_text() {
        let a = log(&[
            line(
                100.0,
                None,
                None,
                "hk",
                "err",
                "run old-bot --cancel to clear",
            ),
            run_line(100.0, 0, 0),
        ]);
        let b = a.replace("old-bot --cancel", "rungbot-exec deploy cancel");
        let o = Options {
            rename: parse_rename("old-bot --cancel=rungbot-exec deploy cancel").unwrap(),
            ..opts()
        };
        assert_eq!(compare(&a, &b, &o).unwrap().unexplained(), 0);
    }

    #[test]
    fn every_known_divergence_is_listed() {
        assert_eq!(known_lines().len(), KNOWN.len());
        for id in [
            "market-zero",
            "tranche-baseline",
            "coingecko-id",
            "divergence-dedupe",
            "regime-return",
            "atomic-state",
            "offline-switch",
            "rollback-window",
            "fillodds-partial",
            "audit-withdrawal",
        ] {
            assert!(KNOWN.iter().any(|k| k.id == id), "{id}");
        }
    }

    #[test]
    fn a_log_that_does_not_parse_is_an_error() {
        assert!(compare("{nope\n", "", &opts()).is_err());
    }
}
