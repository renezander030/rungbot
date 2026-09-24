//! The bear/chop screen: of the deeply dislocated coins, which have a business that
//! survives to the next bull and gets re-rated on fundamentals?
//!
//! * `build-index` — DefiLlama `/protocols` (TVL, category) joined to `/overview/fees`
//!   (30-day fees and revenue) on the lower-cased protocol name, keyed by ticker. When
//!   two protocols share a ticker the one with the higher TVL wins.
//! * `select` — **value first**: coins in the index earning at least $50k of fees in 30
//!   days, 40–92% off their high, with $500k of 24h volume; the ten deepest go to the
//!   ledger.
//! * `run` — for each of those, a 180-day web search for fundamentals (revenue,
//!   treasury, value accrual, whether the team still ships) and one LLM verdict line
//!   with a hard gate: real revenue now AND a token that captures it.

use crate::catalyst::{error_item, get_or, results, text_head};
use crate::py::{self, Json};
use crate::{Console, Ctx, ResearchError, Result};

pub const PROTOCOLS: &str = "https://api.llama.fi/protocols";
pub const FEES: &str = "https://api.llama.fi/overview/fees";
pub const TIMEOUT_S: u64 = 45;
/// Synthesize every selected candidate; select caps at the same number.
pub const TOP_N: usize = 10;
/// At or above this in 30-day fees (USD) counts as a real business.
pub const FEE_FLOOR: f64 = 50_000.0;
pub const MIN_DD: f64 = 40.0;
pub const MAX_DD: f64 = 92.0;
pub const MIN_VOL: f64 = 500_000.0;
pub const SEARCH_DAYS: i64 = 180;

pub const SYNTH: &str = "You are a BEAR-market value screen. Today is {today}. It is a bear market:
narrative and hype are worthless because the marginal buyer does not exist. The only
question: will {name} ({sym}) SURVIVE to the 2028 bull and be re-rated on FUNDAMENTALS?
It is down {dd}% from ATH (a cheap entry, nothing more).

HARD GATE — fail EITHER and the verdict is SPECULATIVE (no matter how good the story):
1. REAL economic activity NOW: earns real fees/revenue or has real paying usage during
   the bear (not token speculation). DefiLlama 30d fees: {fees}. TVL: {tvl}. Category: {cat}.
2. TOKEN VALUE-CAPTURE: the token actually accrues that value (fee burn/share, real-yield
   staking from real revenue, buybacks) — not a detached governance token.

If BOTH pass -> SURVIVOR, then score durability 0-100 using:
- Survivability: treasury/funding runway to ship 2+ more bear years; team active.
- Category durability: leader in infra that still matters in 2028 (settlement, DA, oracles,
  stablecoin/payment rails, real-demand DePIN, real-volume exchange token). Not a fad.
- Supply: dilution/unlocks mostly cleared by 2028.
Dead/abandoned/scam -> DEAD. Not enough evidence -> UNCLEAR.

EVIDENCE (revenue / treasury / tokenomics / team):
{ev}

Output ONLY the following single line — no preamble, no second line, no notes after it:
{sym} | real_revenue: <what+rough size, or NONE> | token_capture: <how, or NONE> | survives_2028: <yes/no/unclear> | durability: <0-100> | verdict: <SURVIVOR|SPECULATIVE|DEAD|UNCLEAR>";

fn lower_name(p: &Json) -> String {
    match p.get("name") {
        Some(n) if n.truthy() => py::strip(&n.display()).to_lowercase(),
        _ => String::new(),
    }
}

/// Build the value index from the two DefiLlama responses. Pure.
pub fn index_from(protos: &Json, fees: &Json) -> Json {
    let mut fee_by_name = Json::obj();
    if let Some(list) = fees.get("protocols").and_then(Json::as_arr) {
        for p in list {
            let n = lower_name(p);
            if !n.is_empty() {
                fee_by_name.set(
                    &n,
                    Json::Obj(vec![
                        (
                            "fees30d".into(),
                            p.get("total30d").cloned().unwrap_or(Json::Null),
                        ),
                        (
                            "revenue30d".into(),
                            p.get("revenue30d").cloned().unwrap_or(Json::Null),
                        ),
                    ]),
                );
            }
        }
    }
    let mut idx = Json::obj();
    for p in protos.as_arr().unwrap_or_default() {
        let sym = match p.get("symbol") {
            Some(s) if s.truthy() => s.display().to_uppercase(),
            _ => String::new(),
        };
        if sym.is_empty() || sym == "-" {
            continue;
        }
        let f = fee_by_name
            .get(&lower_name(p))
            .cloned()
            .unwrap_or(Json::obj());
        let raw = |k: &str| p.get(k).cloned().unwrap_or(Json::Null);
        let fee = |k: &str| f.get(k).cloned().unwrap_or(Json::Null);
        let rec = Json::Obj(vec![
            ("name".into(), raw("name")),
            ("tvl".into(), raw("tvl")),
            ("category".into(), raw("category")),
            ("gecko_id".into(), raw("gecko_id")),
            ("fees30d".into(), fee("fees30d")),
            ("revenue30d".into(), fee("revenue30d")),
        ]);
        let tvl = |r: &Json| py::or_zero(r.get("tvl")).as_f64().unwrap_or(0.0);
        let replace = match idx.get(&sym) {
            None => true,
            Some(prev) => tvl(&rec) > tvl(prev),
        };
        if replace {
            idx.set(&sym, rec);
        }
    }
    idx
}

pub fn build_index(ctx: &Ctx, con: &mut dyn Console) -> Result<()> {
    let get = |u: &str| {
        ctx.fetch
            .get_json(u, crate::UA_BROWSER_SCAN, TIMEOUT_S)
            .map_err(ResearchError::Failed)
    };
    let protos = get(PROTOCOLS)?;
    let fees = get(FEES)?;
    let idx = index_from(&protos, &fees);
    crate::write_json(&ctx.paths.value_index(), &idx)?;
    con.out(&format!(
        "value-index: {} tickers (TVL+fees+category) -> {}",
        idx.as_obj().map_or(0, <[_]>::len),
        crate::VALUE_INDEX
    ));
    Ok(())
}

fn read_index(ctx: &Ctx) -> Result<Json> {
    let path = ctx.paths.value_index();
    if !path.exists() {
        return Err(ResearchError::Exit(format!(
            "no {} — run: rungbot research survivor build-index",
            crate::VALUE_INDEX
        )));
    }
    crate::read_json(&path)
}

/// The value-first selection from the index and the ticker rows. Pure.
pub fn select_from(vidx: &Json, tickers: &[Json]) -> Vec<Json> {
    let mut tk = Json::obj();
    for c in tickers {
        let Some(sym) = c.get("symbol").and_then(Json::as_str) else {
            continue;
        };
        let q = match c.get("quotes") {
            Some(q) if q.truthy() => q.get("USD").cloned().unwrap_or(Json::obj()),
            _ => Json::obj(),
        };
        tk.set(sym, q);
    }
    let mut out = Vec::new();
    for (sym, v) in vidx.as_obj().unwrap_or_default() {
        let Some(q) = tk.get(sym).filter(|q| q.truthy()) else {
            continue;
        };
        let vol = py::or_zero(q.get("volume_24h"));
        let Some(fath) = q.get_some("percent_from_price_ath") else {
            continue;
        };
        if py::or_zero(v.get("fees30d")).as_f64().unwrap_or(0.0) < FEE_FLOOR {
            continue;
        }
        let dd = py::neg_json(fath);
        let dd_f = dd.as_f64().unwrap_or(0.0);
        if !(MIN_DD..=MAX_DD).contains(&dd_f) || vol.as_f64().unwrap_or(0.0) < MIN_VOL {
            continue;
        }
        let vget = |k: &str| v.get(k).cloned().unwrap_or(Json::Null);
        let name = match v.get("name") {
            Some(n) if n.truthy() => n.clone(),
            _ => Json::str(sym.as_str()),
        };
        out.push(Json::Obj(vec![
            ("symbol".into(), Json::str(sym.as_str())),
            ("name".into(), name),
            ("from_ath_pct".into(), py::round_json(fath, 1)),
            ("dislocation_score".into(), py::round_json(&dd, 1)),
            (
                "price".into(),
                q.get("price").cloned().unwrap_or(Json::Null),
            ),
            ("vol_24h".into(), py::round_json_int(&vol)),
            ("value_fees30d".into(), vget("fees30d")),
            ("value_tvl".into(), vget("tvl")),
            ("value_category".into(), vget("category")),
        ]));
    }
    let mut ranked: Vec<Json> = crate::by_dislocation(&out)
        .into_iter()
        .map(|i| out[i].clone())
        .collect();
    ranked.truncate(TOP_N);
    ranked
}

pub fn select(ctx: &Ctx, con: &mut dyn Console) -> Result<()> {
    let vidx = read_index(ctx)?;
    let rows = ctx
        .fetch
        .get_json(crate::oppscan::PAPRIKA, crate::UA_BROWSER_SCAN, TIMEOUT_S)
        .map_err(ResearchError::Failed)?;
    let out = select_from(&vidx, rows.as_arr().unwrap_or_default());
    crate::write_ledger(&ctx.paths, &out)?;
    con.out(&format!(
        "value-first: {} real-revenue + dislocated candidates",
        out.len()
    ));
    for c in &out {
        let n = |k: &str| py::or_zero(c.get(k)).as_f64().unwrap_or(0.0);
        con.out(&format!(
            "  {} {}%  fees30d=${}M tvl=${}M  {}",
            py::ljust(&c.get("symbol").map(Json::display).unwrap_or_default(), 8),
            py::rjust(
                &c.get("from_ath_pct").map(Json::display).unwrap_or_default(),
                5
            ),
            py::fixed(n("value_fees30d") / 1e6, 2),
            py::fixed(n("value_tvl") / 1e6, 0),
            c.get("value_category")
                .map(Json::display)
                .unwrap_or_default()
        ));
    }
    Ok(())
}

/// The search request body: 180 days back, dated before the contents block.
pub fn search_body(query: &str, ctx: &Ctx) -> String {
    py::dumps(&Json::Obj(vec![
        ("query".into(), Json::str(query)),
        ("numResults".into(), Json::Int(5)),
        (
            "startPublishedDate".into(),
            Json::str(ctx.today.minus_days(SEARCH_DAYS).search_start()),
        ),
        (
            "contents".into(),
            Json::Obj(vec![(
                "text".into(),
                Json::Obj(vec![("maxCharacters".into(), Json::Int(1200))]),
            )]),
        ),
    ]))
}

/// The prompt for one coin from its evidence and index record.
pub fn prompt_for(c: &Json, v: &Json, ev: &[Json], today: &str) -> String {
    let evtxt = ev
        .iter()
        .filter(|e| e.has("title"))
        .map(|e| {
            format!(
                "- ({}) {}: {}",
                py::head(&get_or(e, "published", "?").display(), 10),
                get_or(e, "title", "").display(),
                py::head(&get_or(e, "text", "").display(), 280)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let evtxt = if evtxt.is_empty() {
        "(no evidence)".to_string()
    } else {
        evtxt
    };
    let money = |k: &str, prec: usize| match v.get(k) {
        Some(x) if x.truthy() => format!("${}M", py::fixed(x.as_f64().unwrap_or(0.0) / 1e6, prec)),
        _ => "unknown/none".to_string(),
    };
    let cat = match v.get("category") {
        Some(x) if x.truthy() => x.display(),
        _ => "unknown".into(),
    };
    let sym = c.get("symbol").map(Json::display).unwrap_or_default();
    let name = c
        .get("name")
        .map(Json::display)
        .unwrap_or_else(|| sym.clone());
    let dd = py::neg_json(c.get("dislocation_score").unwrap_or(&Json::Int(0))).display();
    py::fill(
        SYNTH,
        &[
            ("today", today),
            ("name", &name),
            ("sym", &sym),
            ("dd", &dd),
            ("fees", &money("fees30d", 2)),
            ("tvl", &money("tvl", 1)),
            ("cat", &cat),
            ("ev", &evtxt),
        ],
    )
}

/// The verdict line out of the LLM's answer: the last line that has `verdict:` and a
/// `|`, else the last line, else nothing.
pub fn pick_line(raw: &str) -> String {
    let out = py::strip(raw);
    let lines = py::splitlines(out);
    lines
        .iter()
        .rev()
        .find(|l| l.to_lowercase().contains("verdict:") && l.contains('|'))
        .or(lines.last())
        .copied()
        .unwrap_or("")
        .to_string()
}

pub fn run_synth(ctx: &Ctx, con: &mut dyn Console) -> Result<()> {
    let llm = ctx.llm.ok_or_else(crate::no_llm)?;
    let vidx = read_index(ctx)?;
    let mut cands = crate::read_ledger(&ctx.paths)?;
    let today = ctx.today.to_string();
    for i in crate::by_dislocation(&cands).into_iter().take(TOP_N) {
        let c = &mut cands[i];
        let sym = c.get("symbol").map(Json::display).unwrap_or_default();
        let name = c
            .get("name")
            .map(Json::display)
            .unwrap_or_else(|| sym.clone());
        let v = vidx
            .get(&sym.to_uppercase())
            .cloned()
            .unwrap_or(Json::obj());
        for (k, from) in [
            ("value_fees30d", "fees30d"),
            ("value_tvl", "tvl"),
            ("value_category", "category"),
        ] {
            c.set(k, v.get(from).cloned().unwrap_or(Json::Null));
        }
        let q = format!(
            "{name} ({sym}) crypto protocol fundamentals: revenue and fees, treasury \
             runway and funding, token value accrual buyback fee burn staking yield, \
             is the team still shipping in 2026"
        );
        let ev: Vec<Json> = match ctx.search.search(&search_body(&q, ctx)) {
            Ok(resp) => results(&resp)
                .iter()
                .map(|x| {
                    Json::Obj(vec![
                        ("title".into(), get_or(x, "title", "")),
                        ("published".into(), get_or(x, "publishedDate", "")),
                        ("text".into(), text_head(x, 600)),
                    ])
                })
                .collect(),
            Err(e) => vec![error_item(e, "ValueError: EXA_API_KEY missing")],
        };
        c.set("survivor_evidence", Json::Arr(ev.clone()));
        let prompt = prompt_for(c, &v, &ev, &today);
        match llm.ask(&prompt) {
            Ok(raw) => {
                let line = pick_line(&raw);
                con.out(&line);
                c.set("survivor_verdict_line", Json::Str(line));
            }
            Err(e) => con.out(&format!("{sym} synth ERR: {e}")),
        }
    }
    crate::write_ledger(&ctx.paths, &cands)
}

/// `rungbot research survivor <build-index|select|run>`.
pub fn run(ctx: &Ctx, cmd: &str, con: &mut dyn Console) -> Result<()> {
    match cmd {
        "build-index" => build_index(ctx, con),
        "select" => select(ctx, con),
        "run" => run_synth(ctx, con),
        other => Err(ResearchError::Config(format!(
            "survivor: unknown step {other:?} (build-index | select | run)"
        ))),
    }
}
