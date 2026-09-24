//! Scripted price paths through the live ladder, asserting its documented rules.
//!
//! No history and no money: a handful of `(price, 24h change)` steps at 30-minute
//! intervals, each followed by a check on what [`rungbot_core::analyze`] decided. This
//! proves the decision *logic* — the dip ladder, the high-water dedup, the directional
//! 24h lock, the knife floor, the protected core, the missing-data hold and the cost
//! basis seed. Profitability is the window backtest's question, not this one's.

use std::collections::BTreeMap;

use rungbot_core::{analyze, Coin, CoinState, Config, Price, Settings, State, Venue};

use crate::py::{round, Py};

const SYM: &str = "COIN";
const ENTRY: f64 = 0.70;
const HOUR: f64 = 3600.0;

fn config() -> Config {
    Config::new(
        vec![Coin {
            symbol: SYM.into(),
            venue: Venue::Coingecko,
            pair: "coin".into(),
            name: String::new(),
            entry: Some(ENTRY),
            bands: None,
        }],
        Settings::default(),
    )
    .expect("the scenario config is valid")
}

type Fired = Vec<(i64, f64)>;

fn step(
    cfg: &Config,
    state: &State,
    now: f64,
    price: f64,
    chg: Option<f64>,
) -> (State, Fired, Fired) {
    let mut prices = BTreeMap::new();
    prices.insert(
        SYM.to_string(),
        Price {
            price,
            chg_24h: chg,
        },
    );
    let out = analyze(cfg, &prices, state, now);
    let b = out.buys.iter().map(|t| (t.rung, round(t.pct, 1))).collect();
    let s = out
        .sells
        .iter()
        .map(|t| (t.rung, round(t.pct, 1)))
        .collect();
    (out.state, b, s)
}

/// The report, and whether every invariant held.
pub fn run() -> (String, bool) {
    let cfg = config();
    let mut out = String::new();
    let mut fails: Vec<String> = Vec::new();
    let mut check = |out: &mut String, name: &str, cond: bool| {
        out.push_str(&format!(
            "  {}  {name}\n",
            if cond { "PASS" } else { "FAIL" }
        ));
        if !cond {
            fails.push(name.to_string());
        }
    };
    let t = 1_000_000.0;
    let fresh = State::new;

    out.push_str("dip ladder + high-water dedup\n");
    let (st, b, _) = step(&cfg, &fresh(), t, 0.63, Some(-12.0));
    check(&mut out, "first dip -> rung1 buy 10%", b == vec![(1, 10.0)]);
    let (st, b, _) = step(&cfg, &st, t + 1800.0, 0.595, Some(-17.0));
    check(&mut out, "deeper dip -> rung2 buy 5%", b == vec![(2, 5.0)]);
    let (_, b, _) = step(&cfg, &st, t + 3600.0, 0.60, Some(-16.0));
    check(&mut out, "retrace -> no re-fire (high-water)", b.is_empty());

    out.push_str("directional 24h lock\n");
    let (st, _, _) = step(&cfg, &fresh(), t, 0.63, Some(-12.0));
    let (st, _, s) = step(&cfg, &st, t + 1800.0, 1.05, Some(3.0));
    check(&mut out, "sell frozen during buy window", s.is_empty());
    let (_, _, s) = step(&cfg, &st, t + 25.0 * HOUR, 1.05, Some(3.0));
    check(
        &mut out,
        "sell allowed after window closes",
        s.len() == 1 && s[0].0 >= 1,
    );

    out.push_str("-50% knife floor\n");
    let (_, b, _) = step(&cfg, &fresh(), t, 0.31, Some(-55.0));
    check(&mut out, "no buy past -50% 24h", b.is_empty());

    out.push_str("sell core floor (never sell all)\n");
    let (_, _, s) = step(&cfg, &fresh(), t, 1.40, Some(2.0));
    let sold_total = crate::py::sum(s.iter().map(|x| x.1));
    check(
        &mut out,
        "cumulative sell capped at 100-MIN_CORE",
        sold_total <= (100.0 - cfg.settings.min_core_pct) + 0.001,
    );

    out.push_str("missing 24h data holds state (no reset)\n");
    let mut st = State::new();
    st.insert(
        SYM.into(),
        CoinState {
            buy: 2,
            sell: 0,
            deployed_pct: 15.0,
            sold_pct: 0.0,
            win_until: t + 10.0 * HOUR,
            win_dir: "buy".into(),
            cost_basis: Some(ENTRY),
            ..CoinState::default()
        },
    );
    let (ns, b, _) = step(&cfg, &st, t, 0.63, None);
    check(
        &mut out,
        "None 24h change preserves buy rung",
        ns[SYM].buy == 2 && b.is_empty(),
    );

    out.push_str("cost basis seeds from ENTRIES\n");
    let (ns, _, _) = step(&cfg, &fresh(), t, 0.70, Some(-1.0));
    check(
        &mut out,
        "cost_basis seeded to entry 0.70",
        ns[SYM].cost_basis.is_some_and(|c| (c - 0.70).abs() < 1e-9),
    );

    out.push('\n');
    if fails.is_empty() {
        out.push_str("ALL INVARIANTS HOLD\n");
        (out, true)
    } else {
        let names = Py::List(fails.iter().map(|f| Py::from(f.as_str())).collect());
        out.push_str(&format!(
            "FAILED: {} invariant(s): {}\n",
            fails.len(),
            names.repr()
        ));
        (out, false)
    }
}
