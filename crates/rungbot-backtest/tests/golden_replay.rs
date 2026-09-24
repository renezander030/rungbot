//! Cross-language parity for the historical replays.
//!
//! `tests/golden/replay.json` holds the reference implementation's console output and
//! every JSON document it wrote, run over the public candles in `tests/fixtures/candles/`
//! (plus a seeded synthetic Fear & Greed series) with neutral example rungs. Each test
//! requires the same bytes out of this crate, report and documents alike.

use std::path::PathBuf;

use rungbot_backtest::py::{loads, Py};
use rungbot_backtest::replay::alt_confirm::{self, AltCoin, LiveRungs};
use rungbot_backtest::replay::alt_top::{self, Ladder, Series};
use rungbot_backtest::replay::btc_confirm::{self, default_shapes, Btc};
use rungbot_backtest::replay::data::{self, Bar};
use rungbot_backtest::replay::sellpolicy_replay::{self, Closes};
use rungbot_backtest::replay::{btc_confirm_analysis, btc_top};

fn candles(name: &str) -> Py {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/candles")
        .join(name);
    loads(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{name}: {e}"))).unwrap()
}

fn golden() -> Py {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/replay.json");
    loads(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn want<'a>(g: &'a Py, study: &str) -> &'a Py {
    g.get(study)
        .unwrap_or_else(|| panic!("golden has no {study}"))
}

fn text<'a>(g: &'a Py, study: &str) -> &'a str {
    want(g, study)
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap()
}

fn file<'a>(g: &'a Py, study: &str, name: &str) -> &'a str {
    want(g, study)
        .get("files")
        .and_then(|f| f.get(name))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("{study}: no {name}"))
}

fn same(name: &str, want: &str, got: &str) {
    if want == got {
        return;
    }
    for (k, (a, b)) in want.lines().zip(got.lines()).enumerate() {
        if a != b {
            panic!("{name}: first difference at line {k}:\n  want: {a}\n   got: {b}");
        }
    }
    panic!(
        "{name}: outputs differ in length ({} vs {} lines)",
        want.lines().count(),
        got.lines().count()
    );
}

fn btc_bars() -> Vec<Bar> {
    data::ohlc(&candles("binance_BTCUSDT.json"))
}

fn source(src: &str) -> Vec<Bar> {
    let file = format!("{}.json", src.replace(':', "_"));
    data::ohlc(&candles(&file))
}

#[test]
fn btc_confirmation_replay_and_analysis_match_the_reference() {
    let g = golden();
    let btc = Btc::new(&btc_bars());
    let (out, docs) = btc_confirm::report(
        &btc,
        &["nobreadth", "btcbreadth"],
        &default_shapes(),
        "2026-08-10",
    );
    same("btc_replay stdout", text(&g, "btc_replay"), &out);
    for (v, doc) in &docs {
        let name = format!("replay_{v}.json");
        same(&name, file(&g, "btc_replay", &name), &doc.json(Some(1)));
    }
    let nb = &docs.iter().find(|(v, _)| v == "nobreadth").unwrap().1;
    let (out, doc) = btc_confirm_analysis::run(&btc, nb, 1000.0);
    same("btc_analysis stdout", text(&g, "btc_analysis"), &out);
    same(
        "analysis.json",
        file(&g, "btc_analysis", "analysis.json"),
        &doc.json(Some(1)),
    );
}

fn alt_coins() -> Vec<AltCoin> {
    let c = |sym: &str, src: &str, d: [f64; 3], w: [f64; 3], r1: (f64, f64)| AltCoin {
        sym: sym.into(),
        source: src.into(),
        depths: d,
        weights: w,
        rung1: Some(r1),
    };
    vec![
        c(
            "BTC",
            "binance:BTCUSDT",
            [4.0, 8.0, 14.0],
            [25.0, 35.0, 40.0],
            (60000.0, 65000.0),
        ),
        c(
            "FET",
            "binance:FETUSDT",
            [5.0, 10.0, 18.0],
            [30.0, 40.0, 30.0],
            (0.14, 0.157),
        ),
        c(
            "OSMO",
            "binance:OSMOUSDT",
            [2.0, 5.0, 9.0],
            [30.0, 40.0, 30.0],
            (0.03, 0.0348),
        ),
        c(
            "FETG",
            "gate:FET_USDT",
            [5.0, 10.0, 18.0],
            [30.0, 40.0, 30.0],
            (0.15, 0.157),
        ),
    ]
}

#[test]
fn alt_confirmation_study_sweep_and_current_read_match_the_reference() {
    let g = golden();
    let (out, results) = alt_confirm::run(&btc_bars(), &alt_coins(), &source);
    same(
        "alt_analyze stdout",
        text(&g, "alt_analyze"),
        &format!("{out}\nwrote <dir>/results.json\n"),
    );
    same(
        "alt results.json",
        file(&g, "alt_analyze", "results.json"),
        &results.json(Some(1)),
    );
    let sweep = alt_confirm::profile_sweep(&results, &alt_confirm::default_profiles());
    same("alt_sweep stdout", text(&g, "alt_sweep"), &sweep);

    let live = |sym: &str, src: &str, rungs: [f64; 3], spot: f64| LiveRungs {
        sym: sym.into(),
        source: src.into(),
        rungs: rungs.to_vec(),
        spot,
    };
    let coins = vec![
        live(
            "BTC",
            "binance:BTCUSDT",
            [72000.0, 68000.0, 64000.0],
            80000.0,
        ),
        live("FET", "binance:FETUSDT", [0.15, 0.14, 0.13], 0.157),
        live("OSMO", "binance:OSMOUSDT", [0.033, 0.03, 0.028], 0.0348),
        live("FETG", "gate:FET_USDT", [0.152, 0.146, 0.138], 0.157),
    ];
    let cur = alt_confirm::current(&coins, &source, "2026-08-18", "2026-09-01");
    same("alt_current stdout", text(&g, "alt_current"), &cur);
}

fn btc_top_inputs() -> btc_top::Inputs {
    btc_top::Inputs {
        binance: btc_bars()
            .into_iter()
            .filter(|b| b.date.as_str() < "2026-09-04")
            .map(|b| (b.date, b.close))
            .collect(),
        coinmetrics: data::coinmetrics(&candles("coinmetrics_btc_priceusd.json")),
        fng: data::fng(&candles("fng_history.json")),
    }
}

#[test]
fn btc_top_anatomy_matches_the_reference() {
    let g = golden();
    let (out, doc) = btc_top::run(&btc_top_inputs());
    same("btc_top stdout", text(&g, "btc_top"), &out);
    same(
        "btc_top results.json",
        file(&g, "btc_top", "results.json"),
        &doc.json(Some(1)),
    );
}

fn alt_series() -> (Series, Vec<(String, Series)>) {
    let btc = Series::merge(&[("binance".into(), btc_bars())]);
    let fet = Series::merge(&[("binance".into(), source("binance:FETUSDT"))]);
    let osmo = Series::merge(&[
        ("binance".into(), source("binance:OSMOUSDT")),
        (
            "llama".into(),
            data::px_bars(&candles("llama_osmosis.json")),
        ),
    ]);
    (btc, vec![("FET".into(), fet), ("OSMO".into(), osmo)])
}

#[test]
fn alt_top_anatomy_matches_the_reference() {
    let g = golden();
    let (btc, coins) = alt_series();
    let (out, doc) = alt_top::run(&Ladder::default(), &btc, &coins);
    same("alt_top stdout", text(&g, "alt_top"), &out);
    same(
        "alt_top results.json",
        file(&g, "alt_top", "results.json"),
        &doc.json(Some(1)),
    );
}

fn closes(series: &Series) -> Closes {
    Closes {
        d: series.d.clone(),
        c: series.c.clone(),
    }
}

#[test]
fn sell_policy_replay_matches_the_reference() {
    let g = golden();
    // The replay reads the two top studies' documents as the reference wrote them.
    let alt_results = loads(file(&g, "alt_top", "results.json")).unwrap();
    let btc_results = loads(file(&g, "btc_top", "results.json")).unwrap();
    let (_, coins) = alt_series();
    let alt = |sym: &str| {
        coins
            .iter()
            .find(|(s, _)| s == sym)
            .map(|(_, s)| closes(s))
            .unwrap_or_default()
    };
    // BTC: Coin Metrics, overlaid with Binance closes where present.
    let mut merged: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    for (d, v) in data::coinmetrics(&candles("coinmetrics_btc_priceusd.json")) {
        merged.insert(d, v);
    }
    for b in btc_bars() {
        merged.insert(b.date, b.close);
    }
    let btc = Closes {
        d: merged.keys().map(|d| data::day_num(d)).collect(),
        c: merged.values().copied().collect(),
    };
    let (out, doc) = sellpolicy_replay::run(&alt_results, &btc_results, &alt, &btc);
    same(
        "sellpolicy stdout",
        text(&g, "sellpolicy_replay"),
        &format!("{out}\nwritten <dir>/sellpolicy_replay.json\n"),
    );
    same(
        "sellpolicy_replay.json",
        file(&g, "sellpolicy_replay", "sellpolicy_replay.json"),
        &doc.json(Some(1)),
    );
}

mod microstate_feed {
    use super::*;
    use rungbot_backtest::replay::microstate::{self, Candle, Feed, Orders, Watch};

    struct Fixture(Py);

    impl Feed for Fixture {
        fn daily(&self, venue: &str, pair: &str) -> Result<Vec<Candle>, String> {
            let k = format!("{venue}:{pair}");
            let rows = self
                .0
                .get("daily")
                .and_then(|d| d.get(&k))
                .ok_or_else(|| format!("no fixture for {k}"))?;
            Ok(rows
                .as_list()
                .iter()
                .map(|r| {
                    let f = |x: &str| r.get(x).and_then(|v| v.as_f64()).unwrap();
                    Candle {
                        t: f("t") as i64,
                        o: f("o"),
                        h: f("h"),
                        l: f("l"),
                        c: f("c"),
                        qv: f("qv"),
                    }
                })
                .collect())
        }

        fn spot(&self, venue: &str, pair: &str) -> Result<f64, String> {
            let k = format!("{venue}:{pair}");
            self.0
                .get("spot")
                .and_then(|d| d.get(&k))
                .and_then(|v| v.as_f64())
                .ok_or_else(|| format!("no fixture spot for {k}"))
        }

        fn derivs(&self) -> Py {
            let d = self.0.get("derivs").unwrap();
            Py::Dict(
                d.items()
                    .iter()
                    .map(|(k, v)| {
                        let v = if k.as_str() == Some("fng") {
                            Py::List(
                                v.as_list()
                                    .iter()
                                    .map(|x| Py::Tuple(x.as_list().to_vec()))
                                    .collect(),
                            )
                        } else {
                            v.clone()
                        };
                        (k.clone(), v)
                    })
                    .collect(),
            )
        }
    }

    #[test]
    fn microstate_read_matches_the_reference() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
        let feed =
            loads(&std::fs::read_to_string(root.join("fixtures/microstate_feed.json")).unwrap())
                .unwrap();
        let g =
            loads(&std::fs::read_to_string(root.join("golden/microstate.json")).unwrap()).unwrap();
        let o = g.get("orders").unwrap();
        let pairs = |v: Option<&Py>| -> Vec<(Py, Py)> {
            v.map(|l| {
                l.as_list()
                    .iter()
                    .map(|p| (p.as_list()[0].clone(), p.as_list()[1].clone()))
                    .collect()
            })
            .unwrap_or_default()
        };
        let coins: Vec<Watch> = o
            .get("src")
            .unwrap()
            .items()
            .iter()
            .map(|(sym, vp)| {
                let s = sym.to_py_string();
                Watch {
                    venue: vp.as_list()[0].to_py_string(),
                    pair: vp.as_list()[1].to_py_string(),
                    orders: Orders {
                        rungs: pairs(o.get("rungs").and_then(|r| r.get(&s))),
                        sells: pairs(o.get("sells").and_then(|r| r.get(&s))),
                        cost: o.get("cost").and_then(|c| c.get(&s)).cloned().unwrap(),
                    },
                    sym: s,
                }
            })
            .collect();
        let (table, doc, notes) =
            microstate::run(&coins, "2026-09-04 08:30 UTC", &Fixture(feed)).unwrap();
        same(
            "microstate table",
            g.get("stdout").and_then(|v| v.as_str()).unwrap(),
            &table,
        );
        same(
            "microstate.json",
            g.get("microstate.json").and_then(|v| v.as_str()).unwrap(),
            &doc.json(Some(2)),
        );
        assert_eq!(notes, g.get("stderr").and_then(|v| v.as_str()).unwrap());
    }
}
