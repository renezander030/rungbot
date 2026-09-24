//! When does a coin's supply overhang clear? The next token-unlock cliffs from
//! DefiLlama's open dataset CDN, no key needed.
//!
//! * `build-index` — map ticker → DefiLlama emissions slug, through the CoinGecko id each
//!   emissions record names.
//! * `enrich` — for every ledger coin, the unlock events of the next 180 days: date,
//!   tokens, share of total supply and USD at the ledger price. A single event of 3% of
//!   supply or more is an overhang worth waiting out.

use crate::py::{self, Json};
use crate::{Console, Ctx, ResearchError, Result};

pub const CDN: &str = "https://defillama-datasets.llama.fi";
pub const GECKO_LIST: &str = "https://api.coingecko.com/api/v3/coins/list";
pub const TIMEOUT_S: u64 = 60;
pub const HORIZON_DAYS: i64 = 180;
/// An unlock of at least this share of supply is an overhang.
pub const OVERHANG_PCT: f64 = 3.0;

fn get(ctx: &Ctx, url: &str) -> std::result::Result<Json, String> {
    ctx.fetch.get_json(url, crate::UA_BROWSER_SCAN, TIMEOUT_S)
}

pub fn build_index(ctx: &Ctx, con: &mut dyn Console) -> Result<Json> {
    let slugs =
        get(ctx, &format!("{CDN}/emissionsProtocolsList")).map_err(ResearchError::Failed)?;
    let gecko = get(ctx, GECKO_LIST).map_err(ResearchError::Failed)?;
    let mut gid2sym = Json::obj();
    for c in gecko.as_arr().unwrap_or_default() {
        if let Some(id) = c.get("id").and_then(Json::as_str) {
            gid2sym.set(id, c.get("symbol").cloned().unwrap_or(Json::Null));
        }
    }
    let slugs = slugs.as_arr().unwrap_or_default();
    let (mut idx, mut skipped) = (Json::obj(), 0usize);
    for (i, slug) in slugs.iter().enumerate() {
        let slug_s = slug.display();
        let md = match get(ctx, &format!("{CDN}/emissions/{slug_s}")) {
            Ok(d @ Json::Obj(_)) => d.get("metadata").cloned().unwrap_or(Json::obj()),
            _ => {
                skipped += 1;
                continue;
            }
        };
        let tok = match md.get("token") {
            Some(t) if t.truthy() => t.display(),
            _ => String::new(),
        };
        let gid = tok.split_once(':').map(|(_, g)| g.to_string());
        let sym = gid.as_deref().and_then(|g| gid2sym.get(g));
        if let (Some(sym), Some(gid)) = (sym.filter(|s| s.truthy()), gid.as_deref()) {
            idx.set(
                &sym.display().to_uppercase(),
                Json::Obj(vec![
                    ("slug".into(), slug.clone()),
                    ("gecko_id".into(), Json::str(gid)),
                ]),
            );
        }
        if i % 50 == 0 {
            con.err(&format!("  ...{i}/{}", slugs.len()));
        }
    }
    crate::write_json(&ctx.paths.unlock_index(), &idx)?;
    con.out(&format!(
        "index: {} symbols mapped ({skipped} slugs skipped) -> {}",
        idx.as_obj().map_or(0, <[_]>::len),
        crate::UNLOCK_INDEX
    ));
    Ok(idx)
}

/// A number that keeps its integer-ness through addition.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Num {
    I(i128),
    F(f64),
}

impl Num {
    fn of(v: &Json) -> Num {
        match v {
            Json::Int(i) => Num::I(*i),
            Json::Bool(b) => Num::I(i128::from(*b)),
            other => Num::F(other.as_f64().unwrap_or(0.0)),
        }
    }
    fn add(self, o: Num) -> Num {
        match (self, o) {
            (Num::I(a), Num::I(b)) => Num::I(a + b),
            (a, b) => Num::F(a.f() + b.f()),
        }
    }
    fn f(self) -> f64 {
        match self {
            Num::I(i) => i as f64,
            Num::F(f) => f,
        }
    }
}

/// Tokens released by one event: the summary totals when present, else the sum of its
/// cliff allocations.
fn tokens(ev: &Json) -> Num {
    let s = ev
        .get("summary")
        .filter(|s| s.truthy())
        .cloned()
        .unwrap_or(Json::obj());
    if s.has("totalTokensCliff") || s.has("totalLinear") {
        return Num::of(&py::or_zero(s.get("totalTokensCliff")))
            .add(Num::of(&py::or_zero(s.get("totalLinear"))));
    }
    let mut sum = Num::I(0);
    for a in ev
        .get("cliffAllocations")
        .and_then(Json::as_arr)
        .unwrap_or_default()
    {
        sum = sum.add(Num::of(&py::or_zero(a.get("amount"))));
    }
    sum
}

/// The unlock events of one emissions record inside `(now, now + horizon]`. Pure.
pub fn upcoming_from(record: &Json, now: i64, price: Option<&Json>) -> Vec<Json> {
    let md = record.get("metadata").cloned().unwrap_or(Json::obj());
    let total = py::or_zero(md.get("total"));
    let end = now + HORIZON_DAYS * 86_400;
    let price = price.filter(|p| p.truthy());
    let mut out = Vec::new();
    for ev in md
        .get("unlockEvents")
        .filter(|e| e.truthy())
        .and_then(Json::as_arr)
        .unwrap_or_default()
    {
        let ts = py::or_zero(ev.get("timestamp")).as_f64().unwrap_or(0.0);
        if !((now as f64) < ts && ts <= end as f64) {
            continue;
        }
        let tok = tokens(ev);
        if tok.f() <= 0.0 {
            continue;
        }
        let tokens_rounded = match tok {
            Num::I(i) => Json::Int(i),
            Num::F(f) => Json::Int(py::round_i(f)),
        };
        let pct = if total.truthy() {
            Json::Float(py::round_f(
                tok.f() / total.as_f64().unwrap_or(1.0) * 100.0,
                2,
            ))
        } else {
            Json::Null
        };
        let usd = match price {
            Some(p) => match (tok, p) {
                (Num::I(t), Json::Int(pi)) => Json::Int(t * pi),
                _ => Json::Int(py::round_i(tok.f() * p.as_f64().unwrap_or(0.0))),
            },
            None => Json::Null,
        };
        out.push(Json::Obj(vec![
            (
                "date".into(),
                Json::Str(crate::date::Date::from_epoch_utc(ts).to_string()),
            ),
            ("tokens".into(), tokens_rounded),
            ("pct_supply".into(), pct),
            ("usd".into(), usd),
        ]));
    }
    out.sort_by(|a, b| {
        let d = |x: &Json| x.get("date").map(Json::display).unwrap_or_default();
        d(a).cmp(&d(b))
    });
    out
}

/// The status line for a coin's upcoming events.
pub fn status(evs: &[Json]) -> String {
    let big = evs
        .iter()
        .find(|e| py::or_zero(e.get("pct_supply")).as_f64().unwrap_or(0.0) >= OVERHANG_PCT);
    let field = |e: &Json, k: &str| e.get(k).map(Json::display).unwrap_or_default();
    if let Some(b) = big {
        format!(
            "OVERHANG: {}% on {} — wait",
            field(b, "pct_supply"),
            field(b, "date")
        )
    } else if let Some(first) = evs.first() {
        format!("minor cliffs only (next {})", field(first, "date"))
    } else {
        "CLEAN: no cliffs in horizon — overhang cleared".into()
    }
}

pub fn enrich(ctx: &Ctx, now: i64, con: &mut dyn Console) -> Result<()> {
    let path = ctx.paths.unlock_index();
    if !path.exists() {
        return Err(ResearchError::Exit(format!(
            "no {} — run: rungbot research unlocks build-index",
            crate::UNLOCK_INDEX
        )));
    }
    let idx = crate::read_json(&path)?;
    let mut cands = crate::read_ledger(&ctx.paths)?;
    for c in cands.iter_mut() {
        let sym = c.get("symbol").map(Json::display).unwrap_or_default();
        let Some(m) = idx.get(&sym.to_uppercase()).filter(|m| m.truthy()) else {
            c.set("unlock_status", Json::str("no_unlock_data"));
            continue;
        };
        let slug = m.get("slug").cloned().unwrap_or(Json::Null);
        let record = get(ctx, &format!("{CDN}/emissions/{}", slug.display()))
            .map_err(ResearchError::Failed)?;
        let evs = upcoming_from(&record, now, c.get("price"));
        c.set("unlock_slug", slug);
        c.set(
            "next_unlocks",
            Json::Arr(evs.iter().take(3).cloned().collect()),
        );
        c.set("unlock_status", Json::Str(status(&evs)));
    }
    crate::write_ledger(&ctx.paths, &cands)?;
    for c in &cands {
        let u = c
            .get("unlock_status")
            .map(Json::display)
            .unwrap_or_default();
        let nx = c
            .get("next_unlocks")
            .and_then(Json::as_arr)
            .unwrap_or_default();
        let tag = match nx.first() {
            Some(n) => format!(
                " next={} {}%/${}M",
                n.get("date").map(Json::display).unwrap_or_default(),
                n.get("pct_supply").map(Json::display).unwrap_or_default(),
                py::fixed(py::or_zero(n.get("usd")).as_f64().unwrap_or(0.0) / 1e6, 1)
            ),
            None => String::new(),
        };
        con.out(&format!(
            "{} {}%  {u}{tag}",
            py::ljust(&c.get("symbol").map(Json::display).unwrap_or_default(), 9),
            py::rjust(
                &c.get("from_ath_pct").map(Json::display).unwrap_or_default(),
                7
            )
        ));
    }
    Ok(())
}

/// `rungbot research unlocks <build-index|enrich>`.
pub fn run(ctx: &Ctx, cmd: &str, now: i64, con: &mut dyn Console) -> Result<()> {
    match cmd {
        "build-index" => build_index(ctx, con).map(|_| ()),
        "enrich" => enrich(ctx, now, con),
        other => Err(ResearchError::Config(format!(
            "unlocks: unknown step {other:?} (build-index | enrich)"
        ))),
    }
}
