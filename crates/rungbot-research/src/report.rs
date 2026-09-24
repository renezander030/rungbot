//! The weekly report: read the regime, run the matching thesis, write one dated note
//! and a short email.
//!
//! Without `--commit` it is a dry run: the steps still run (so the verdicts are fresh),
//! then the title, the email overview and the note are printed and nothing is sent.
//! With `--commit` the note goes to the configured [`NoteSink`] and the email, carrying
//! the overview and a link to the note, to the [`Mailer`].
//!
//! Research only: the note says so, and nothing here is wired to an order path.

use std::path::PathBuf;

use crate::py::{self, Json};
use crate::theses::{Step, Theses, Thesis};
use crate::{Console, Ctx, Result};

/// One coin's named regime signals, e.g. `("higher_lows", true)`.
pub type Signals = Vec<(String, bool)>;
/// Per coin, its signals, or `None` when the coin's regime read failed.
pub type CoinSignals = Vec<(String, Option<Signals>)>;

/// The market label and, per coin, the regime's basing signals (`None` when the coin's
/// regime read failed and it has no signals).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RegimeRead {
    pub market: String,
    pub coins: CoinSignals,
}

/// Where the full note goes.
pub trait NoteSink {
    /// Create the note. `Ok(None)` when the service answered without an id; `Err` is the
    /// failure text (logged as `note create failed <text>`).
    fn create(&self, title: &str, content: &str) -> std::result::Result<Option<String>, String>;
    /// A link a reader can open, for the email's button.
    fn link(&self, id: &str) -> Option<String>;
}

/// Sends the HTML email; the answer is a status word for the log (`sent`, ...).
pub trait Mailer {
    fn send(&self, subject: &str, html: &str) -> String;
}

/// Log lines: `[YYYY-MM-DDTHH:MM:SSZ] msg` to stdout and, when set, appended to a file.
pub struct Logger {
    pub stamp: Box<dyn Fn() -> String>,
    pub file: Option<PathBuf>,
}

impl Logger {
    pub fn system(file: Option<PathBuf>) -> Logger {
        Logger {
            stamp: Box::new(|| crate::date::utc_stamp(crate::date::now_epoch())),
            file,
        }
    }

    pub fn log(&self, con: &mut dyn Console, msg: &str) {
        let line = format!("[{}] {msg}", (self.stamp)());
        con.out(&line);
        if let Some(f) = &self.file {
            use std::io::Write;
            // A log that cannot be written must not stop the report.
            if let Ok(mut h) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(f)
            {
                let _ = writeln!(h, "{line}");
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Opts {
    pub commit: bool,
    pub refresh: bool,
    pub no_synth: bool,
    /// Forces the regime label; the basing signals still come from the regime read.
    pub regime: Option<String>,
}

/// What a run produced, for the caller and the tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub market: String,
    pub title: String,
    pub overview: String,
    pub note: String,
    pub top: usize,
    pub note_id: Option<String>,
    pub subject: Option<String>,
    pub html: Option<String>,
    pub email: Option<String>,
}

// ---------------------------------------------------------------------------------
// Pure pieces

/// `SYM | key: value | ...` → an ordered record: `raw`, `symbol`, each `key` lower-cased,
/// and `durability` as a float (0.0 when absent or not a number).
pub fn parse_line(line: &str) -> Json {
    let mut d = Json::Obj(vec![("raw".into(), Json::str(line))]);
    let parts: Vec<&str> = line.split('|').map(py::strip).collect();
    d.set("symbol", Json::str(py::strip(parts[0])));
    for p in &parts[1..] {
        if let Some((k, v)) = p.split_once(':') {
            d.set(&py::strip(k).to_lowercase(), Json::str(py::strip(v)));
        }
    }
    let dur = match d.get("durability") {
        Some(Json::Str(s)) if !s.is_empty() => py::parse_float(s).unwrap_or(0.0),
        Some(v) if v.truthy() => v.as_f64().unwrap_or(0.0),
        _ => 0.0,
    };
    d.set("durability", Json::Float(dur));
    d
}

/// The chop re-rank: `range_score = dislocation/100 × (1 + basing bonus)`, the bonus
/// being +0.5 higher lows, +0.3 above the 30-day SMA, +0.2 a fresh 30-day high.
/// Returns how many candidates had any regime signals.
pub fn attach_basing(cands: &mut [Json], coins: &[(String, Option<Signals>)]) -> usize {
    let mut hit = 0;
    for c in cands.iter_mut() {
        let sym = c.get("symbol").and_then(Json::as_str).unwrap_or_default();
        let sig: &[(String, bool)] = coins
            .iter()
            .find(|(s, _)| s == sym)
            .and_then(|(_, sig)| sig.as_deref())
            .unwrap_or_default();
        let on = |k: &str| sig.iter().any(|(n, v)| n == k && *v);
        let mut bonus = 0.0;
        if on("higher_lows") {
            bonus += 0.5;
        }
        if on("above_sma30") {
            bonus += 0.3;
        }
        if on("fresh_30d_high") {
            bonus += 0.2;
        }
        if !sig.is_empty() {
            hit += 1;
        }
        let disloc = py::or_zero(c.get("dislocation_score"))
            .as_f64()
            .unwrap_or(0.0)
            / 100.0;
        c.set(
            "range_score",
            Json::Float(py::round_f(disloc * (1.0 + bonus), 3)),
        );
    }
    hit
}

pub fn score_of(d: &Json, t: &Thesis) -> f64 {
    if t.score_key == "range" {
        return match d.get("range_score") {
            Some(v) if v.truthy() => v.as_f64().unwrap_or(0.0),
            _ => 0.0,
        };
    }
    match d.get(&t.score_key) {
        Some(v) if v.truthy() => match v {
            Json::Str(s) => py::parse_float(s).unwrap_or(0.0),
            other => other.as_f64().unwrap_or(0.0),
        },
        _ => 0.0,
    }
}

pub fn score_label(d: &Json, t: &Thesis) -> String {
    let s = score_of(d, t);
    match t.score_key.as_str() {
        "confidence" => format!("confidence {}", py::fixed(s, 2)),
        "range" => format!("range-score {}", py::fixed(s, 2)),
        _ => format!(
            "durability {}",
            if s.is_finite() { s.trunc() as i64 } else { 0 }
        ),
    }
}

/// Record keys shown in the row's context or score, never repeated as `key: value`.
const SKIP: [&str; 10] = [
    "symbol",
    "verdict",
    "raw",
    "from_ath",
    "cat",
    "fees30d",
    "tvl",
    "range_score",
    "durability",
    "confidence",
];

pub fn render_row(d: &Json, t: &Thesis) -> String {
    let mut ctx = Vec::new();
    if let Some(fa) = d.get_some("from_ath") {
        ctx.push(format!("{}% from ATH", fa.display()));
    }
    if let Some(cat) = d.get("cat").filter(|c| c.truthy()) {
        ctx.push(cat.display());
    }
    if let Some(fees) = d.get("fees30d").filter(|f| f.truthy()) {
        ctx.push(format!(
            "${}M/mo fees",
            py::fixed(fees.as_f64().unwrap_or(0.0) / 1e6, 1)
        ));
    }
    let ctxs = if ctx.is_empty() {
        String::new()
    } else {
        format!(" ({})", ctx.join(", "))
    };
    let kv: Vec<String> = d
        .as_obj()
        .unwrap_or_default()
        .iter()
        .filter(|(k, _)| !SKIP.contains(&k.as_str()))
        .filter_map(|(k, v)| match v {
            Json::Str(s) if !s.is_empty() => Some(format!("{}: {s}", k.replace('_', "-"))),
            _ => None,
        })
        .collect();
    let kv = if kv.is_empty() {
        String::new()
    } else {
        format!("{} ", kv.join(" · "))
    };
    format!(
        "- **{}**{ctxs} — {kv}_({})_",
        d.get("symbol").map(Json::display).unwrap_or_default(),
        score_label(d, t)
    )
}

/// Buckets in thesis order, each sorted by score, best first.
pub type Buckets = Vec<(String, Vec<Json>)>;

pub fn bucket(cands: &[Json], t: &Thesis) -> Buckets {
    let fallback = t.order.get(1).unwrap_or(&t.order[0]).clone();
    let mut by: Buckets = t.order.iter().map(|k| (k.clone(), Vec::new())).collect();
    for c in cands {
        let Some(line) = c.get(&t.verdict_field).filter(|l| l.truthy()) else {
            continue;
        };
        let Some(line) = line.as_str() else { continue };
        let mut d = parse_line(line);
        d.set(
            "from_ath",
            c.get("from_ath_pct").cloned().unwrap_or(Json::Null),
        );
        for (to, from) in [
            ("fees30d", "value_fees30d"),
            ("tvl", "value_tvl"),
            ("cat", "value_category"),
        ] {
            d.set(to, c.get(from).cloned().unwrap_or(Json::Null));
        }
        if let Some(rs) = c.get("range_score") {
            d.set("range_score", rs.clone());
        }
        let verdict = match d.get("verdict") {
            Some(v) if v.truthy() => v.display(),
            _ => fallback.clone(),
        };
        let v = py::split_ws(&verdict)
            .first()
            .map(|w| w.to_uppercase())
            .unwrap_or_else(|| fallback.clone());
        let key = if t.order.contains(&v) {
            v
        } else {
            fallback.clone()
        };
        if let Some((_, rows)) = by.iter_mut().find(|(k, _)| *k == key) {
            rows.push(d);
        }
    }
    for (_, rows) in by.iter_mut() {
        rows.sort_by(|a, b| {
            score_of(b, t)
                .partial_cmp(&score_of(a, t))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    by
}

pub fn build_note(
    cands: &[Json],
    today: &str,
    t: &Thesis,
    market: &str,
    regime_line: &str,
) -> (String, Buckets) {
    let by = bucket(cands, t);
    let mut md = vec![format!("# {today} {}", t.title), String::new()];
    md.extend(t.intro.iter().cloned());
    md.push(regime_line.replace("{market}", market));
    md.push(String::new());
    for (k, rows) in &by {
        if rows.is_empty() && *k != t.top_bucket {
            continue;
        }
        md.push(t.head(k).to_string());
        if rows.is_empty() {
            md.push("_None cleared the gate this run._".into());
        }
        for d in rows {
            md.push(render_row(d, t));
        }
        md.push(String::new());
    }
    md.push("## How this was built".into());
    md.push(t.how.clone());
    md.push(String::new());
    (md.join("\n"), by)
}

fn top_rows<'b>(by: &'b Buckets, t: &Thesis) -> &'b [Json] {
    by.iter()
        .find(|(k, _)| *k == t.top_bucket)
        .map(|(_, r)| r.as_slice())
        .unwrap_or_default()
}

pub fn build_overview(by: &Buckets, t: &Thesis) -> String {
    let top = top_rows(by, t);
    if top.is_empty() {
        return t.empty.clone();
    }
    let mut lines = vec![format!("Top {} candidate(s) this week:", t.noun)];
    for d in top {
        lines.push(format!(
            "- {} ({}% from ATH, {})",
            d.get("symbol").map(Json::display).unwrap_or_default(),
            d.get("from_ath")
                .map(Json::display)
                .unwrap_or_else(|| "None".into()),
            score_label(d, t)
        ));
    }
    lines.join("\n")
}

pub fn email_html(overview: &str, link: Option<&str>, title: &str, link_label: &str) -> String {
    let btn = match link {
        Some(l) => format!(
            "<a href=\"{l}\" style=\"background:#2d6cdf;color:#fff;padding:10px 18px;\
             border-radius:6px;text-decoration:none;display:inline-block\">{link_label}</a>"
        ),
        None => String::new(),
    };
    let body = overview.replace('\n', "<br>");
    format!(
        "<div style='font-family:system-ui,sans-serif;max-width:640px'>\
         <h2>🧭 {title}</h2><p>{body}</p><p>{btn}</p>\
         <p style='color:#888;font-size:12px'>Research-only. Never trades. \
         Free sources (CoinPaprika + exa).</p></div>"
    )
}

pub fn subject(t: &Thesis, top: usize, today: &str) -> String {
    format!("🧭 {} — {top} {}(s) — {today}", t.title, t.noun)
}

// ---------------------------------------------------------------------------------
// The run

pub struct Deps<'a> {
    pub theses: &'a Theses,
    pub regime: &'a dyn Fn() -> std::result::Result<RegimeRead, String>,
    /// Runs one pipeline step. A failing step is reported and the report carries on
    /// with whatever the ledger holds.
    pub step: &'a mut dyn FnMut(Step, &mut dyn Console),
    pub sink: Option<&'a dyn NoteSink>,
    pub mailer: Option<&'a dyn Mailer>,
    pub logger: &'a Logger,
    /// The email button's text.
    pub link_label: &'a str,
}

pub fn run(ctx: &Ctx, opts: &Opts, deps: Deps, con: &mut dyn Console) -> Result<Outcome> {
    let (mut market, coins) = match (deps.regime)() {
        Ok(r) => (r.market, r.coins),
        Err(e) => {
            deps.logger.log(
                con,
                &format!("regime read failed ({e}) — defaulting to bear thesis"),
            );
            ("bear".to_string(), Vec::new())
        }
    };
    if let Some(r) = &opts.regime {
        market = r.clone();
    }
    let t = deps.theses.for_regime(&market);
    deps.logger
        .log(con, &format!("regime={market} → thesis={}", t.title));

    if opts.refresh {
        for s in &t.refresh {
            (deps.step)(*s, con);
        }
    }
    if !opts.no_synth {
        for s in &t.synth {
            (deps.step)(*s, con);
        }
    }

    let mut cands = crate::read_ledger(&ctx.paths)?;
    if t.score_key == "range" {
        let hit = attach_basing(&mut cands, &coins);
        deps.logger.log(
            con,
            &format!(
                "chop re-rank: {hit}/{} candidates had a regime basing signal \
                 (rest scored on dislocation only)",
                cands.len()
            ),
        );
    }
    let today = ctx.today.to_string();
    let (note, by) = build_note(&cands, &today, t, &market, &deps.theses.regime_line);
    let overview = build_overview(&by, t);
    let title = format!("{today} {}", t.title);
    let top = top_rows(&by, t).len();
    let mut outcome = Outcome {
        market: market.clone(),
        title: title.clone(),
        overview: overview.clone(),
        note: note.clone(),
        top,
        ..Outcome::default()
    };

    if !opts.commit {
        con.out(&format!("=== TITLE ===\n{title}"));
        con.out(&format!("\n=== EMAIL OVERVIEW ===\n{overview}"));
        con.out(&format!("\n=== NOTE ===\n{note}"));
        con.out(&format!(
            "\n[dry-run] regime={market} {}s={top} — nothing written. Re-run with --commit.",
            t.noun
        ));
        return Ok(outcome);
    }

    let note_id = match deps.sink {
        Some(sink) => match sink.create(&title, &note) {
            Ok(id) => id,
            Err(e) => {
                deps.logger.log(con, &format!("note create failed {e}"));
                None
            }
        },
        None => None,
    };
    let link = match (deps.sink, &note_id) {
        (Some(sink), Some(id)) => sink.link(id),
        _ => None,
    };
    let subj = subject(t, top, &today);
    let html = email_html(&overview, link.as_deref(), &title, deps.link_label);
    let email = match deps.mailer {
        Some(m) => m.send(&subj, &html),
        None => "no email configured".to_string(),
    };
    let note_word = match (&note_id, deps.sink) {
        (Some(id), _) => id.clone(),
        (None, Some(_)) => "FAILED".to_string(),
        (None, None) => "off".to_string(),
    };
    deps.logger.log(
        con,
        &format!(
            "[committed] regime={market} note={note_word} email={email} {}s={top}",
            t.noun
        ),
    );
    outcome.note_id = note_id;
    outcome.subject = Some(subj);
    outcome.html = Some(html);
    outcome.email = Some(email);
    Ok(outcome)
}
