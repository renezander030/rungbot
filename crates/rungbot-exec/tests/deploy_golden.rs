//! Replays the deploy layer, the book audit, churn, fill odds and the funding card
//! against goldens frozen from the reference implementation on synthetic inputs
//! (`tests/golden/deploy/*.json`).
//!
//! Each deploy golden is a multi-run scenario: an initial journal, deploy state and
//! scripted venues, then steps (a run of the layer or a manual command) with the
//! venue changes before each. After every step the results, the printed output, every
//! order call the venues saw, the journal and the deploy state must match.
//!
//! The scripted venue follows the same rules the goldens were generated with:
//! prices and pair rules per pair, balances that move only with the layer's own orders,
//! a resting book, per-order final statuses, optional deposit history, and failure
//! rules keyed on a client id, order id or pair.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use indexmap::IndexMap;
use rungbot_exec::deploy::{self, cli, Layer, RegimeFeed};
use rungbot_exec::gate::Deposit;
use rungbot_exec::housekeeping::Persist;
use rungbot_exec::journal::Journal;
use rungbot_exec::reconcile::VenueSource;
use rungbot_exec::run::config::RunConfig;
use rungbot_exec::run::funding;
use rungbot_exec::{store, Balance, Limits, ParsedOrder, Venue, VenueError};
use serde_json::{json, Value};

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("deploy")
}

fn load(name: &str) -> Value {
    let p = golden_dir().join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

/// The parts of a scenario whose key order is behaviour: journal rows are walked in the
/// order they were written.
#[derive(serde::Deserialize)]
struct Ordered {
    initial: OrderedInit,
    steps: Vec<OrderedStep>,
}

#[derive(serde::Deserialize)]
struct OrderedInit {
    #[serde(default)]
    journal: IndexMap<String, Value>,
}

#[derive(serde::Deserialize)]
struct OrderedStep {
    #[serde(default)]
    journal_patch: Option<IndexMap<String, IndexMap<String, Value>>>,
}

fn load_ordered(name: &str) -> Ordered {
    let p = golden_dir().join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

// ------------------------------------------------------------------ comparison

/// Equal as JSON, numbers compared by value (`5` is `5.0`).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// Null values dropped, recursively: null and absent read the same in the port.
fn clean(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(_, x)| !x.is_null())
                .map(|(k, x)| (k.clone(), clean(x)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(clean).collect()),
        x => x.clone(),
    }
}

fn check_same(what: &str, want: &Value, got: &Value) {
    assert!(
        same(want, got),
        "{what} differs\n want: {}\n  got: {}",
        serde_json::to_string_pretty(want).unwrap(),
        serde_json::to_string_pretty(got).unwrap()
    );
}

// ------------------------------------------------------------------ the scripted venue

fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn pair_assets(pair: &str) -> (String, String) {
    if let Some((a, b)) = pair.split_once('/') {
        return (a.into(), b.into());
    }
    if let Some((a, b)) = pair.split_once('_') {
        return (a.into(), b.into());
    }
    for q in ["USDC", "USDT", "USD"] {
        if let Some(a) = pair.strip_suffix(q) {
            return (a.into(), q.into());
        }
    }
    (pair.into(), "USD".into())
}

#[derive(Default)]
struct VState {
    prices: BTreeMap<String, f64>,
    info: BTreeMap<String, Value>,
    bal: BTreeMap<String, Balance>,
    book: Vec<Value>,
    status: BTreeMap<String, Value>,
    fail: Vec<Value>,
    deposits: Vec<Value>,
    n: u64,
}

struct FakeVenue {
    name: &'static str,
    has_deposits: bool,
    st: RefCell<VState>,
    calls: Rc<RefCell<Vec<Value>>>,
}

impl FakeVenue {
    fn new(name: &'static str, spec: &Value, calls: Rc<RefCell<Vec<Value>>>) -> FakeVenue {
        let v = FakeVenue {
            name,
            has_deposits: spec.get("deposits").is_some_and(|d| !d.is_null()),
            st: RefCell::new(VState {
                n: spec.get("next_id").and_then(Value::as_u64).unwrap_or(1),
                ..Default::default()
            }),
            calls,
        };
        v.apply(spec);
        v
    }

    fn apply(&self, p: &Value) {
        let mut s = self.st.borrow_mut();
        if let Some(m) = p.get("prices").and_then(Value::as_object) {
            for (k, x) in m {
                s.prices.insert(k.clone(), x.as_f64().unwrap());
            }
        }
        if let Some(m) = p.get("info").and_then(Value::as_object) {
            for (k, x) in m {
                s.info.insert(k.clone(), x.clone());
            }
        }
        if let Some(m) = p.get("balances").and_then(Value::as_object) {
            for (k, x) in m {
                s.bal.insert(
                    k.clone(),
                    Balance {
                        free: f(x, "free"),
                        locked: f(x, "locked"),
                    },
                );
            }
        }
        if let Some(b) = p.get("book").and_then(Value::as_array) {
            s.book = b.clone();
        }
        if let Some(m) = p.get("status").and_then(Value::as_object) {
            for (k, x) in m {
                s.status.insert(k.clone(), x.clone());
            }
        }
        if let Some(fl) = p.get("fail").and_then(Value::as_array) {
            s.fail = fl.clone();
        }
        if let Some(d) = p.get("deposits").and_then(Value::as_array) {
            s.deposits = d.clone();
        }
    }

    fn check(&self, method: &str, key: &str) -> Result<(), VenueError> {
        let mut s = self.st.borrow_mut();
        for fl in s.fail.iter_mut() {
            let m = fl["method"].as_str().unwrap_or("");
            let k = fl["key"].as_str().unwrap_or("");
            let times = fl["times"].as_i64().unwrap_or(0);
            if m == method && key.contains(k) && times != 0 {
                if times > 0 {
                    fl["times"] = json!(times - 1);
                }
                return Err(VenueError::new(
                    self.name,
                    None,
                    fl["msg"].as_str().unwrap_or("").to_string(),
                ));
            }
        }
        Ok(())
    }

    fn info(&self, pair: &str) -> Result<Value, VenueError> {
        self.check("info", pair)?;
        self.st
            .borrow()
            .info
            .get(pair)
            .cloned()
            .ok_or_else(|| VenueError::new(self.name, None, format!("no pair {pair}")))
    }

    fn log(&self, v: Value) {
        self.calls.borrow_mut().push(v);
    }

    fn bal_mut<'s>(s: &'s mut VState, asset: &str) -> &'s mut Balance {
        s.bal.entry(asset.to_string()).or_default()
    }

    fn next_id(&self) -> String {
        let mut s = self.st.borrow_mut();
        let id = format!("{}-{}", self.name, s.n);
        s.n += 1;
        id
    }

    fn venue_cid(&self, cid: Option<&str>) -> String {
        let Some(cid) = cid.filter(|c| !c.is_empty()) else {
            return String::new();
        };
        match self.name {
            "revx" => rungbot_exec::ids::safe_revx_cid(cid).unwrap(),
            "gate" => rungbot_exec::ids::safe_gate_text(cid).unwrap(),
            _ => cid.to_string(),
        }
    }
}

fn po(v: &Value) -> ParsedOrder {
    ParsedOrder {
        order_id: v["order_id"].as_str().unwrap_or("").into(),
        client_id: v["client_id"].as_str().unwrap_or("").into(),
        side: v["side"].as_str().unwrap_or("").into(),
        price: v["price"].as_f64(),
        qty: f(v, "qty"),
        status: v["status"].as_str().unwrap_or("").into(),
        ..Default::default()
    }
}

impl Venue for FakeVenue {
    fn name(&self) -> &'static str {
        self.name
    }
    fn parse_order(&self, raw: &Value) -> Result<ParsedOrder, VenueError> {
        Ok(ParsedOrder {
            order_id: raw["id"].as_str().unwrap_or("").into(),
            status: raw["status"].as_str().unwrap_or("").into(),
            ..Default::default()
        })
    }
    fn order_status(&self, _pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        self.check("order_status", order_id)?;
        let st = self
            .st
            .borrow()
            .status
            .get(order_id)
            .cloned()
            .unwrap_or_else(|| {
                json!({"status": "cancelled", "filled": false, "base_qty": 0.0,
                                      "quote": 0.0, "avg_price": null})
            });
        Ok(ParsedOrder {
            order_id: order_id.into(),
            status: st["status"].as_str().unwrap_or("").into(),
            filled: st["filled"].as_bool().unwrap_or(false),
            base_qty: st["base_qty"].as_f64(),
            quote: f(&st, "quote"),
            avg_price: st["avg_price"].as_f64(),
            ..Default::default()
        })
    }
    fn open_orders(&self, pair: &str) -> Result<Vec<ParsedOrder>, VenueError> {
        self.check("open_orders", pair)?;
        Ok(self
            .st
            .borrow()
            .book
            .iter()
            .filter(|o| o["pair"] == pair)
            .map(po)
            .collect())
    }
    fn cancel(&self, pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        self.log(json!([self.name, "cancel", pair, order_id]));
        self.check("cancel", order_id)?;
        let mut s = self.st.borrow_mut();
        let Some(i) = s.book.iter().position(|o| o["order_id"] == order_id) else {
            return Err(VenueError::new(self.name, None, "order not found"));
        };
        let o = s.book.remove(i);
        let (_, q) = pair_assets(pair);
        let amt = f(&o, "qty") * f(&o, "price");
        let b = Self::bal_mut(&mut s, &q);
        b.free += amt;
        b.locked -= amt;
        Ok(ParsedOrder::default())
    }
    fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError> {
        self.check("balances", "")?;
        Ok(self
            .st
            .borrow()
            .bal
            .iter()
            .map(|(k, b)| (k.clone(), b.free))
            .collect())
    }
    fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError> {
        self.check("balances_full", "")?;
        Ok(self.st.borrow().bal.clone())
    }
    fn price(&self, pair: &str) -> Result<f64, VenueError> {
        self.check("price", pair)?;
        self.st
            .borrow()
            .prices
            .get(pair)
            .copied()
            .ok_or_else(|| VenueError::new(self.name, None, format!("no price for {pair}")))
    }
    fn limits(&self, pair: &str) -> Result<Limits, VenueError> {
        let i = self.info(pair)?;
        Ok(Limits {
            min_base: f(&i, "min_base"),
            min_quote: f(&i, "min_quote"),
        })
    }
    fn qty_step(&self, pair: &str) -> Result<f64, VenueError> {
        let i = self.info(pair)?;
        let prec = i.get("prec").and_then(Value::as_i64).unwrap_or(0);
        Ok(match self.name {
            "binance" => f(&i, "step"),
            "revx" if f(&i, "step") != 0.0 => f(&i, "step"),
            _ => rungbot_exec::venue::step_from_precision(prec),
        })
    }
    fn round_amount(&self, pair: &str, x: f64) -> Result<f64, VenueError> {
        let i = self.info(pair)?;
        if self.name == "gate" {
            let p = 10f64.powi(i.get("prec").and_then(Value::as_i64).unwrap_or(0) as i32);
            return Ok((x * p).floor() / p);
        }
        let step = f(&i, "step");
        Ok(if step > 0.0 {
            (x / step).floor() * step
        } else {
            x
        })
    }
    fn round_price(&self, pair: &str, p: f64) -> Result<f64, VenueError> {
        let tick = f(&self.info(pair)?, "tick");
        Ok(if tick > 0.0 {
            (p / tick).floor() * tick
        } else {
            p
        })
    }
    fn market_buy(&self, pair: &str, amount: f64, cid: Option<&str>) -> Result<Value, VenueError> {
        self.log(json!([self.name, "market_buy", pair, amount, cid]));
        self.check("market_buy", cid.unwrap_or(pair))?;
        let (b, q) = pair_assets(pair);
        {
            let mut s = self.st.borrow_mut();
            let px = s.prices[pair];
            Self::bal_mut(&mut s, &q).free -= amount;
            Self::bal_mut(&mut s, &b).free += amount / px;
        }
        Ok(json!({"id": self.next_id(), "status": "filled"}))
    }
    fn market_sell(&self, pair: &str, qty: f64, cid: Option<&str>) -> Result<Value, VenueError> {
        self.log(json!([self.name, "market_sell", pair, qty, cid]));
        self.check("market_sell", cid.unwrap_or(pair))?;
        let (b, q) = pair_assets(pair);
        {
            let mut s = self.st.borrow_mut();
            let px = s.prices[pair];
            Self::bal_mut(&mut s, &b).free -= qty;
            Self::bal_mut(&mut s, &q).free += qty * px;
        }
        Ok(json!({"id": self.next_id(), "status": "filled"}))
    }
    fn limit_buy(
        &self,
        pair: &str,
        base: f64,
        price: f64,
        cid: Option<&str>,
    ) -> Result<Value, VenueError> {
        self.log(json!([self.name, "limit_buy", pair, base, price, cid]));
        self.check("limit_buy", cid.unwrap_or(pair))?;
        let (_, q) = pair_assets(pair);
        {
            let mut s = self.st.borrow_mut();
            let b = Self::bal_mut(&mut s, &q);
            b.free -= base * price;
            b.locked += base * price;
        }
        let oid = self.next_id();
        let vcid = self.venue_cid(cid);
        self.st
            .borrow_mut()
            .book
            .push(json!({"order_id": oid, "client_id": vcid,
            "side": "buy", "price": price, "qty": base, "status": "open", "pair": pair}));
        Ok(json!({"id": oid, "status": "open"}))
    }
    fn limit_sell(&self, _: &str, _: f64, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        Err(VenueError::new(self.name, None, "not scripted"))
    }
    fn deposits(&self, _since: f64) -> Option<Result<Vec<Deposit>, VenueError>> {
        if !self.has_deposits {
            return None;
        }
        if let Err(e) = self.check("deposits", "") {
            return Some(Err(e));
        }
        Some(Ok(self
            .st
            .borrow()
            .deposits
            .iter()
            .map(|d| Deposit {
                currency: d["currency"].as_str().map(String::from),
                amount: f(d, "amount"),
                status: d["status"].as_str().map(String::from),
                ts: f(d, "ts"),
            })
            .collect()))
    }
}

struct Venues(BTreeMap<String, FakeVenue>);

impl VenueSource for Venues {
    fn venue(&self, exch: &str) -> Result<&dyn Venue, String> {
        self.0
            .get(exch)
            .map(|v| v as &dyn Venue)
            .ok_or_else(|| format!("no {exch}"))
    }
}

struct Feed {
    regime: RefCell<Value>,
    history: RefCell<Value>,
    closes: RefCell<BTreeMap<String, Vec<f64>>>,
}

impl RegimeFeed for Feed {
    fn regime(&self) -> Result<Value, String> {
        let r = self.regime.borrow().clone();
        if r.is_null() {
            return Err("regime cache cold".into());
        }
        Ok(r)
    }
    fn label_history(&self) -> Result<Value, String> {
        Ok(self.history.borrow().clone())
    }
    fn closes(&self, exch: &str, pair: &str, _n: usize) -> Result<Vec<f64>, String> {
        self.closes
            .borrow()
            .get(&format!("{exch}|{pair}"))
            .cloned()
            .ok_or_else(|| format!("no closes for {pair}"))
    }
}

fn closes_of(v: &Value) -> BTreeMap<String, Vec<f64>> {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, a)| {
                    (
                        k.clone(),
                        a.as_array()
                            .unwrap()
                            .iter()
                            .map(|x| x.as_f64().unwrap())
                            .collect(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------ the config

fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

fn config(c: &Value, dir: &Path) -> RunConfig {
    let mut y = String::from("watchlist:\n");
    let routing = c["routing"].as_array().unwrap();
    for r in routing {
        y.push_str(&format!("  {}: x\n", r[0].as_str().unwrap()));
    }
    y.push_str("routing:\n");
    for r in routing {
        y.push_str(&format!(
            "  {}: {}\n",
            r[0].as_str().unwrap(),
            yaml_str(&format!(
                "{} {} {}",
                r[1].as_str().unwrap(),
                r[2].as_str().unwrap(),
                r[3].as_str().unwrap()
            ))
        ));
    }
    if let Some(rp) = c.get("revx_pairs").and_then(Value::as_array) {
        y.push_str("revx_pairs:\n");
        for r in rp {
            y.push_str(&format!(
                "  {}: {}\n",
                r[0].as_str().unwrap(),
                yaml_str(r[1].as_str().unwrap())
            ));
        }
    }
    let alloc = c
        .get("alloc")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if !alloc.is_empty() {
        y.push_str("deploy_alloc:\n");
        for (k, v) in &alloc {
            y.push_str(&format!("  {k}: {v}\n"));
        }
    }
    let zones = c
        .get("zones")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if !zones.is_empty() {
        y.push_str("deploy_zones:\n");
        for (k, z) in &zones {
            y.push_str(&format!(
                "  {k}:\n    depths: {}\n    weights: {}\n",
                z["depths"], z["weights"]
            ));
        }
    }
    for k in [
        "deploy_min_usd",
        "deploy_max_tranche_usd",
        "deploy_max_depth_pct",
        "deploy_bull_sweep_days",
        "deploy_bull_sweep_max_age",
    ] {
        if let Some(v) = c.get(k) {
            y.push_str(&format!("{k}: {v}\n"));
        }
    }
    for k in [
        "deploy_pin_prices",
        "deploy_sweep_idle",
        "deploy_bull_sweep",
    ] {
        if let Some(v) = c.get(k).and_then(Value::as_bool) {
            y.push_str(&format!("{k}: {}\n", if v { "yes" } else { "no" }));
        }
    }
    y.push_str("deploy: live\nlive_trading_enabled: yes\n");
    // Paths as the run tests write them: double-quoted, verbatim (Windows backslashes).
    y.push_str(&format!("state_dir: \"{}\"\n", dir.display()));
    y.push_str(&format!("halt_file: \"{}\"\n", dir.join("HALT").display()));
    RunConfig::from_yaml(&y, &|_| None).unwrap_or_else(|e| panic!("{e}\n{y}"))
}

// ------------------------------------------------------------------ one scenario

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!(
            "rungbot-deploy-golden-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_scenario(g: &Value, ord: &Ordered) {
    let name = g["name"].as_str().unwrap();
    let dir = Scratch::new(name);
    let base_cfg = config(&g["config"], &dir.0);
    let init = &g["initial"];
    let jpath = base_cfg.journal_path();
    let spath = base_cfg.deploy_state_path();
    let mut journal = Journal::default();
    for (cid, row) in &ord.initial.journal {
        journal
            .orders
            .insert(cid.clone(), serde_json::from_value(row.clone()).unwrap());
    }
    store::save_journal(&jpath, &journal).unwrap();
    if !init["state"].is_null() {
        std::fs::write(&spath, init["state"].to_string()).unwrap();
    }
    let calls = Rc::new(RefCell::new(Vec::new()));
    let venues = Venues(
        ["binance", "gate", "revx"]
            .into_iter()
            .map(|n| {
                (
                    n.to_string(),
                    FakeVenue::new(n, &init["venues"][n], calls.clone()),
                )
            })
            .collect(),
    );
    let feed = Feed {
        regime: RefCell::new(
            init.get("regime")
                .cloned()
                .unwrap_or_else(|| json!({"market": "chop", "coins": {}})),
        ),
        history: RefCell::new(
            init.get("history")
                .cloned()
                .unwrap_or_else(|| json!({"labels": []})),
        ),
        closes: RefCell::new(closes_of(&init["closes"])),
    };
    let clock = Cell::new(0.0);
    for (i, step) in g["steps"].as_array().unwrap().iter().enumerate() {
        let at = format!("{name} step {i} ({})", step["op"]);
        let now = step["now"].as_f64().unwrap();
        clock.set(now);
        if let Some(m) = step.get("venues").and_then(Value::as_object) {
            for (n, p) in m {
                venues.0[n].apply(p);
            }
        }
        if let Some(r) = step.get("regime") {
            *feed.regime.borrow_mut() = r.clone();
        }
        if let Some(h) = step.get("history") {
            *feed.history.borrow_mut() = h.clone();
        }
        if let Some(c) = step.get("closes") {
            feed.closes.borrow_mut().extend(closes_of(c));
        }
        if let Some(m) = &ord.steps[i].journal_patch {
            for (cid, fields) in m {
                let mut row = journal
                    .orders
                    .get(cid)
                    .map(|o| serde_json::to_value(o).unwrap())
                    .unwrap_or_else(|| json!({"client_id": cid}));
                for (k, v) in fields {
                    row[k] = v.clone();
                }
                journal
                    .orders
                    .insert(cid.clone(), serde_json::from_value(row).unwrap());
            }
            store::save_journal(&jpath, &journal).unwrap();
        }
        let halt = base_cfg.halt_file.clone();
        if step["halt"].as_bool() == Some(true) {
            std::fs::write(&halt, "").unwrap();
        } else {
            let _ = std::fs::remove_file(&halt);
        }
        let mut cfg = base_cfg.clone();
        cfg.live_trading_enabled = step
            .get("live")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                g["config"]
                    .get("live")
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
            });
        calls.borrow_mut().clear();
        let jp = jpath.clone();
        let mut persist = Persist::new(move |j: &Journal| store::save_journal(&jp, j), |_| Ok(()));
        let stderr = |_: &str| {};
        let clk = || clock.get();
        let mut layer = Layer {
            cfg: &cfg,
            venues: &venues,
            regime: &feed,
            clock: &clk,
            sleep: &|_| {},
            persist: &mut persist,
            stderr: &stderr,
            j: &mut journal,
            results: Vec::new(),
        };
        let mut out: Vec<u8> = Vec::new();
        let args = &step["args"];
        let r = match step["op"].as_str().unwrap() {
            "check" => layer.check(),
            "status" => cli::status(&layer, &mut out),
            "plan" => cli::plan(&layer, args["usd"].as_f64().unwrap(), &mut out),
            "tranche" => {
                let only = args["only"]
                    .as_array()
                    .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect());
                cli::tranche(
                    &mut layer,
                    args["usd"].as_f64().unwrap(),
                    args["venue"].as_str().unwrap(),
                    only,
                    &mut out,
                )
            }
            "market" => cli::market(
                &mut layer,
                args["share"].as_f64().unwrap(),
                args["venue"].as_str().unwrap(),
                args["sym"].as_str().unwrap(),
                &mut out,
            ),
            "cancel" => cli::cancel(&mut layer, args["venue"].as_str(), &mut out),
            op => panic!("unknown op {op}"),
        };
        let results = std::mem::take(&mut layer.results);
        let want = &step["expect"];
        let is_check = step["op"] == "check";
        match (&r, want["exit"].as_str(), want["raised"].as_bool().unwrap()) {
            (Err(e), Some(x), _) => assert_eq!(e, x, "{at}: exit message"),
            (Err(e), None, true) => {
                let _ = e;
            }
            (Err(e), None, false) => panic!("{at}: unexpected error {e}"),
            (Ok(()), Some(x), _) => panic!("{at}: expected exit {x}"),
            (Ok(()), None, true) => panic!("{at}: expected an uncaught failure"),
            (Ok(()), None, false) => {}
        }
        if is_check {
            let got: Vec<Value> = results.iter().map(|r| r.to_json()).collect();
            check_same(&format!("{at}: results"), &want["results"], &json!(got));
        } else {
            let text = String::from_utf8(out)
                .unwrap()
                .replace(&spath.display().to_string(), "<STATE>")
                .replace(&halt.display().to_string(), "<HALT>");
            assert_eq!(text, want["stdout"].as_str().unwrap(), "{at}: output");
        }
        check_same(
            &format!("{at}: venue calls"),
            &want["calls"],
            &json!(*calls.borrow()),
        );
        let j: Journal = store::load_journal(&jpath).unwrap();
        assert_eq!(j, journal, "{at}: the journal on disk is the one in memory");
        check_same(
            &format!("{at}: journal"),
            &want["journal"],
            &clean(&serde_json::to_value(&journal).unwrap()),
        );
        let st = deploy::load_state(&spath).unwrap();
        if want["state"].is_null() {
            assert!(st.is_empty(), "{at}: no state expected");
        } else {
            check_same(&format!("{at}: state"), &want["state"], &Value::Object(st));
        }
    }
}

macro_rules! scenario {
    ($t:ident, $f:literal) => {
        #[test]
        fn $t() {
            run_scenario(&load($f), &load_ordered($f));
        }
    };
}

scenario!(first_tranche_and_roll, "deploy_first_tranche_and_roll.json");
scenario!(
    resume_adoption_and_trim,
    "deploy_resume_adoption_and_trim.json"
);
scenario!(idle_sweep, "deploy_idle_sweep.json");
scenario!(bull_sweep_young, "deploy_bull_sweep_young.json");
scenario!(bull_sweep_old, "deploy_bull_sweep_old.json");
scenario!(bull_sweep_off, "deploy_bull_sweep_off.json");
scenario!(onramp_and_inflight, "deploy_onramp_and_inflight.json");
scenario!(
    inflight_balance_growth,
    "deploy_inflight_balance_growth.json"
);
scenario!(withhold, "deploy_withhold.json");
scenario!(blocked_then_live, "deploy_blocked_then_live.json");
scenario!(
    zones_override_depth_cap_unreachable,
    "deploy_zones_override_depth_cap_unreachable.json"
);
scenario!(cli_commands, "deploy_cli.json");
scenario!(cli_market_failure, "deploy_cli_market_failure.json");
scenario!(
    inflight_accumulate_and_backstop,
    "deploy_inflight_accumulate_and_backstop.json"
);

#[test]
fn every_deploy_golden_is_replayed() {
    let mut names: Vec<String> = std::fs::read_dir(golden_dir())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("deploy_"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 14, "{names:?}");
}

// ------------------------------------------------------------------ audit, churn, odds, card

fn routing_cfg(routing: &Value, dir: &Path) -> RunConfig {
    config(&json!({"routing": routing}), dir)
}

#[test]
fn book_audit() {
    let g = load("audit.json");
    let dir = Scratch::new("audit");
    let cfg = routing_cfg(&g["routing"], &dir.0);
    for case in g["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let venues = Venues(
            ["gate", "revx"]
                .into_iter()
                .map(|n| {
                    (
                        n.to_string(),
                        FakeVenue::new(n, &case["venues"][n], calls.clone()),
                    )
                })
                .collect(),
        );
        let j: Journal = serde_json::from_value(case["journal"].clone()).unwrap();
        let (results, state) = deploy::audit::run(&cfg, &venues, &j, case["now"].as_f64().unwrap());
        let got: Vec<Value> = results.iter().map(|r| r.to_json()).collect();
        let want: Vec<Value> = case["expect"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r["hk"] = json!(true);
                r
            })
            .collect();
        check_same(&format!("audit {name}: results"), &json!(want), &json!(got));
        check_same(
            &format!("audit {name}: state"),
            &case["expect"]["state"],
            &serde_json::to_value(&state).unwrap(),
        );
    }
}

fn journal_rows(v: &Value) -> Vec<rungbot_exec::Order> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| serde_json::from_value(r.clone()).unwrap())
        .collect()
}

#[test]
fn churn_metrics() {
    let g = load("churn.json");
    let now = g["now"].as_f64().unwrap();
    for case in g["cases"].as_array().unwrap() {
        let rows = journal_rows(case.get("rows").unwrap_or(&g["rows"]));
        let refs: Vec<&rungbot_exec::Order> = rows.iter().collect();
        let days = case["days"].as_i64().unwrap();
        let m = deploy::churn::metrics(&refs, now, days);
        check_same(
            &format!("churn {days}d"),
            &case["metrics"],
            &serde_json::to_value(&m).unwrap(),
        );
        assert_eq!(
            deploy::churn::summary(&m),
            case["summary"].as_str().unwrap()
        );
    }
}

#[test]
fn fill_odds() {
    let g = load("fillodds.json");
    let dir = Scratch::new("fillodds");
    let cache = dir.0.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    for (name, rows) in g["cache"].as_object().unwrap() {
        std::fs::write(cache.join(name), rows.to_string()).unwrap();
    }
    let j: Journal = serde_json::from_value(g["journal"].clone()).unwrap();
    let rcfg = rungbot_core::RegimeConfig {
        run_min_signals: 3,
        run_ret30_min: 25.0,
    };
    for case in g["cases"].as_array().unwrap() {
        let hz: Vec<usize> = case["horizons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_u64().unwrap() as usize)
            .collect();
        let res = deploy::fillodds::compute(
            &j,
            &g["regime"],
            Some(&g["history"]),
            &cache,
            &hz,
            0.30,
            rcfg,
        )
        .unwrap();
        check_same(
            &format!("fillodds {hz:?}"),
            &case["json"],
            &serde_json::to_value(&res).unwrap(),
        );
        assert_eq!(
            deploy::fillodds::render(&res),
            case["text"].as_str().unwrap()
        );
    }
}

#[test]
fn funding_card() {
    let g = load("funding.json");
    let routing: Vec<(String, String)> = g["routing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_str().unwrap().into(), r[1].as_str().unwrap().into()))
        .collect();
    let alloc: Vec<(String, f64)> = g["alloc"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_f64().unwrap()))
        .collect();
    let bals = |v: &Value| -> BTreeMap<String, Balance> {
        v.as_object()
            .unwrap()
            .iter()
            .map(|(k, b)| {
                (
                    k.clone(),
                    Balance {
                        free: f(b, "free"),
                        locked: f(b, "locked"),
                    },
                )
            })
            .collect()
    };
    for case in g["cases"].as_array().unwrap() {
        let card = funding::card(
            f(case, "rx_stable"),
            f(case, "gate_stable"),
            f(case, "usdc_free"),
            &alloc,
            &routing,
        );
        check_same(
            "funding card",
            &case["card"],
            &serde_json::to_value(&card).unwrap(),
        );
        assert_eq!(
            funding::revx_stable_from(&bals(&case["rx_balances"])),
            f(case, "revx_stable_from")
        );
        assert_eq!(
            funding::gate_stable_from(&bals(&case["gate_balances"])),
            f(case, "gate_stable_from")
        );
    }
}

#[test]
fn deploy_and_audit_state_import_from_the_reference() {
    let dir = Scratch::new("import-src");
    let out = Scratch::new("import-out");
    let onramp = load("deploy_onramp_and_inflight.json");
    // The state as the reference left it with a top-up in flight.
    let deploy_state = onramp["steps"][1]["expect"]["state"].clone();
    assert!(deploy_state.get("inflight").is_some());
    let audit_state = load("audit.json")["cases"][0]["expect"]["state"].clone();
    std::fs::write(dir.0.join("orders-journal.json"), "{}").unwrap();
    std::fs::write(
        dir.0.join("deploy-state.live.json"),
        deploy_state.to_string(),
    )
    .unwrap();
    std::fs::write(dir.0.join("audit-state.json"), audit_state.to_string()).unwrap();
    let imp = rungbot_exec::import::read_dir(&dir.0).unwrap();
    let lines = imp.report.lines().join("\n");
    assert!(
        lines.contains("deploy state: 3 venue baseline(s), a top-up in flight"),
        "{lines}"
    );
    let n = audit_state["findings"].as_array().unwrap().len();
    assert!(
        lines.contains(&format!("book audit: {n} finding(s)")),
        "{lines}"
    );
    let target = out.0.join("orders-journal.json");
    rungbot_exec::import::write(&imp, &target, false).unwrap();
    let st = deploy::load_state(&out.0.join("deploy-state.json")).unwrap();
    check_same("imported deploy state", &deploy_state, &Value::Object(st));
    let a: Value =
        serde_json::from_str(&std::fs::read_to_string(out.0.join("audit-state.json")).unwrap())
            .unwrap();
    check_same("imported audit state", &audit_state, &a);
    // A second import does not replace them without --force.
    std::fs::write(&target, "{}").unwrap();
    let e = rungbot_exec::import::write(&imp, &target, false).unwrap_err();
    assert!(e.contains("deploy-state.json"), "{e}");
}

#[test]
fn a_state_file_that_does_not_parse_stops_the_layer() {
    let g = load("deploy_blocked_then_live.json");
    let dir = Scratch::new("unreadable");
    let cfg = config(&g["config"], &dir.0);
    std::fs::write(cfg.deploy_state_path(), "{not json").unwrap();
    let venues = Venues(BTreeMap::new());
    let feed = Feed {
        regime: RefCell::new(json!({"market": "chop"})),
        history: RefCell::new(json!({})),
        closes: RefCell::new(BTreeMap::new()),
    };
    let mut j = Journal::default();
    let mut layer = Layer {
        cfg: &cfg,
        venues: &venues,
        regime: &feed,
        clock: &|| 1.0,
        sleep: &|_| {},
        persist: &mut Persist::none(),
        stderr: &|_| {},
        j: &mut j,
        results: Vec::new(),
    };
    let e = layer.check().unwrap_err();
    assert!(e.contains("deploy-state.json"), "{e}");
}

// ------------------------------------------------------------------ review regressions

/// One scenario's venues, journal and state, run step by step with any closure over the
/// layer. `venues` names the venues that have a client; one left out has none.
struct Rig {
    dir: Scratch,
    cfg: RunConfig,
    venues: Venues,
    feed: Feed,
    journal: Journal,
    calls: Rc<RefCell<Vec<Value>>>,
    /// Journal writes so far, and the write (1-based) from which every write fails.
    writes: Rc<Cell<usize>>,
    fail_write_from: Option<usize>,
}

struct Step {
    r: Result<(), String>,
    texts: Vec<String>,
    calls: Vec<Value>,
    out: String,
}

impl Rig {
    fn new(tag: &str, g: &Value, venues: &[&'static str], state: Option<Value>) -> Rig {
        let dir = Scratch::new(tag);
        let cfg = config(&g["config"], &dir.0);
        let init = &g["initial"];
        let mut journal = Journal::default();
        if let Some(m) = init["journal"].as_object() {
            for (cid, row) in m {
                journal
                    .orders
                    .insert(cid.clone(), serde_json::from_value(row.clone()).unwrap());
            }
        }
        store::save_journal(&cfg.journal_path(), &journal).unwrap();
        if let Some(s) = state.or_else(|| Some(init["state"].clone()).filter(|v| !v.is_null())) {
            std::fs::write(cfg.deploy_state_path(), s.to_string()).unwrap();
        }
        let calls = Rc::new(RefCell::new(Vec::new()));
        let venues = Venues(
            venues
                .iter()
                .map(|n| {
                    (
                        n.to_string(),
                        FakeVenue::new(n, &init["venues"][*n], calls.clone()),
                    )
                })
                .collect(),
        );
        let feed = Feed {
            regime: RefCell::new(
                init.get("regime")
                    .cloned()
                    .unwrap_or_else(|| json!({"market": "chop", "coins": {}})),
            ),
            history: RefCell::new(
                init.get("history")
                    .cloned()
                    .unwrap_or_else(|| json!({"labels": []})),
            ),
            closes: RefCell::new(closes_of(&init["closes"])),
        };
        Rig {
            dir,
            cfg,
            venues,
            feed,
            journal,
            calls,
            writes: Rc::new(Cell::new(0)),
            fail_write_from: None,
        }
    }

    fn venue(&self, n: &str) -> &FakeVenue {
        &self.venues.0[n]
    }

    fn run(
        &mut self,
        now: f64,
        f: impl FnOnce(&mut Layer, &mut Vec<u8>) -> Result<(), String>,
    ) -> Step {
        self.calls.borrow_mut().clear();
        let jpath = self.cfg.journal_path();
        let writes = self.writes.clone();
        let fail_from = self.fail_write_from;
        let mut persist = Persist::new(
            move |j: &Journal| {
                writes.set(writes.get() + 1);
                if fail_from.is_some_and(|n| writes.get() >= n) {
                    return Err("disk full".into());
                }
                store::save_journal(&jpath, j)
            },
            |_| Ok(()),
        );
        let clk = || now;
        let mut layer = Layer {
            cfg: &self.cfg,
            venues: &self.venues,
            regime: &self.feed,
            clock: &clk,
            sleep: &|_| {},
            persist: &mut persist,
            stderr: &|_| {},
            j: &mut self.journal,
            results: Vec::new(),
        };
        let mut out = Vec::new();
        let r = f(&mut layer, &mut out);
        let texts = layer
            .results
            .iter()
            .map(|x| {
                let v = x.to_json();
                ["done", "warn", "err"]
                    .iter()
                    .find_map(|k| v.get(*k).and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default()
            })
            .collect();
        Step {
            r,
            texts,
            calls: self.calls.borrow().clone(),
            out: String::from_utf8(out).unwrap(),
        }
    }

    fn check(&mut self, now: f64) -> Step {
        self.run(now, |l, _| l.check())
    }

    fn state(&self) -> serde_json::Map<String, Value> {
        deploy::load_state(&self.cfg.deploy_state_path()).unwrap()
    }
}

fn has_call(s: &Step, venue: &str, method: &str) -> bool {
    s.calls.iter().any(|c| c[0] == venue && c[1] == method)
}

/// Revolut X holds staged USDC for the Gate top-up and fresh USD above its baseline.
fn staged_topup() -> (Value, Value) {
    let g = load("deploy_onramp_and_inflight.json");
    let mut g = g.clone();
    g["initial"]["venues"]["revx"]["balances"] = json!({
        "USD": {"free": 400.0, "locked": 0.0},
        "USDC": {"free": 300.0, "locked": 0.0}
    });
    let state = json!({"ts": 1799998200.0, "stable": {"binance": 50.0, "gate": 100.0, "revx": 100.0},
                       "part": {}, "offquote": {"revx:USDC": 300.0}});
    (g, state)
}

fn assert_fail_closed(rig: &mut Rig, s: &Step) {
    assert!(s.r.is_ok(), "{:?}", s.r);
    assert!(
        !has_call(s, "revx", "market_sell"),
        "staged USDC sold: {:?}",
        s.calls
    );
    assert!(
        !has_call(s, "revx", "limit_buy"),
        "revx laddered: {:?}",
        s.calls
    );
    assert!(
        s.texts.iter().any(|t| t
            == "Gate top-up reserve unknown (Gate balance unreadable): no new capital laddered on revx this run"),
        "{:?}",
        s.texts
    );
    // The new capital is not absorbed: it carries to the next run.
    assert_eq!(rig.state()["stable"]["revx"], json!(100.0));
    let _ = &rig.dir;
}

#[test]
fn a_missing_gate_client_holds_the_revx_topup_reserve() {
    let (g, state) = staged_topup();
    let mut rig = Rig::new("rv-nogate", &g, &["binance", "revx"], Some(state));
    let s = rig.check(1_800_000_000.0);
    assert!(
        s.texts.iter().any(|t| t.starts_with(
            "Gate top-up reserve unavailable in the revx pre-step; staged USDC stays USDC this run: "
        )),
        "{:?}",
        s.texts
    );
    assert_fail_closed(&mut rig, &s);
}

#[test]
fn an_unreadable_gate_balance_holds_the_revx_topup_reserve() {
    let (mut g, state) = staged_topup();
    g["initial"]["venues"]["gate"]["fail"] =
        json!([{"method": "balances_full", "key": "", "msg": "gate down", "times": -1}]);
    let mut rig = Rig::new("rv-gatedown", &g, &["binance", "gate", "revx"], Some(state));
    let s = rig.check(1_800_000_000.0);
    assert!(
        s.texts.iter().any(|t| t
            == "Gate top-up reserve unavailable in the revx pre-step; staged USDC stays USDC this run: gate down"),
        "{:?}",
        s.texts
    );
    assert_fail_closed(&mut rig, &s);
    // Once Gate reads again the reserve is known and the fresh USD ladders.
    rig.venue("gate").apply(&json!({"fail": []}));
    let s = rig.check(1_800_001_800.0);
    assert!(has_call(&s, "revx", "limit_buy"), "{:?}", s.calls);
}

#[test]
fn a_venue_without_a_client_keeps_its_baseline() {
    let g = load("deploy_onramp_and_inflight.json");
    let mut rig = Rig::new("rv-nobinance", &g, &["gate", "revx"], None);
    let s = rig.check(1_800_000_000.0);
    assert!(s.r.is_ok(), "{:?}", s.r);
    assert_eq!(rig.state()["stable"]["binance"], json!(50.0));
}

/// The young-bull scenario: AAA's two stale zones on Revolut X are swept to market.
fn bull() -> Value {
    load("deploy_bull_sweep_young.json")
}

fn sweep_buy(s: &Step) -> f64 {
    s.calls
        .iter()
        .find(|c| c[0] == "revx" && c[1] == "market_buy")
        .and_then(|c| c[3].as_f64())
        .unwrap_or_else(|| panic!("no sweep buy: {:?}", s.calls))
}

#[test]
fn a_bull_sweep_cancel_with_an_unreadable_status_adds_nothing() {
    let mut g = bull();
    g["initial"]["venues"]["revx"]["fail"] =
        json!([{"method": "order_status", "key": "revx-60", "msg": "timeout", "times": -1}]);
    let mut rig = Rig::new("rv-bull-unread", &g, &["binance", "gate", "revx"], None);
    let s = rig.check(1_800_000_000.0);
    assert!(s.r.is_ok(), "{:?}", s.r);
    assert!(
        s.texts
            .iter()
            .any(|t| t == "bull sweep: b1 final status unreadable, its budget is not swept"),
        "{:?}",
        s.texts
    );
    // Only b2's unspent $54 is swept, not b1's $95 on top.
    assert_eq!(sweep_buy(&s), 53.83);
}

#[test]
fn a_bull_sweep_buy_is_capped_at_free_cash() {
    let mut g = bull();
    // b1 mostly filled venue-side since the last poll: its cancel frees only $9.50.
    let book = g["initial"]["venues"]["revx"]["book"]
        .as_array_mut()
        .unwrap();
    book[0]["qty"] = json!(1.0);
    g["initial"]["venues"]["revx"]["balances"]["USD"]["free"] = json!(0.0);
    let mut rig = Rig::new("rv-bull-cap", &g, &["binance", "gate", "revx"], None);
    let s = rig.check(1_800_000_000.0);
    assert!(s.r.is_ok(), "{:?}", s.r);
    // Free after the cancels: $9.50 + $90.00; the journal's unspent says $149.
    let buy = sweep_buy(&s);
    assert!(buy <= 99.5, "market buy ${buy} exceeds free cash");
    assert_eq!(buy, 99.2);
}

fn cli_rig(tag: &str) -> Rig {
    let g = load("deploy_cli.json");
    Rig::new(tag, &g, &["binance", "gate", "revx"], None)
}

#[test]
fn cli_market_share_must_be_above_0_and_at_most_100() {
    let mut rig = cli_rig("rv-share");
    for (share, shown) in [
        (0.0, "0.0"),
        (-5.0, "-5.0"),
        (100.5, "100.5"),
        (f64::NAN, "nan"),
        (f64::INFINITY, "inf"),
    ] {
        let s = rig.run(1_800_000_000.0, |l, o| {
            cli::market(l, share, "revx", "AAA", o)
        });
        assert_eq!(
            s.r,
            Err(format!(
                "--market share must be above 0 and at most 100, got {shown}"
            ))
        );
        assert!(s.calls.is_empty(), "{:?}", s.calls);
    }
}

#[test]
fn cli_tranche_refuses_a_negative_or_non_finite_budget() {
    let mut rig = cli_rig("rv-tranche-neg");
    for b in [-10.0, f64::NAN, f64::INFINITY] {
        let s = rig.run(1_800_000_000.0, |l, o| cli::tranche(l, b, "gate", None, o));
        assert!(s.r.is_err(), "{b}: {:?}", s.r);
        assert!(s.calls.is_empty(), "{b}: {:?}", s.calls);
    }
}

#[test]
fn cli_cancel_refuses_an_unknown_venue() {
    let mut rig = cli_rig("rv-cancel");
    let s = rig.run(1_800_000_000.0, |l, o| cli::cancel(l, Some("kraken"), o));
    assert!(s.r.is_err(), "{:?} {}", s.r, s.out);
    assert!(s.calls.is_empty(), "{:?}", s.calls);
}

#[test]
fn cli_numbers_must_be_finite() {
    for bad in ["nan", "NaN", "inf", "-inf", "1e999", "x"] {
        assert!(cli::parse_num(bad, "tranche").is_err(), "{bad}");
    }
    assert_eq!(cli::parse_num("12.5", "tranche"), Ok(12.5));
}

/// Gate with no zones, $100 USDT, baseline $100.
fn tranche_rig(tag: &str, usdt: f64) -> Rig {
    let mut g = load("deploy_onramp_and_inflight.json");
    g["initial"]["venues"]["gate"]["balances"] = json!({"USDT": {"free": usdt, "locked": 0.0}});
    Rig::new(tag, &g, &["binance", "gate", "revx"], None)
}

#[test]
fn a_cli_tranche_of_baselined_money_leaves_the_baseline() {
    let mut rig = tranche_rig("rv-bump-none", 100.0);
    let s = rig.run(1_800_000_000.0, |l, o| {
        cli::tranche(l, 50.0, "gate", None, o)
    });
    assert!(s.r.is_ok(), "{:?}", s.r);
    assert!(has_call(&s, "gate", "limit_buy"), "{:?}", s.calls);
    assert_eq!(rig.state()["stable"]["gate"], json!(100.0));
}

#[test]
fn a_cli_tranche_raises_the_baseline_by_new_money_placed() {
    let mut rig = tranche_rig("rv-bump-part", 130.0);
    let s = rig.run(1_800_000_000.0, |l, o| {
        cli::tranche(l, 50.0, "gate", None, o)
    });
    assert!(s.r.is_ok(), "{:?}", s.r);
    assert_eq!(rig.state()["stable"]["gate"], json!(130.0));
    // Nothing placed: nothing counted as deployed.
    let mut rig = tranche_rig("rv-bump-fail", 130.0);
    rig.venue("gate").apply(
        &json!({"fail": [{"method": "limit_buy", "key": "dep", "msg": "down", "times": -1}]}),
    );
    let s = rig.run(1_800_000_000.0, |l, o| {
        cli::tranche(l, 50.0, "gate", None, o)
    });
    assert!(s.r.is_ok(), "{:?}", s.r);
    assert_eq!(rig.state()["stable"]["gate"], json!(100.0));
}

#[test]
fn a_malformed_inflight_marker_is_read_as_none() {
    let g = load("deploy_onramp_and_inflight.json");
    let mut g = g.clone();
    g["initial"]["venues"]["revx"]["balances"] =
        json!({"USD": {"free": 100.0, "locked": 0.0}, "USDC": {"free": 0.0, "locked": 0.0}});
    // Gate cannot say what landed: the USDC that left reads as a top-up in flight.
    g["initial"]["venues"]["gate"]
        .as_object_mut()
        .unwrap()
        .remove("deposits");
    let state = json!({"ts": 1799998200.0, "stable": {"binance": 50.0, "gate": 100.0, "revx": 100.0},
                       "part": {}, "offquote": {"revx:USDC": 300.0}, "inflight": "garbage"});
    let mut rig = Rig::new("rv-inflight", &g, &["binance", "gate", "revx"], Some(state));
    let s = rig.check(1_800_000_000.0);
    assert!(s.r.is_ok(), "{:?}", s.r);
    assert!(
        s.texts
            .iter()
            .any(|t| t.contains("`inflight` is not an object")),
        "{:?}",
        s.texts
    );
    let st = rig.state();
    assert_eq!(st["inflight"]["amount"], json!(300.0), "{st:?}");
}

#[test]
fn a_coin_without_a_positive_spot_is_skipped() {
    for px in [0.0, -1.0] {
        let mut g = load("deploy_cli.json");
        g["initial"]["venues"]["revx"]["prices"]["AAA/USD"] = json!(px);
        let mut rig = Rig::new("rv-spot", &g, &["binance", "gate", "revx"], None);
        let s = rig.run(1_800_000_000.0, |l, o| cli::plan(l, 200.0, o));
        assert!(s.r.is_ok(), "{:?}", s.r);
        assert!(
            s.out.contains("skip AAA: venue data: spot "),
            "{px}: {}",
            s.out
        );
    }
}

fn cfg_yaml(extra: &str) -> Result<RunConfig, String> {
    let y = format!("watchlist:\n  AAA: x\nrouting:\n  AAA: \"gate AAA_USDT USDT\"\n{extra}");
    RunConfig::from_yaml(&y, &|_| None)
}

#[test]
fn deploy_config_rejects_non_positive_amounts_and_bad_zone_weights() {
    assert!(cfg_yaml("").is_ok());
    for bad in [
        "deploy_min_usd: 0\n",
        "deploy_min_usd: -5\n",
        "deploy_max_tranche_usd: 0\n",
        "deploy_max_tranche_usd: -1\n",
        "deploy_zones:\n  AAA:\n    depths: [5, 10]\n    weights: [30, 40]\n",
    ] {
        assert!(cfg_yaml(bad).is_err(), "accepted: {bad}");
    }
    assert!(cfg_yaml(
        "deploy_zones:\n  AAA:\n    depths: [5, 10, 20]\n    weights: [30, 40, 30]\n"
    )
    .is_ok());
}

#[test]
fn a_failed_journal_write_after_a_placement_stops_placing_without_aborting() {
    // The onramp run journals six rungs (writes 1-6), then places them one by one, each
    // placement written with the venue's answer (write 7 on).
    let g = load("deploy_onramp_and_inflight.json");
    let mut rig = Rig::new("rv-write-fail", &g, &["binance", "gate", "revx"], None);
    rig.fail_write_from = Some(7);
    let s = rig.check(1_800_000_000.0);
    assert!(
        s.r.is_ok(),
        "the layer must not abort after a venue call: {:?}",
        s.r
    );
    let placed: Vec<&Value> = s.calls.iter().filter(|c| c[1] == "limit_buy").collect();
    assert_eq!(placed.len(), 1, "{:?}", s.calls);
    // The placed rung is in memory with its venue id, for the run's final save.
    let o = rig.journal.get("depxAAA1800000000r1").unwrap();
    assert!(
        o.order_id.as_deref().is_some_and(|x| !x.is_empty()),
        "{o:?}"
    );
}

#[test]
fn a_failed_journal_write_before_a_placement_places_nothing() {
    let g = load("deploy_onramp_and_inflight.json");
    let mut rig = Rig::new("rv-write-fail-pre", &g, &["binance", "gate", "revx"], None);
    rig.fail_write_from = Some(3);
    let s = rig.check(1_800_000_000.0);
    assert!(s.r.is_ok(), "{:?}", s.r);
    assert!(!has_call(&s, "revx", "limit_buy"), "{:?}", s.calls);
    let o = rig.journal.get("depxAAA1800000000r3").unwrap();
    assert_eq!(o.status, "error");
    assert!(
        s.texts
            .iter()
            .any(|t| t.contains("not placed, journal write failed: disk full")),
        "{:?}",
        s.texts
    );
}
