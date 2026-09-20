//! The opportunity screen: which deeply-dislocated coins have a business under them.
//!
//! Two halves, deliberately run in this order:
//!
//! * **Value first.** A protocol earning real fees now is a business. Starting from
//!   dislocation instead surfaces memecoins with nothing to screen — 90% off an all-time
//!   high means nothing if there was never anything there.
//! * **Then dislocation.** Of the coins that *could* pass the gate, keep the cheap ones.
//!   Both ends are bounded: a mild dip is not an opportunity, and past a certain depth
//!   you are in the graveyard rather than the bargain bin.
//!
//! The verdict this produces is **mechanical and research-only**. It says "this has
//! revenue and is deeply off its high", which is not the same as "buy this". It is never
//! wired to the ladder, and [`Verdict::Survivor`] is a prompt to go and read, not a
//! signal to act.
//!
//! Pure. Fetching the market and value data is the CLI's job.

use serde::{Deserialize, Serialize};

/// One coin as the market data describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub symbol: String,
    pub name: String,
    pub rank: u32,
    pub price: Option<f64>,
    /// Percent below the all-time high, as a positive magnitude.
    pub drawdown_pct: f64,
    pub vol_24h: f64,
    pub market_cap: Option<f64>,
    pub chg_24h: Option<f64>,
    pub chg_7d: Option<f64>,
    pub chg_1y: Option<f64>,
    /// Seven-day green. Shown as context and deliberately **not** rewarded in the rank.
    pub basing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<ValueFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
}

/// What a protocol actually earns, from a public fees/TVL source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueFacts {
    pub fees_30d: Option<f64>,
    pub revenue_30d: Option<f64>,
    pub tvl: Option<f64>,
    pub category: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Real fees now, and deeply dislocated. Worth reading about.
    Survivor,
    /// Passes one half of the gate. Needs a human.
    Unclear,
    /// No measurable revenue: whatever the story is, the market is the only buyer.
    Speculative,
    /// No revenue, no liquidity, or so far gone there is nothing to recover to.
    Dead,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Survivor => "SURVIVOR",
            Verdict::Unclear => "UNCLEAR",
            Verdict::Speculative => "SPECULATIVE",
            Verdict::Dead => "DEAD",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScreenConfig {
    /// Skip the mega-caps: they are too efficiently priced for this to find anything.
    pub min_rank: u32,
    pub max_rank: u32,
    pub min_vol_24h: f64,
    /// A mild dip is not a dislocation.
    pub min_drawdown_pct: f64,
    /// Past this it is the graveyard, not the bargain bin.
    pub max_drawdown_pct: f64,
    /// 30-day fees at or above this count as a real business.
    pub fee_floor_30d: f64,
    pub limit: usize,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        ScreenConfig {
            min_rank: 40,
            max_rank: 600,
            min_vol_24h: 500_000.0,
            min_drawdown_pct: 40.0,
            max_drawdown_pct: 92.0,
            fee_floor_30d: 50_000.0,
            limit: 10,
        }
    }
}

/// Filter to the tradable, dislocated band and rank by depth.
///
/// Ranking is on dislocation alone. `basing` is carried for context but not rewarded:
/// rewarding near-term green turns a value screen into a momentum screen, which is a
/// different thing with different failure modes.
pub fn screen(rows: &[Candidate], cfg: ScreenConfig) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = rows
        .iter()
        .filter(|c| (cfg.min_rank..=cfg.max_rank).contains(&c.rank))
        .filter(|c| c.vol_24h >= cfg.min_vol_24h)
        .filter(|c| (cfg.min_drawdown_pct..=cfg.max_drawdown_pct).contains(&c.drawdown_pct))
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        b.drawdown_pct
            .partial_cmp(&a.drawdown_pct)
            .unwrap_or(core::cmp::Ordering::Equal)
            // Stable and deterministic when two coins are equally dislocated.
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    out.truncate(cfg.limit);
    out
}

/// The mechanical half of the verdict: does this have a business under it?
///
/// An LLM pass can refine this afterwards, but the gate itself is arithmetic so that a
/// run with no model available still produces an auditable answer.
pub fn gate(c: &Candidate, cfg: ScreenConfig) -> Verdict {
    let fees = c.value.as_ref().and_then(|v| v.fees_30d).unwrap_or(0.0);
    let has_revenue = fees >= cfg.fee_floor_30d;
    let dislocated = c.drawdown_pct >= cfg.min_drawdown_pct;
    let tradable = c.vol_24h >= cfg.min_vol_24h;

    match (has_revenue, dislocated, tradable) {
        (_, _, false) => Verdict::Dead,
        (true, true, true) => Verdict::Survivor,
        (true, false, true) => Verdict::Unclear,
        (false, _, true) if c.drawdown_pct > cfg.max_drawdown_pct => Verdict::Dead,
        (false, _, true) => Verdict::Speculative,
    }
}

/// Screen, then attach value facts and a verdict to each survivor of the filter.
pub fn run(
    rows: &[Candidate],
    values: &dyn Fn(&str) -> Option<ValueFacts>,
    cfg: ScreenConfig,
) -> Vec<Candidate> {
    screen(rows, cfg)
        .into_iter()
        .map(|mut c| {
            c.value = values(&c.symbol);
            c.verdict = Some(gate(&c, cfg));
            c
        })
        .collect()
}

/// The facts an external model would need, as text. No model is called from this crate.
///
/// Emitting the prompt rather than calling a provider keeps rungbot free of an API key
/// and lets you pipe it to whatever you already use.
pub fn brief(c: &Candidate) -> String {
    let v = c.value.as_ref();
    let money = |x: Option<f64>| match x {
        Some(n) if n >= 1e6 => format!("${:.1}M", n / 1e6),
        Some(n) => format!("${n:.0}"),
        None => "unknown".into(),
    };
    format!(
        "{} ({}) rank {} · {:.0}% off its all-time high · 24h volume {} · \
         30d fees {} · TVL {} · category {} · mechanical verdict {}",
        c.symbol,
        c.name,
        c.rank,
        c.drawdown_pct,
        money(Some(c.vol_24h)),
        money(v.and_then(|x| x.fees_30d)),
        money(v.and_then(|x| x.tvl)),
        v.and_then(|x| x.category.clone())
            .unwrap_or_else(|| "unknown".into()),
        c.verdict.map(|x| x.as_str()).unwrap_or("-"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(sym: &str, rank: u32, dd: f64, vol: f64) -> Candidate {
        Candidate {
            symbol: sym.into(),
            name: sym.into(),
            rank,
            price: Some(1.0),
            drawdown_pct: dd,
            vol_24h: vol,
            market_cap: Some(1e8),
            chg_24h: Some(-1.0),
            chg_7d: Some(2.0),
            chg_1y: Some(-50.0),
            basing: true,
            value: None,
            verdict: None,
        }
    }

    fn with_fees(mut c: Candidate, fees: f64) -> Candidate {
        c.value = Some(ValueFacts {
            fees_30d: Some(fees),
            revenue_30d: Some(fees / 2.0),
            tvl: Some(2e8),
            category: Some("Dexes".into()),
        });
        c
    }

    #[test]
    fn mega_caps_and_micro_caps_are_both_out_of_band() {
        let cfg = ScreenConfig::default();
        let rows = vec![
            cand("BTC", 1, 50.0, 1e9),     // too big
            cand("MID", 100, 50.0, 1e7),   // in band
            cand("DUST", 5000, 50.0, 1e7), // too small
        ];
        let got = screen(&rows, cfg);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].symbol, "MID");
    }

    #[test]
    fn a_mild_dip_and_the_graveyard_are_both_excluded() {
        let cfg = ScreenConfig::default();
        let rows = vec![
            cand("MILD", 100, 10.0, 1e7),
            cand("GOOD", 100, 60.0, 1e7),
            cand("GONE", 100, 99.0, 1e7),
        ];
        let got = screen(&rows, cfg);
        let syms: Vec<&str> = got.iter().map(|c| c.symbol.as_str()).collect();
        assert_eq!(syms, vec!["GOOD"], "both ends are bounded on purpose");
    }

    #[test]
    fn illiquid_coins_are_excluded_however_cheap_they_look() {
        let cfg = ScreenConfig::default();
        let rows = vec![cand("THIN", 100, 80.0, 1_000.0)];
        assert!(
            screen(&rows, cfg).is_empty(),
            "you cannot trade what has no book"
        );
    }

    #[test]
    fn ranking_is_by_dislocation_and_deterministic_on_ties() {
        let cfg = ScreenConfig::default();
        let rows = vec![
            cand("BBB", 100, 60.0, 1e7),
            cand("CCC", 100, 80.0, 1e7),
            cand("AAA", 100, 60.0, 1e7),
        ];
        let got = screen(&rows, cfg);
        let syms: Vec<&str> = got.iter().map(|c| c.symbol.as_str()).collect();
        assert_eq!(
            syms,
            vec!["CCC", "AAA", "BBB"],
            "deepest first, then alphabetical"
        );
    }

    #[test]
    fn basing_is_carried_but_never_rewarded() {
        let cfg = ScreenConfig::default();
        let mut quiet = cand("QUIET", 100, 80.0, 1e7);
        quiet.basing = false;
        let rows = vec![cand("GREEN", 100, 60.0, 1e7), quiet];
        let got = screen(&rows, cfg);
        assert_eq!(
            got[0].symbol, "QUIET",
            "a deeper coin outranks a greener one"
        );
        assert!(!got[0].basing, "and its basing flag is still carried");
    }

    #[test]
    fn the_limit_is_honoured() {
        let cfg = ScreenConfig {
            limit: 2,
            ..Default::default()
        };
        let rows: Vec<Candidate> = (0..10)
            .map(|i| cand(&format!("C{i}"), 100, 50.0 + i as f64, 1e7))
            .collect();
        assert_eq!(screen(&rows, cfg).len(), 2);
    }

    #[test]
    fn revenue_plus_dislocation_is_the_only_route_to_survivor() {
        let cfg = ScreenConfig::default();
        let rich = with_fees(cand("RICH", 100, 60.0, 1e7), 1e6);
        assert_eq!(gate(&rich, cfg), Verdict::Survivor);

        let poor = cand("POOR", 100, 60.0, 1e7); // no value facts at all
        assert_eq!(
            gate(&poor, cfg),
            Verdict::Speculative,
            "a story is not revenue"
        );

        let expensive = with_fees(cand("EXP", 100, 5.0, 1e7), 1e6);
        assert_eq!(
            gate(&expensive, cfg),
            Verdict::Unclear,
            "real, but not cheap"
        );
    }

    #[test]
    fn fees_just_under_the_floor_do_not_count_as_a_business() {
        let cfg = ScreenConfig::default();
        let under = with_fees(cand("UNDER", 100, 60.0, 1e7), 49_999.0);
        assert_eq!(gate(&under, cfg), Verdict::Speculative);
        let over = with_fees(cand("OVER", 100, 60.0, 1e7), 50_000.0);
        assert_eq!(
            gate(&over, cfg),
            Verdict::Survivor,
            "the floor is inclusive"
        );
    }

    #[test]
    fn untradable_is_dead_regardless_of_revenue() {
        let cfg = ScreenConfig::default();
        let rich_but_thin = with_fees(cand("THIN", 100, 60.0, 100.0), 1e7);
        assert_eq!(gate(&rich_but_thin, cfg), Verdict::Dead);
    }

    #[test]
    fn run_attaches_values_and_verdicts() {
        let cfg = ScreenConfig::default();
        let rows = vec![cand("AAA", 100, 60.0, 1e7), cand("BBB", 100, 70.0, 1e7)];
        let lookup = |s: &str| {
            (s == "BBB").then(|| ValueFacts {
                fees_30d: Some(2e6),
                revenue_30d: Some(1e6),
                tvl: Some(5e8),
                category: Some("Lending".into()),
            })
        };
        let got = run(&rows, &lookup, cfg);
        assert_eq!(got[0].symbol, "BBB");
        assert_eq!(got[0].verdict, Some(Verdict::Survivor));
        assert_eq!(
            got[1].verdict,
            Some(Verdict::Speculative),
            "no value facts found"
        );
    }

    #[test]
    fn a_brief_states_the_facts_without_making_a_recommendation() {
        let cfg = ScreenConfig::default();
        let c = run(
            &[with_fees(cand("AAA", 100, 60.0, 1e7), 3e6)],
            &|_| None,
            cfg,
        );
        let b = brief(&c[0]);
        assert!(b.contains("AAA"), "{b}");
        assert!(b.contains("60% off its all-time high"), "{b}");
        assert!(
            !b.to_lowercase().contains("buy"),
            "a brief never says buy: {b}"
        );
    }
}
