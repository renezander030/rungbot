//! Cross-language parity for a live run's housekeeping, replayed from frozen reference
//! outputs.
//!
//! * `housekeeping.json`: multi-run scenarios. Each run starts from the previous run's
//!   journal, ladder state, stale-order flags and P&L ledger, drives [`housekeeping::run`]
//!   against scripted venues, and must reproduce the reference's result lines (text
//!   included), the orders it sent, and all four stores afterwards.
//! * `housekeeping_pure.json`: the paired-sell price, the cost basis, the P&L record and
//!   the regime TTL on their own.
//! * `sellcheck.json`: the sellability check and its summary line.
//! * `cex-dir-state/`: run-state files written by the reference, imported.
//!
//! Comparison rules as in `golden.rs`: numbers compare by value, and in objects a `null`
//! equals an absent key.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

use rungbot_core::housekeeping::{self as hk, Fees};
use rungbot_exec::housekeeping::{
    self, Balances, Books, LadderState, PnlLedger, Route, RunCtx, Settings, TtlWarned,
};
use rungbot_exec::http::VenueError;
use rungbot_exec::journal::Journal;
use rungbot_exec::reconcile::VenueSource;
use rungbot_exec::sellcheck::{self, SellCheck};
use rungbot_exec::venue::{Balance, Limits, ParsedOrder, Venue};
use rungbot_exec::{import, pyfmt, store};
use serde_json::{json, Map, Value};

fn golden(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).expect("golden file")).expect("json")
}

fn same(a: &Value, b: &Value, at: &str) -> Result<(), String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x
                .iter()
                .chain(y.iter())
                .filter(|(_, v)| !v.is_null())
                .map(|(k, _)| k)
                .collect();
            for k in keys {
                same(
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                    &format!("{at}.{k}"),
                )?;
            }
            Ok(())
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .try_for_each(|(i, (p, q))| same(p, q, &format!("{at}[{i}]"))),
        (Value::Number(x), Value::Number(y)) if x.as_f64() == y.as_f64() => Ok(()),
        (x, y) if x == y => Ok(()),
        (x, y) => Err(format!("{at}:\n    reference {x}\n    ours      {y}")),
    }
}

fn check(failures: &mut Vec<String>, r: Result<(), String>) {
    if let Err(e) = r {
        failures.push(e);
    }
}

fn finish(what: &str, failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{what}: {} mismatch(es):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn n(v: &Value) -> f64 {
    v.as_f64().unwrap()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

// ------------------------------------------------------------------ scripted venues

type Calls = Rc<RefCell<Vec<Value>>>;

struct Fake {
    name: &'static str,
    spec: Value,
    calls: Calls,
    seen: RefCell<BTreeMap<String, usize>>,
}

impl Fake {
    fn err(&self, msg: &Value) -> VenueError {
        VenueError::new(self.name, None, s(msg))
    }

    fn raised(&self, v: &Value) -> Result<(), VenueError> {
        match v.get("raise") {
            Some(m) => Err(self.err(m)),
            None => Ok(()),
        }
    }
}

impl Venue for Fake {
    fn name(&self) -> &'static str {
        self.name
    }
    fn parse_order(&self, raw: &Value) -> Result<ParsedOrder, VenueError> {
        Ok(serde_json::from_value(raw.clone()).unwrap())
    }
    fn order_status(&self, _pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        let mut v = &self.spec["status"][order_id];
        if v.is_null() {
            return Err(VenueError::new(
                self.name,
                None,
                format!("no scripted status for {order_id}"),
            ));
        }
        if let Some(seq) = v.get("seq").and_then(Value::as_array) {
            let mut seen = self.seen.borrow_mut();
            let k = seen.entry(order_id.to_string()).or_insert(0);
            v = &seq[(*k).min(seq.len() - 1)];
            *k += 1;
        }
        self.raised(v)?;
        Ok(serde_json::from_value(v.clone()).unwrap())
    }
    fn open_orders(&self, pair: &str) -> Result<Vec<ParsedOrder>, VenueError> {
        let v = &self.spec["open"][pair];
        self.raised(v)?;
        Ok(serde_json::from_value(if v.is_null() { json!([]) } else { v.clone() }).unwrap())
    }
    fn cancel(&self, pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        self.calls.borrow_mut().push(
            json!({"venue": self.name, "call": "cancel", "pair": pair, "order_id": order_id}),
        );
        self.raised(&self.spec["cancel"][order_id])?;
        Ok(ParsedOrder::default())
    }
    fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError> {
        unimplemented!()
    }
    fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError> {
        let v = &self.spec["balances"];
        self.raised(v)?;
        Ok(serde_json::from_value(if v.is_null() { json!({}) } else { v.clone() }).unwrap())
    }
    fn price(&self, pair: &str) -> Result<f64, VenueError> {
        let v = &self.spec["price"][pair];
        if v.is_null() {
            return Err(VenueError::new(
                self.name,
                None,
                format!("no price for {pair}"),
            ));
        }
        self.raised(v)?;
        Ok(n(v))
    }
    fn limits(&self, _: &str) -> Result<Limits, VenueError> {
        unimplemented!()
    }
    fn round_amount(&self, _: &str, _: f64) -> Result<f64, VenueError> {
        unimplemented!()
    }
    fn round_price(&self, _: &str, _: f64) -> Result<f64, VenueError> {
        unimplemented!()
    }
    fn market_buy(&self, _: &str, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        unimplemented!()
    }
    fn market_sell(&self, _: &str, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        unimplemented!()
    }
    fn limit_buy(&self, _: &str, _: f64, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        unimplemented!()
    }
    fn limit_sell(
        &self,
        pair: &str,
        base: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        let cid = client_id.unwrap_or("");
        self.calls
            .borrow_mut()
            .push(json!({"venue": self.name, "call": "limit_sell",
            "pair": pair, "base": base, "price": price, "client_id": cid}));
        let v = &self.spec["limit_sell"][cid];
        if v.is_null() {
            return Ok(json!({"order_id": format!("L-{cid}"), "status": "open"}));
        }
        self.raised(v)?;
        Ok(v.clone())
    }
}

struct Fakes(BTreeMap<String, Fake>);

impl VenueSource for Fakes {
    fn venue(&self, exch: &str) -> Result<&dyn Venue, String> {
        self.0
            .get(exch)
            .map(|v| v as &dyn Venue)
            .ok_or_else(|| pyfmt::repr_str(exch))
    }
}

fn settings(g: &Value) -> Settings {
    Settings {
        target_pct: n(&g["target_pct"]),
        fees: Fees {
            gate: n(&g["fee_pct_gate"]),
            revx: n(&g["fee_pct_revx"]),
            binance: n(&g["fee_pct_binance"]),
        },
        window_hours: n(&g["window_hours"]),
        limit_ttl_days: n(&g["limit_ttl_days"]),
        limit_ttl_days_bear: n(&g["limit_ttl_days_bear"]),
        limit_ttl_days_chop: n(&g["limit_ttl_days_chop"]),
        ttl_renag_days: n(&g["ttl_renag_days"]),
        routing: g["routing"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| Route {
                sym: s(&r[0]).into(),
                exch: s(&r[1]).into(),
                pair: s(&r[2]).into(),
                quote: s(&r[3]).into(),
            })
            .collect(),
        entries: g["entries"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), n(v)))
            .collect(),
    }
}

fn set_of(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .map(|a| a.iter().map(|x| s(x).to_string()).collect())
        .unwrap_or_default()
}

fn jv<T: serde::Serialize>(x: &T) -> Value {
    serde_json::to_value(x).unwrap()
}

// ------------------------------------------------------------------ the scenarios

#[test]
fn housekeeping_matches_the_reference_run_after_run() {
    let g = golden("housekeeping.json");
    let mut f = Vec::new();
    let scenarios = g["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 6);
    for sc in scenarios {
        let name = s(&sc["name"]);
        let st = settings(&sc["settings"]);
        let init = &sc["initial"];
        // A parsed JSON object is sorted here; the reference's row order is behaviour.
        let rows = &init["journal"];
        let mut journal: Journal = store::journal_from_rows(
            init["journal_order"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| (s(c).to_string(), rows[s(c)].clone()))
                .collect(),
        )
        .unwrap();
        let mut ladder: LadderState = init["state"].as_object().unwrap().clone();
        let mut ttl: TtlWarned = init["ttl"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), n(v)))
            .collect();
        let mut pnl: PnlLedger = init["pnl"].as_array().unwrap().clone();
        for (i, run) in sc["runs"].as_array().unwrap().iter().enumerate() {
            let at = format!("{name} run {i}");
            let calls: Calls = Rc::default();
            let fakes = Fakes(
                ["binance", "gate", "revx"]
                    .iter()
                    .map(|v| {
                        (
                            v.to_string(),
                            Fake {
                                name: leak(v),
                                spec: run["venues"][*v].clone(),
                                calls: calls.clone(),
                                seen: RefCell::default(),
                            },
                        )
                    })
                    .collect(),
            );
            let (trailing, policy) = (set_of(&run["trailing"]), set_of(&run["policy"]));
            let ctx = RunCtx {
                now: n(&run["now"]),
                blocked: run["blocked"].as_bool().unwrap_or(false),
                market: run["market"].as_str(),
                trailing: &trailing,
                policy: &policy,
            };
            let (bal, mut results): (Balances, _) = housekeeping::fetch_balances(&fakes, "live");
            let mut books = Books {
                journal: &mut journal,
                ladder: &mut ladder,
                ttl: &mut ttl,
                pnl: &mut pnl,
            };
            results.extend(housekeeping::run(
                &st,
                &ctx,
                &bal,
                &mut books,
                &fakes,
                &mut housekeeping::Persist::none(),
            ));
            let out = &run["out"];
            let got: Vec<Value> = results.iter().map(|r| r.to_json()).collect();
            check(
                &mut f,
                same(&out["results"], &json!(got), &format!("{at} results")),
            );
            check(
                &mut f,
                same(
                    &out["calls"],
                    &json!(*calls.borrow()),
                    &format!("{at} calls"),
                ),
            );
            check(
                &mut f,
                same(&out["journal"], &jv(&journal), &format!("{at} journal")),
            );
            check(
                &mut f,
                same(
                    &out["state"],
                    &Value::Object(ladder.clone()),
                    &format!("{at} state"),
                ),
            );
            check(&mut f, same(&out["ttl"], &jv(&ttl), &format!("{at} ttl")));
            check(&mut f, same(&out["pnl"], &jv(&pnl), &format!("{at} pnl")));
        }
    }
    finish("housekeeping", f);
}

// ------------------------------------------------------------------ the pure pieces

#[test]
fn the_arithmetic_matches_the_reference() {
    let g = golden("housekeeping_pure.json");
    let mut f = Vec::new();
    for c in g["limit_target"].as_array().unwrap() {
        let got = hk::limit_target(
            n(&c["fp"]),
            n(&c["target_pct"]),
            Fees::default().pct(s(&c["exch"])),
        );
        check(
            &mut f,
            same(&c["out"], &json!(got), &format!("limit_target {c}")),
        );
    }
    let st = Settings {
        entries: [("AAA".to_string(), 0.4)].into_iter().collect(),
        ..Settings::default()
    };
    for c in g["cost_basis"].as_array().unwrap() {
        let mut state: LadderState = c["state"].as_object().unwrap().clone();
        housekeeping::set_cost_basis(
            &st,
            &mut state,
            s(&c["sym"]),
            n(&c["held"]),
            n(&c["fq"]),
            n(&c["fb"]),
            n(&c["fp"]),
        );
        check(
            &mut f,
            same(&c["out"], &Value::Object(state), &format!("cost basis {c}")),
        );
    }
    let mut ledger: PnlLedger = Vec::new();
    for c in g["pnl"].as_array().unwrap() {
        let (avg, cb) = (c["avg_price"].as_f64(), c["cost_basis"].as_f64());
        let realized = housekeeping::log_pnl(
            &mut ledger,
            s(&c["sym"]),
            n(&c["qty"]),
            n(&c["quote"]),
            avg,
            cb,
            n(&c["now"]),
            s(&c["kind"]),
        );
        check(
            &mut f,
            same(&c["realized"], &json!(realized), &format!("realized {c}")),
        );
        check(
            &mut f,
            same(
                &c["tail"],
                &json!(housekeeping::pnl_tail(realized, cb, avg)),
                &format!("tail {c}"),
            ),
        );
    }
    check(&mut f, same(&g["pnl_ledger"], &json!(ledger), "ledger"));
    for c in g["ttl_days"].as_array().unwrap() {
        let got = hk::effective_ttl_days(n(&c["base"]), 45.0, 30.0, c["market"].as_str());
        check(
            &mut f,
            same(&c["out"], &json!(got), &format!("ttl days {c}")),
        );
    }
    finish("pure", f);
}

// ------------------------------------------------------------------ sellcheck

struct Rounding(Value);

impl Venue for Rounding {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn parse_order(&self, _: &Value) -> Result<ParsedOrder, VenueError> {
        unimplemented!()
    }
    fn order_status(&self, _: &str, _: &str) -> Result<ParsedOrder, VenueError> {
        unimplemented!()
    }
    fn open_orders(&self, _: &str) -> Result<Vec<ParsedOrder>, VenueError> {
        unimplemented!()
    }
    fn cancel(&self, _: &str, _: &str) -> Result<ParsedOrder, VenueError> {
        unimplemented!()
    }
    fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError> {
        unimplemented!()
    }
    fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError> {
        unimplemented!()
    }
    fn price(&self, _: &str) -> Result<f64, VenueError> {
        unimplemented!()
    }
    fn limits(&self, _: &str) -> Result<Limits, VenueError> {
        if self.0["raise_in"] == "limits" {
            return Err(VenueError::new("fake", None, s(&self.0["raise"])));
        }
        Ok(Limits {
            min_base: self.0["min_base"].as_f64().unwrap_or(0.0),
            min_quote: self.0["min_quote"].as_f64().unwrap_or(0.0),
        })
    }
    fn round_amount(&self, _: &str, amount: f64) -> Result<f64, VenueError> {
        if self.0["raise_in"] == "round" {
            return Err(VenueError::new("fake", None, s(&self.0["raise"])));
        }
        let step = self.0["step"].as_f64().unwrap_or(0.0);
        Ok(if step > 0.0 {
            (amount / step).floor() * step
        } else {
            amount
        })
    }
    fn round_price(&self, _: &str, _: f64) -> Result<f64, VenueError> {
        unimplemented!()
    }
    fn market_buy(&self, _: &str, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        unimplemented!()
    }
    fn market_sell(&self, _: &str, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        unimplemented!()
    }
    fn limit_buy(&self, _: &str, _: f64, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        unimplemented!()
    }
    fn limit_sell(&self, _: &str, _: f64, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        unimplemented!()
    }
}

#[test]
fn sellcheck_matches_the_reference() {
    let g = golden("sellcheck.json");
    let mut f = Vec::new();
    let mut all: BTreeMap<String, SellCheck> = BTreeMap::new();
    for (i, c) in g["cases"].as_array().unwrap().iter().enumerate() {
        let got = sellcheck::check(
            &Rounding(c["spec"].clone()),
            s(&c["pair"]),
            c["held"].as_f64(),
            c["price"].as_f64(),
        );
        check(&mut f, same(&c["out"], &jv(&got), &format!("case {i} {c}")));
        all.insert(format!("S{i:02}"), got);
    }
    check(
        &mut f,
        same(
            &g["summary"]["out"],
            &json!(sellcheck::summary(&all)),
            "summary",
        ),
    );
    let one: BTreeMap<String, SellCheck> = [(
        "A".to_string(),
        SellCheck {
            ok: true,
            reason: String::new(),
            qty: 1.0,
            min_base: None,
            min_quote: None,
            notional: 1.0,
        },
    )]
    .into_iter()
    .collect();
    check(
        &mut f,
        same(
            &g["summary_all_ok"],
            &json!(sellcheck::summary(&one)),
            "all ok",
        ),
    );
    check(
        &mut f,
        same(
            &g["summary_empty"],
            &json!(sellcheck::summary(&BTreeMap::new())),
            "empty",
        ),
    );
    finish("sellcheck", f);
}

// ------------------------------------------------------------------ import

#[test]
fn the_run_state_written_by_the_reference_imports_losslessly() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/cex-dir-state");
    let imp = import::read_dir(&dir).unwrap();
    let r = &imp.report;
    assert!(r.mismatches.is_empty(), "{:?}", r.mismatches);
    assert_eq!(r.ladder_coins, Some((1, true)));
    assert_eq!(r.pnl_records, Some(2));
    assert_eq!(r.ttl_flags, Some(1));

    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
    };
    let out = std::env::temp_dir().join(format!("rungbot-import-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let target = out.join("orders-journal.json");
    import::write(&imp, &target, false).unwrap();
    let ladder = store::load_ladder(&store::sibling(&target, store::LADDER_FILE)).unwrap();
    let mut f = Vec::new();
    check(
        &mut f,
        same(
            &read(import::LADDER_SOURCE),
            &Value::Object(ladder),
            "ladder",
        ),
    );
    let pnl = store::load_pnl(&store::sibling(&target, store::PNL_FILE)).unwrap();
    check(&mut f, same(&read(import::PNL_SOURCE), &json!(pnl), "pnl"));
    let ttl = store::load_ttl_warned(&store::sibling(&target, store::TTL_FILE));
    check(&mut f, same(&read(import::TTL_SOURCE), &jv(&ttl), "ttl"));
    finish("import", f);

    // The same import again is a no-op. Run state that moved on is refused like the
    // journal: not replaced without --force.
    import::write(&imp, &target, false).unwrap();
    std::fs::write(store::sibling(&target, store::LADDER_FILE), "{}").unwrap();
    assert!(import::write(&imp, &target, false)
        .unwrap_err()
        .contains("ladder-state.json"));
    import::write(&imp, &target, true).unwrap();
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn run_state_files_follow_the_journal_rules() {
    let out = std::env::temp_dir().join(format!("rungbot-state-rules-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let p = out.join("pnl-ledger.json");
    assert!(store::load_pnl(&p).unwrap().is_empty(), "missing is empty");
    std::fs::write(&p, r#"{"not": "a list"}"#).unwrap();
    assert!(
        store::load_pnl(&p).unwrap().is_empty(),
        "not a list is empty"
    );
    std::fs::write(&p, "[{").unwrap();
    assert!(store::load_pnl(&p).is_err(), "unparseable is an error");
    let t = out.join("ttl-warned.json");
    std::fs::write(&t, "garbage").unwrap();
    assert!(store::load_ttl_warned(&t).is_empty());
    let mut m = Map::new();
    m.insert("AAA".into(), json!({"cost_basis": 0.5}));
    let l = out.join("ladder-state.json");
    store::save_ladder(&l, &m).unwrap();
    assert_eq!(store::load_ladder(&l).unwrap(), m);
    std::fs::write(&l, "{").unwrap();
    assert!(store::load_ladder(&l).is_err());
    let _ = std::fs::remove_dir_all(&out);
}
