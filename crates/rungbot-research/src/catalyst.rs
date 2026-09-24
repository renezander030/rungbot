//! The bull screen: is there a forward catalyst that could reprice a dislocated coin?
//!
//! * `search` — for the six deepest-dislocated coins in the ledger, one web search for
//!   recent (60-day) evidence of any catalyst: an unlock, a listing, a mainnet launch, a
//!   partnership, regulatory news. The evidence is stored on the ledger record.
//! * `synthesize` — for every coin with evidence, the LLM command reads it against a
//!   strict rubric and answers with one verdict line:
//!   `SYM | catalyst: … | timing: … | direction: … | confidence: … | verdict: …`.
//!
//! Research only. A `REPRICE_REASON` verdict is something to read about.

use crate::py::{self, Json};
use crate::{Console, Ctx, ResearchError, Result, SearchError};

pub const RECENT_DAYS: i64 = 60;
pub const TOP_N: usize = 6;

pub const SYNTH_PROMPT: &str = "You are the catalyst step of a crypto dislocation screen. Today is {today}.
Coin: {name} ({sym}), down {dd}% from all-time high.
Below is recent web evidence. Make sense of it and assign ONE verdict. Be strict.

A catalyst counts as ACTIVE if it is dated in the future OR went live within the last
~14 days and is still being absorbed (e.g. a product/partnership that just launched and
is ramping). A one-off event older than ~2 weeks (a listing, a shipped upgrade) is PRICED IN.

VERDICT RUBRIC (apply in order):
- DEAD          : scam/MLM/defunct signals, or abandoned project.
- REPRICE_REASON: ALL of — direction is bull, AND an ACTIVE catalyst (future or <~14d old
                  and ramping), AND confidence >= 0.6.
- WATCH         : a plausible bullish catalyst that is ACTIVE but confidence < 0.6, OR
                  timing unclear, OR only partly absorbed.
- NO_CATALYST   : nothing ahead, OR the catalyst is PRICED IN (one-off >~2 weeks past),
                  OR direction is bear/mixed.

EVIDENCE:
{ev}

Reply with ONE line, no preamble:
{sym} | catalyst: <one sentence or NONE> | timing: <future date/window, or PAST, or NONE> | direction: <bull|bear|mixed> | confidence: <0.0-1.0> | verdict: <REPRICE_REASON|WATCH|NO_CATALYST|DEAD>";

/// The search request body: recent results, 1200 characters of text each.
pub fn search_body(query: &str, ctx: &Ctx) -> String {
    py::dumps(&Json::Obj(vec![
        ("query".into(), Json::str(query)),
        ("numResults".into(), Json::Int(5)),
        (
            "contents".into(),
            Json::Obj(vec![(
                "text".into(),
                Json::Obj(vec![("maxCharacters".into(), Json::Int(1200))]),
            )]),
        ),
        (
            "startPublishedDate".into(),
            Json::str(ctx.today.minus_days(RECENT_DAYS).search_start()),
        ),
    ]))
}

/// `x.get(key, default)`: a missing key gives the default, a `null` stays `null`.
pub(crate) fn get_or(x: &Json, key: &str, default: &str) -> Json {
    x.get(key).cloned().unwrap_or_else(|| Json::str(default))
}

/// `(x.get("text") or "")[:n]`.
pub(crate) fn text_head(x: &Json, n: usize) -> Json {
    match x.get("text") {
        Some(t) if t.truthy() => Json::Str(py::head(&t.display(), n)),
        _ => Json::str(""),
    }
}

/// The search response's `results`, one record per hit.
pub(crate) fn results(resp: &Json) -> Vec<Json> {
    resp.get("results")
        .and_then(Json::as_arr)
        .map(<[Json]>::to_vec)
        .unwrap_or_default()
}

pub fn search(ctx: &Ctx, con: &mut dyn Console) -> Result<()> {
    let mut cands = crate::read_ledger(&ctx.paths)?;
    for i in crate::by_dislocation(&cands).into_iter().take(TOP_N) {
        let c = &cands[i];
        let sym = c.get("symbol").map(Json::display).unwrap_or_default();
        let name = c
            .get("name")
            .map(Json::display)
            .unwrap_or_else(|| sym.clone());
        let q = format!(
            "{name} ({sym}) cryptocurrency upcoming catalyst 2026: token unlock, \
             exchange listing, mainnet launch, partnership, roadmap, or major news"
        );
        let ev = match ctx.search.search(&search_body(&q, ctx)) {
            Ok(resp) => results(&resp)
                .iter()
                .map(|x| {
                    Json::Obj(vec![
                        ("title".into(), get_or(x, "title", "")),
                        ("url".into(), get_or(x, "url", "")),
                        ("published".into(), get_or(x, "publishedDate", "")),
                        ("text".into(), text_head(x, 600)),
                    ])
                })
                .collect(),
            Err(e) => vec![error_item(e, "ValueError: EXA_API_KEY not found")],
        };
        let top = match ev.first() {
            Some(first) if first.has("title") => format!(
                " | top: {}",
                py::head(
                    &first.get("title").map(Json::display).unwrap_or_default(),
                    70
                )
            ),
            _ => String::new(),
        };
        con.out(&format!("{} {} hits{top}", py::ljust(&sym, 9), ev.len()));
        cands[i].set("catalyst_evidence", Json::Arr(ev));
    }
    crate::write_ledger(&ctx.paths, &cands)?;
    con.out(&format!(
        "\nevidence -> {}  (next: rungbot research catalyst synthesize)",
        crate::LEDGER
    ));
    Ok(())
}

pub(crate) fn error_item(e: SearchError, no_key: &str) -> Json {
    let msg = match e {
        SearchError::NoKey => no_key.to_string(),
        SearchError::Failed(m) => m,
    };
    Json::Obj(vec![("error".into(), Json::Str(msg))])
}

/// The prompt for one ledger record, or `None` when it has no usable evidence.
pub fn prompt_for(c: &Json, today: &str) -> Option<String> {
    let ev = c.get("catalyst_evidence").filter(|e| e.truthy())?;
    let items = ev.as_arr()?;
    if items.first().is_some_and(|f| f.has("error")) {
        return None;
    }
    let evtxt = items
        .iter()
        .map(|e| {
            format!(
                "- ({}) {}: {}",
                get_or(e, "published", "?").display(),
                get_or(e, "title", "").display(),
                py::head(&get_or(e, "text", "").display(), 300)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let sym = c.get("symbol").map(Json::display).unwrap_or_default();
    let name = c
        .get("name")
        .map(Json::display)
        .unwrap_or_else(|| sym.clone());
    let dd = py::neg_json(c.get("dislocation_score").unwrap_or(&Json::Int(0))).display();
    Some(py::fill(
        SYNTH_PROMPT,
        &[
            ("today", today),
            ("name", &name),
            ("sym", &sym),
            ("dd", &dd),
            ("ev", &evtxt),
        ],
    ))
}

pub fn synthesize(ctx: &Ctx, con: &mut dyn Console) -> Result<()> {
    let llm = ctx.llm.ok_or_else(crate::no_llm)?;
    let mut cands = crate::read_ledger(&ctx.paths)?;
    let today = ctx.today.to_string();
    for c in cands.iter_mut() {
        let Some(prompt) = prompt_for(c, &today) else {
            continue;
        };
        match llm.ask(&prompt) {
            Ok(raw) => {
                let out = py::strip(&raw);
                let line = py::splitlines(out)
                    .last()
                    .copied()
                    .unwrap_or("")
                    .to_string();
                con.out(&line);
                c.set("catalyst_verdict_line", Json::Str(line));
            }
            Err(e) => con.out(&format!(
                "{} synth ERR: {e}",
                c.get("symbol").map(Json::display).unwrap_or_default()
            )),
        }
    }
    crate::write_ledger(&ctx.paths, &cands)
}

/// `rungbot research catalyst <search|synthesize>`.
pub fn run(ctx: &Ctx, cmd: &str, con: &mut dyn Console) -> Result<()> {
    match cmd {
        "search" => search(ctx, con),
        "synthesize" => synthesize(ctx, con),
        other => Err(ResearchError::Config(format!(
            "catalyst: unknown step {other:?} (search | synthesize)"
        ))),
    }
}
