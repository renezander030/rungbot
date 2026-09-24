//! The technical half: which coins inside a tradable band are furthest off their high.
//!
//! One keyless call to CoinPaprika's `/v1/tickers` for the whole market. A coin is kept
//! when it has an all-time-high figure, a rank inside `[min_rank, max_rank]` (a missing
//! rank counts as 0), 24h volume of at least `min_vol`, and a drawdown inside
//! `[min_dd, max_dd]`. The shortlist is ranked by drawdown alone and written to the
//! ledger, where the catalyst pass picks it up.
//!
//! `basing` (seven-day change above zero) is carried as context and deliberately not
//! rewarded. The 30-day change is not in the free feed.

use crate::py::{self, Json};
use crate::{Console, Ctx, ResearchError, Result};

pub const PAPRIKA: &str = "https://api.coinpaprika.com/v1/tickers";
pub const TIMEOUT_S: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Args {
    pub min_rank: i64,
    pub max_rank: i64,
    pub min_vol: i64,
    pub min_dd: f64,
    pub max_dd: f64,
    pub limit: usize,
    pub json: bool,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            min_rank: 40,
            max_rank: 600,
            min_vol: 1_000_000,
            min_dd: 40.0,
            max_dd: 90.0,
            limit: 8,
            json: false,
        }
    }
}

/// `quotes.USD` of a ticker row, or an empty object.
pub fn usd_quote(row: &Json) -> Json {
    match row.get_some("quotes").and_then(|q| q.get_some("USD")) {
        Some(q @ Json::Obj(_)) => q.clone(),
        _ => Json::obj(),
    }
}

/// Filter and rank the market. Pure.
pub fn scan(rows: &[Json], a: &Args) -> Result<Vec<Json>> {
    let mut out = Vec::new();
    for c in rows {
        let q = usd_quote(c);
        let rank = py::or_zero(c.get("rank"));
        let vol = py::or_zero(q.get("volume_24h"));
        let Some(fath) = q.get_some("percent_from_price_ath") else {
            continue;
        };
        let (Some(rank_f), Some(vol_f), Some(_)) = (rank.as_f64(), vol.as_f64(), fath.as_f64())
        else {
            continue;
        };
        if !(a.min_rank as f64 <= rank_f && rank_f <= a.max_rank as f64) || vol_f < a.min_vol as f64
        {
            continue;
        }
        let dd = py::neg_json(fath);
        let dd_f = dd.as_f64().unwrap_or(0.0);
        if !(a.min_dd <= dd_f && dd_f <= a.max_dd) {
            continue;
        }
        let chg = |k: &str| match q.get_some(k) {
            Some(v) => py::round_json(v, 1),
            None => Json::Null,
        };
        let basing = q
            .get_some("percent_change_7d")
            .and_then(Json::as_f64)
            .is_some_and(|d| d > 0.0);
        let field = |k: &str| {
            c.get(k)
                .cloned()
                .ok_or_else(|| ResearchError::Failed(format!("a ticker row has no {k:?}")))
        };
        out.push(Json::Obj(vec![
            ("symbol".into(), field("symbol")?),
            ("name".into(), field("name")?),
            ("rank".into(), rank),
            (
                "price".into(),
                q.get("price").cloned().unwrap_or(Json::Null),
            ),
            ("from_ath_pct".into(), py::round_json(fath, 1)),
            ("chg_24h".into(), chg("percent_change_24h")),
            ("chg_7d".into(), chg("percent_change_7d")),
            ("chg_1y".into(), chg("percent_change_1y")),
            ("basing".into(), Json::Bool(basing)),
            ("vol_24h".into(), py::round_json_int(&vol)),
            (
                "mcap".into(),
                py::round_json_int(&py::or_zero(q.get("market_cap"))),
            ),
            ("dislocation_score".into(), py::round_json(&dd, 1)),
            ("catalyst".into(), Json::Null),
            ("catalyst_confidence".into(), Json::Null),
        ]));
    }
    let order = crate::by_dislocation(&out);
    let mut ranked: Vec<Json> = order.into_iter().map(|i| out[i].clone()).collect();
    ranked.truncate(a.limit);
    Ok(ranked)
}

/// The table printed without `--json`.
pub fn human(cands: &[Json], con: &mut dyn Console) {
    con.out(&format!(
        "{:>2} {:<7}{:>5}{:>9}{:>7}{:>7}{:>8}{:>12}  BASING",
        "#", "COIN", "RANK", "FROM_ATH", "24h", "7d", "1y", "VOL_24H"
    ));
    for (i, c) in cands.iter().enumerate() {
        let s = |k: &str| c.get(k).map(Json::display).unwrap_or_default();
        let g = |k: &str| match c.get(k) {
            None | Some(Json::Null) => String::new(),
            Some(v) => v.display(),
        };
        let vol = c.get("vol_24h").and_then(Json::as_f64).unwrap_or(0.0);
        let v = format!("${}M", py::fixed(vol / 1e6, 1));
        let basing = c.get("basing").is_some_and(Json::truthy);
        con.out(&format!(
            "{:>2} {}{}{}%{}{}{}{}  {}",
            i + 1,
            py::ljust(&s("symbol"), 7),
            py::rjust(&s("rank"), 5),
            py::rjust(&s("from_ath_pct"), 8),
            py::rjust(&g("chg_24h"), 7),
            py::rjust(&g("chg_7d"), 7),
            py::rjust(&g("chg_1y"), 8),
            py::rjust(&v, 12),
            if basing { "yes" } else { "" }
        ));
    }
}

/// `rungbot research oppscan`: fetch, scan, write the ledger, print.
pub fn run(ctx: &Ctx, a: &Args, con: &mut dyn Console) -> Result<Vec<Json>> {
    let rows = ctx
        .fetch
        .get_json(PAPRIKA, crate::UA_SCAN, TIMEOUT_S)
        .map_err(ResearchError::Failed)?;
    let rows = rows
        .as_arr()
        .ok_or_else(|| ResearchError::Failed("coinpaprika: tickers is not a list".into()))?;
    let cands = scan(rows, a)?;
    crate::write_ledger(&ctx.paths, &cands)?;
    if a.json {
        con.out(&py::dumps_indent(&Json::Arr(cands.clone()), 2));
    } else {
        human(&cands, con);
        con.out(&format!(
            "\n{} candidates -> {} (awaiting catalyst pass)",
            cands.len(),
            crate::LEDGER
        ));
    }
    Ok(cands)
}
