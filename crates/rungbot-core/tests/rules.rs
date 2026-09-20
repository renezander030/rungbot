//! The five strategy rules, each pinned by the case that would break it.
//!
//! These are the tests that matter. Everything else in rungbot is plumbing around them.

use std::collections::BTreeMap;

use rungbot_core::{
    analyze, Bands, Coin, CoinState, Config, Outcome, Price, Settings, State, Trail, Venue,
};

const T0: f64 = 1_700_000_000.0;
const DAY: f64 = 86_400.0;

fn cfg_with(f: impl FnOnce(&mut Settings)) -> Config {
    let mut s = Settings {
        bands: Bands {
            first_pct: 10.0,
            step_pct: 5.0,
        },
        min_trade_pct: 1.0,
        min_core_pct: 20.0,
        window_hours: 24.0,
        buy_floor_pct: 50.0,
        target_pct: 10.0,
        breaker_pct: 40.0,
        breaker_days: 7.0,
        trail: Trail::Off,
        trail_giveback_pct: 5.0,
    };
    f(&mut s);
    Config::new(
        vec![Coin {
            symbol: "AAA".into(),
            venue: Venue::Binance,
            pair: "AAAUSDT".into(),
            name: "Alpha".into(),
            entry: Some(100.0),
            bands: None,
        }],
        s,
    )
    .unwrap()
}

fn cfg() -> Config {
    cfg_with(|_| {})
}

fn px(price: f64, chg: f64) -> BTreeMap<String, Price> {
    let mut m = BTreeMap::new();
    m.insert(
        "AAA".to_string(),
        Price {
            price,
            chg_24h: Some(chg),
        },
    );
    m
}

fn run(c: &Config, prices: &BTreeMap<String, Price>, st: &State, now: f64) -> Outcome {
    analyze(c, prices, st, now)
}

fn st_of(o: &Outcome) -> &CoinState {
    o.state.get("AAA").expect("AAA in state")
}

// --- Rule 1: a rung fires once; a retrace does not re-fire it ----------------------

#[test]
fn rule1_a_rung_fires_once_and_only_deeper_advances_it() {
    let c = cfg();
    let o1 = run(&c, &px(88.0, -12.0), &BTreeMap::new(), T0);
    assert_eq!(o1.buys.len(), 1, "the first dip fires");
    assert_eq!(o1.buys[0].rung, 1);
    assert_eq!(o1.buys[0].pct, 10.0, "a fresh rung 1 trades first_pct");

    let o2 = run(&c, &px(88.5, -11.5), &o1.state, T0 + 3600.0);
    assert!(o2.buys.is_empty(), "the same rung does not fire twice");

    let o3 = run(&c, &px(84.0, -16.0), &o2.state, T0 + 7200.0);
    assert_eq!(o3.buys.len(), 1, "a deeper rung advances the ladder");
    assert_eq!(o3.buys[0].pct, 5.0, "the deepening rung trades step_pct");
    assert_eq!(st_of(&o3).deployed_pct, 15.0, "deployed accumulates");
}

// --- Rule 2: the directional window locks out the opposite side -------------------

#[test]
fn rule2_the_window_locks_direction_then_re_arms() {
    let c = cfg();
    let bought = run(&c, &px(88.0, -12.0), &BTreeMap::new(), T0);
    assert_eq!(st_of(&bought).win_dir, "buy", "a buy opens a buy window");

    let locked = run(&c, &px(130.0, 2.0), &bought.state, T0 + 60.0);
    assert!(
        locked.sells.is_empty(),
        "no sell while a buy window is open"
    );

    let freed = run(&c, &px(130.0, 2.0), &bought.state, T0 + 25.0 * 3600.0);
    assert_eq!(
        freed.sells.len(),
        1,
        "the sell fires once the window closed"
    );

    let sold = run(&c, &px(130.0, 2.0), &BTreeMap::new(), T0);
    assert_eq!(st_of(&sold).win_dir, "sell");
    let buy_locked = run(&c, &px(80.0, -20.0), &sold.state, T0 + 60.0);
    assert!(
        buy_locked.buys.is_empty(),
        "no buy while a sell window is open"
    );
}

#[test]
fn rule2_a_coin_that_bought_this_run_does_not_also_sell() {
    // Deep in profit AND dipping 12% in the last 24h: the buy fires and must lock out
    // the sell in the same run.
    let c = cfg();
    let o = run(&c, &px(130.0, -12.0), &BTreeMap::new(), T0);
    assert_eq!(o.buys.len(), 1, "the dip buys");
    assert!(o.sells.is_empty(), "and the same run cannot also sell");
}

// --- Rule 3: the dynamic cap clamps cumulative buys --------------------------------

#[test]
fn rule3_the_cap_breathes_with_pnl_and_clamps_the_buy() {
    let c = cfg();
    // 60% underwater -> cap is 40% of base; 35% already deployed leaves room for 5.
    let mut deep: State = BTreeMap::new();
    deep.insert(
        "AAA".into(),
        CoinState {
            deployed_pct: 35.0,
            cost_basis: Some(100.0),
            ..Default::default()
        },
    );

    let o = run(&c, &px(40.0, -12.0), &deep, T0);
    assert_eq!(o.buys.len(), 1, "a capped buy still fires");
    assert!(
        (o.buys[0].pct - 5.0).abs() < 1e-9,
        "clamped to the remaining 5%"
    );
    assert!(o.buys[0].capped, "the capped flag is set");
    assert!(
        (st_of(&o).deployed_pct - 40.0).abs() < 1e-9,
        "deployed stops at the cap"
    );

    let mut at_cap: State = BTreeMap::new();
    at_cap.insert(
        "AAA".into(),
        CoinState {
            deployed_pct: 40.0,
            cost_basis: Some(100.0),
            ..Default::default()
        },
    );
    let o2 = run(&c, &px(40.0, -12.0), &at_cap, T0);
    assert!(o2.buys.is_empty(), "nothing fires once the cap is reached");
}

// --- Rule 4: the protected core is never sold ---------------------------------------

#[test]
fn rule4_sells_stop_at_the_protected_core() {
    let c = cfg();
    let mut mostly_sold: State = BTreeMap::new();
    mostly_sold.insert(
        "AAA".into(),
        CoinState {
            sold_pct: 75.0,
            cost_basis: Some(100.0),
            ..Default::default()
        },
    );

    let o = run(&c, &px(130.0, 2.0), &mostly_sold, T0);
    assert_eq!(o.sells.len(), 1, "a sell still fires near the core");
    assert_eq!(o.sells[0].pct, 5.0, "clamped to the last 5% above the core");
    assert_eq!(st_of(&o).sold_pct, 80.0, "sold stops at 100 - min_core_pct");

    let o2 = run(&c, &px(200.0, 2.0), &o.state, T0 + 25.0 * 3600.0);
    assert!(o2.sells.is_empty(), "nothing sells into the core");
}

// --- Rule 5: the circuit breaker freezes buys, never sells --------------------------

#[test]
fn rule5_the_breaker_arms_trips_alerts_once_and_clears() {
    let c = cfg();
    let d0 = run(&c, &px(50.0, -2.0), &BTreeMap::new(), T0);
    assert_eq!(
        st_of(&d0).below_since,
        T0,
        "the breaker arms on the first deep day"
    );
    assert!(!st_of(&d0).breaker, "it has not tripped on day 0");
    assert!(d0.errors.is_empty(), "no alert before it trips");

    let d8 = run(&c, &px(50.0, -20.0), &d0.state, T0 + 8.0 * DAY);
    assert!(st_of(&d8).breaker, "it trips after breaker_days");
    assert!(d8.buys.is_empty(), "no dip-buy while the breaker holds");
    assert!(
        d8.errors.iter().any(|e| e.contains("CIRCUIT BREAKER")),
        "it alerts when it trips"
    );

    let d9 = run(&c, &px(50.0, -20.0), &d8.state, T0 + 9.0 * DAY);
    assert!(d9.errors.is_empty(), "it does not re-alert every run");

    let ok = run(&c, &px(95.0, -2.0), &d9.state, T0 + 10.0 * DAY);
    assert!(!st_of(&ok).breaker, "it clears on recovery");
    assert_eq!(st_of(&ok).below_since, 0.0, "and the clock resets");
}

// --- the knife floor, dust, missing prices, no-basis coins --------------------------

#[test]
fn past_the_knife_floor_there_is_no_buy() {
    let o = run(&cfg(), &px(40.0, -60.0), &BTreeMap::new(), T0);
    assert!(o.buys.is_empty(), "some dips are not dips");
}

#[test]
fn a_coin_without_a_cost_basis_is_watched_for_dips_only() {
    let c = Config::new(
        vec![Coin {
            symbol: "AAA".into(),
            venue: Venue::Binance,
            pair: "AAAUSDT".into(),
            name: String::new(),
            entry: None,
            bands: None,
        }],
        Settings::default(),
    )
    .unwrap();
    let o = run(&c, &px(500.0, -12.0), &BTreeMap::new(), T0);
    assert_eq!(o.buys.len(), 1, "it still dip-buys");
    assert!(o.sells.is_empty(), "it never sells");
    assert_eq!(o.rows[0].pnl, None, "and reports no P&L");
}

#[test]
fn a_missing_price_is_an_error_and_leaves_that_ladder_untouched() {
    let o = run(&cfg(), &BTreeMap::new(), &BTreeMap::new(), T0);
    assert!(o.rows.is_empty(), "a coin with no price is not a row");
    assert_eq!(o.errors.len(), 1, "it is an error");
    assert!(o.state.is_empty(), "and its state is untouched");
}

#[test]
fn min_trade_pct_suppresses_meaningless_dust() {
    let c = cfg_with(|s| s.min_trade_pct = 8.0);
    let mut prior: State = BTreeMap::new();
    prior.insert(
        "AAA".into(),
        CoinState {
            buy: 1,
            deployed_pct: 10.0,
            win_until: T0 + DAY,
            win_dir: "buy".into(),
            cost_basis: Some(100.0),
            ..Default::default()
        },
    );
    let o = run(&c, &px(84.0, -16.0), &prior, T0);
    assert!(
        o.buys.is_empty(),
        "a 5% step under an 8% minimum is not emitted"
    );
}

// --- trailing take-profit -----------------------------------------------------------

#[test]
fn trailing_locks_rung_one_then_harvests_on_the_give_back() {
    let c = cfg_with(|s| {
        s.trail = Trail::On;
        s.trail_giveback_pct = 5.0;
    });

    let t1 = run(&c, &px(112.0, 2.0), &BTreeMap::new(), T0);
    assert_eq!(t1.sells.len(), 1, "rung 1 still locks in immediately");
    assert_eq!(st_of(&t1).sell, 1);

    let t2 = run(&c, &px(130.0, 2.0), &t1.state, T0 + 3600.0);
    assert!(
        t2.sells.is_empty(),
        "upper rungs are held while the move runs"
    );
    assert!(
        (st_of(&t2).peak_pnl - 30.0).abs() < 1e-9,
        "the peak is tracked"
    );

    let t3 = run(&c, &px(124.0, 2.0), &t2.state, T0 + 7200.0);
    assert_eq!(t3.sells.len(), 1, "the give-back harvests the run");
    assert_eq!(t3.sells[0].rung, 5, "up to the peak rung");
}

// --- config validation --------------------------------------------------------------

#[test]
fn config_refuses_what_a_human_must_fix() {
    let coin = |entry: Option<f64>| Coin {
        symbol: "AAA".into(),
        venue: Venue::Binance,
        pair: "AAAUSDT".into(),
        name: String::new(),
        entry,
        bands: None,
    };
    assert!(
        Config::new(vec![], Settings::default()).is_err(),
        "no coins"
    );
    assert!(
        Config::new(vec![coin(Some(-1.0))], Settings::default()).is_err(),
        "negative entry"
    );

    let s = Settings {
        min_core_pct: 100.0,
        ..Settings::default()
    };
    assert!(
        Config::new(vec![coin(None)], s).is_err(),
        "a 100% core would mean never selling anything"
    );

    assert!(Venue::parse("kraken").is_err(), "unknown venue");
    assert_eq!(
        Venue::parse("GATE").unwrap(),
        Venue::Gate,
        "venue parsing is case-insensitive"
    );
    assert_eq!(Trail::parse("off").unwrap(), Trail::Off);
    assert_eq!(
        Trail::parse("false").unwrap(),
        Trail::Off,
        "YAML 1.1 turns `off` into false"
    );
    assert!(Trail::parse("maybe").is_err());
}
