//! Cross-language parity for the whole 30-minute run, replayed from frozen reference
//! outputs (`tests/golden/run.json`).
//!
//! Each scenario is a sequence of runs over one state directory. A run drives
//! [`run::run_locked`] against scripted venues, tickers, daily closes, a mail/Telegram
//! capture, and deploy/audit stubs, with the clock fixed, and must reproduce:
//!
//! * stdout and stderr, line for line;
//! * every mail (subject, text and HTML) and every Telegram ping, byte for byte;
//! * the orders and cancels sent to the venues, and the retry sleeps;
//! * the exit code;
//! * every state file afterwards: the decision log, the journal archive and the notice
//!   file byte for byte, the others (journal, ladder state, P&L, stale-order flags,
//!   BTC level, regime cache and history) by value.
//!
//! The knobs reach the run the way the reference read them: as environment variables.
//! The reference's output is normalised before it is frozen: the temp dir prints as
//! `<dir>`, and the four strings that named the reference itself (its name in the
//! error mail and the lock line, its log path, its halt and cancel commands) print as
//! this runtime's. Scenarios that exercise a reference bug ported fixed say so in their
//! `fixed` note, and the frozen state carries the fixed values.
//!
//! Comparison rules as in `golden.rs`: numbers compare by value, and in objects a `null`
//! equals an absent key.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rungbot_exec::http::VenueError;
use rungbot_exec::reconcile::VenueSource;
use rungbot_exec::run::config::RunConfig;
use rungbot_exec::run::hooks::{AuditHook, AuditReport, DeployHook, HookCtx};
use rungbot_exec::run::market::Market;
use rungbot_exec::run::{self, Deps, Flags, Outbox, RunResult};
use rungbot_exec::store;
use rungbot_exec::venue::{Balance, Limits, ParsedOrder, Venue};
use serde_json::{json, Value};

fn golden() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/run.json");
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

fn text_eq(a: &str, b: &str, at: &str) -> Result<(), String> {
    if a == b {
        return Ok(());
    }
    let (la, lb): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let i = la
        .iter()
        .zip(&lb)
        .position(|(x, y)| x != y)
        .unwrap_or(la.len().min(lb.len()));
    Err(format!(
        "{at} differs at line {}:\n    reference {:?}\n    ours      {:?}",
        i + 1,
        la.get(i).copied().unwrap_or("<end>"),
        lb.get(i).copied().unwrap_or("<end>")
    ))
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn n(v: &Value) -> f64 {
    v.as_f64().unwrap()
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
    fn err(&self, msg: &str) -> VenueError {
        VenueError::new(self.name, None, msg.to_string())
    }

    fn raised(&self, v: &Value) -> Result<(), VenueError> {
        match v.get("raise") {
            Some(m) => Err(self.err(s(m))),
            None => Ok(()),
        }
    }

    fn lim(&self, pair: &str) -> Result<&Value, VenueError> {
        let v = &self.spec["limits"][pair];
        self.raised(v)?;
        Ok(v)
    }

    fn order(
        &self,
        call: &str,
        pair: &str,
        amount: f64,
        cid: &str,
        default: Value,
    ) -> Result<Value, VenueError> {
        self.calls
            .borrow_mut()
            .push(json!({"venue": self.name, "call": call, "pair": pair,
            "amount": amount, "client_id": cid}));
        let v = &self.spec[call][cid];
        if v.is_null() {
            return Ok(default);
        }
        self.raised(v)?;
        Ok(v.clone())
    }
}

fn filled_default(prefix: &str, cid: &str) -> Value {
    json!({"order_id": format!("{prefix}-{cid}"), "status": "filled", "filled": true,
           "base_qty": 0.0, "quote": 0.0, "avg_price": null})
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
            return Err(self.err(&format!("no scripted status for {order_id}")));
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
            return Err(self.err(&format!("no price for {pair}")));
        }
        self.raised(v)?;
        Ok(n(v))
    }
    fn limits(&self, pair: &str) -> Result<Limits, VenueError> {
        let v = self.lim(pair)?;
        Ok(Limits {
            min_base: v["min_base"].as_f64().unwrap_or(0.0),
            min_quote: v["min_quote"].as_f64().unwrap_or(0.0),
        })
    }
    fn round_amount(&self, pair: &str, amount: f64) -> Result<f64, VenueError> {
        let step = self.lim(pair)?["step"].as_f64().unwrap_or(0.0);
        Ok(if step > 0.0 {
            (amount / step).floor() * step
        } else {
            amount
        })
    }
    fn round_price(&self, _: &str, p: f64) -> Result<f64, VenueError> {
        Ok(p)
    }
    fn market_buy(&self, pair: &str, quote: f64, cid: Option<&str>) -> Result<Value, VenueError> {
        let cid = cid.unwrap_or("");
        self.order("market_buy", pair, quote, cid, filled_default("M", cid))
    }
    fn market_sell(&self, pair: &str, base: f64, cid: Option<&str>) -> Result<Value, VenueError> {
        let cid = cid.unwrap_or("");
        self.order("market_sell", pair, base, cid, filled_default("S", cid))
    }
    fn limit_buy(&self, _: &str, _: f64, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        unimplemented!()
    }
    fn limit_sell(
        &self,
        pair: &str,
        base: f64,
        price: f64,
        cid: Option<&str>,
    ) -> Result<Value, VenueError> {
        let cid = cid.unwrap_or("");
        self.calls
            .borrow_mut()
            .push(json!({"venue": self.name, "call": "limit_sell",
            "pair": pair, "amount": base, "price": price, "client_id": cid}));
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
            .ok_or_else(|| format!("'{exch}'"))
    }
}

struct FakeMarket {
    tickers: Value,
    klines: Value,
}

impl Market for FakeMarket {
    fn ticker(&self, _exch: &str, pair: &str) -> Result<(f64, f64), String> {
        match &self.tickers[pair] {
            Value::Array(a) => Ok((n(&a[0]), n(&a[1]))),
            Value::Object(o) => Err(s(&o["raise"]).to_string()),
            _ => Err(format!("ticker {pair}: not in feed")),
        }
    }
    fn closes(&self, exch: &str, symbol: &str, days: usize) -> Result<Vec<f64>, String> {
        match self.klines[symbol].as_array() {
            Some(a) => {
                let v: Vec<f64> = a.iter().map(n).collect();
                Ok(v[v.len().saturating_sub(days)..].to_vec())
            }
            None if exch == "binance" => Err(format!("binance klines {symbol} -> 500")),
            None => Err(format!("gate candlesticks {symbol} -> 500")),
        }
    }
}

#[derive(Default)]
struct Capture {
    ok: bool,
    mails: RefCell<Vec<Value>>,
    pings: RefCell<Vec<String>>,
}

impl Outbox for Capture {
    fn email(&self, subject: &str, text: &str, html: Option<&str>) -> bool {
        self.mails
            .borrow_mut()
            .push(json!({"subject": subject, "text": text, "html": html}));
        self.ok
    }
    fn telegram(&self, text: &str) {
        self.pings
            .borrow_mut()
            .push(text.chars().take(3900).collect());
    }
}

fn result_of(v: &Value) -> RunResult {
    let o = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    RunResult {
        sym: o("sym"),
        side: o("side"),
        mode: o("mode"),
        hk: b("hk"),
        committed: b("committed"),
        fatal: b("fatal"),
        policy: b("policy"),
        deploy: b("deploy"),
        plan: o("plan"),
        done: o("done"),
        skip: o("skip"),
        warn: o("warn"),
        err: o("err"),
    }
}

struct Deploy(Value);

impl DeployHook for Deploy {
    fn check(&mut self, _: &mut HookCtx, execution: &mut Vec<RunResult>) -> Result<(), String> {
        if self.0.is_null() {
            return Ok(());
        }
        if let Some(e) = self.0.get("raise") {
            return Err(s(e).to_string());
        }
        execution.extend(self.0["results"].as_array().unwrap().iter().map(result_of));
        Ok(())
    }
}

struct Audit(Value);

impl AuditHook for Audit {
    fn run(&mut self, _: &mut HookCtx) -> Option<Result<AuditReport, String>> {
        if self.0.is_null() {
            return Some(Ok(AuditReport {
                ok: true,
                ..Default::default()
            }));
        }
        if let Some(e) = self.0.get("raise") {
            return Some(Err(s(e).to_string()));
        }
        let st = &self.0["state"];
        Some(Ok(AuditReport {
            results: self.0["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(result_of)
                .collect(),
            ok: st["ok"].as_bool().unwrap_or(false),
            findings: st["findings"].as_array().map_or(0, Vec::len),
            checked: st["checked"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_i64().unwrap()))
                        .collect()
                })
                .unwrap_or_default(),
        }))
    }
}

// ------------------------------------------------------------------ the replay

const FILES: [(&str, &str); 8] = [
    ("journal", "orders-journal.json"),
    ("ladder", "ladder-state.json"),
    ("pnl", "pnl-ledger.json"),
    ("ttl", "ttl-warned.json"),
    ("notices", "signal-notices.json"),
    ("btc_alert", "btc-alert-state.json"),
    ("regime", "regime-state.json"),
    ("regime_history", "regime-history.json"),
];
const TEXTS: [(&str, &str); 3] = [
    ("decisions", "decisions.jsonl"),
    ("archive", "orders-archive.jsonl"),
    ("notices_text", "signal-notices.json"),
];

/// A scratch directory, removed at the end.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn put(path: &Path, v: &Value) {
    if v.is_null() {
        let _ = std::fs::remove_file(path);
    } else {
        std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
    }
}

fn yaml_for(sc: &Value, dir: &Path) -> String {
    let mut y = format!("state_dir: \"{}\"\nwatchlist:\n", dir.display());
    for w in sc["watchlist"].as_array().unwrap() {
        y.push_str(&format!("  {}: {}\n", s(&w[0]), s(&w[1])));
    }
    y.push_str("names:\n");
    for (k, v) in sc["names"].as_object().unwrap() {
        y.push_str(&format!("  {k}: \"{}\"\n", s(v)));
    }
    y.push_str("entries:\n");
    for (k, v) in sc["entries"].as_object().unwrap() {
        y.push_str(&format!("  {k}: {}\n", n(v)));
    }
    y.push_str("routing:\n");
    for r in sc["routing"].as_array().unwrap() {
        y.push_str(&format!(
            "  {}: {} {} {}\n",
            s(&r[0]),
            s(&r[1]),
            s(&r[2]),
            s(&r[3])
        ));
    }
    y.push_str("regime_kline_source:\n");
    for (k, v) in sc["kline_source"].as_object().unwrap() {
        y.push_str(&format!("  {k}: {} {}\n", s(&v[0]), s(&v[1])));
    }
    y
}

fn env_for(sc: &Value, dir: &Path, extra: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let d = |f: &str| dir.join(f).display().to_string();
    let mut env: BTreeMap<String, String> = sc["knobs"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), s(v).to_string()))
        .collect();
    for (k, f) in [
        ("ORDER_JOURNAL", "orders-journal.json"),
        ("RUN_LOCK", ".run.lock"),
        ("STATE_PATH", "ladder-state.json"),
        ("TTL_WARN_STATE", "ttl-warned.json"),
        ("PNL_LEDGER", "pnl-ledger.json"),
        ("BTC_ALERT_STATE", "btc-alert-state.json"),
        ("HALT_FILE", "halt"),
        ("DECISIONS_LOG", "decisions.jsonl"),
        ("SIGNAL_NOTICES", "signal-notices.json"),
        ("REGIME_STATE", "regime-state.json"),
        ("FROTH_STATE", "froth-state.json"),
        ("SELL_ARM_FILE", "sell-armed"),
    ] {
        env.insert(k.into(), d(f));
    }
    env.insert("DEPLOY_ALLOC_JSON".into(), sc["alloc"].to_string());
    env.extend(extra.clone());
    env
}

fn replay(sc: &Value, failures: &mut Vec<String>) {
    let name = s(&sc["name"]);
    // Some texts cut at a fixed width after the state dir is named in them, so the
    // scratch dir is kept as short as the reference harness's own.
    let base = if Path::new("/tmp").is_dir() {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("rr{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _scratch = Scratch(dir.clone());
    let dstr = dir.display().to_string();
    let init = &sc["initial"];
    for (k, f) in FILES {
        if let Some(v) = init.get(k) {
            put(&dir.join(f), v);
        }
    }
    if let Some(v) = init.get("froth") {
        put(&dir.join("froth-state.json"), v);
    }
    if let Some(raw) = init.get("ladder_raw").and_then(Value::as_str) {
        std::fs::write(dir.join("ladder-state.json"), raw).unwrap();
    }
    let yaml = yaml_for(sc, &dir);

    for (i, run) in sc["runs"].as_array().unwrap().iter().enumerate() {
        let at = format!("{name}[{i}]");
        let now = n(&run["now"]);
        if let Some(t) = run["touch"].as_object() {
            for (f, on) in t {
                if on.as_bool() == Some(true) {
                    std::fs::write(dir.join(f), "").unwrap();
                } else {
                    let _ = std::fs::remove_file(dir.join(f));
                }
            }
        }
        if let Some(p) = run["put"].as_object() {
            for (k, f) in FILES {
                if let Some(v) = p.get(k) {
                    put(&dir.join(f), v);
                }
            }
            if let Some(v) = p.get("froth") {
                put(&dir.join("froth-state.json"), v);
            }
        }
        let mut extra = BTreeMap::new();
        if run["lock_held"].as_bool() == Some(true) {
            extra.insert("RUN_LOCK_WAIT".to_string(), "0".to_string());
        }
        let env = env_for(sc, &dir, &extra);
        let cfg = RunConfig::from_yaml(&yaml, &|k| env.get(k).cloned())
            .unwrap_or_else(|e| panic!("{at}: config: {e}"));

        let calls: Calls = Rc::new(RefCell::new(Vec::new()));
        let venues = Fakes(
            [("binance", "binance"), ("gate", "gate"), ("revx", "revx")]
                .into_iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        Fake {
                            name: v,
                            spec: run["venues"][k].clone(),
                            calls: calls.clone(),
                            seen: RefCell::new(BTreeMap::new()),
                        },
                    )
                })
                .collect(),
        );
        let market = FakeMarket {
            tickers: run["tickers"].clone(),
            klines: run["klines"].clone(),
        };
        let outbox = Capture {
            ok: run["send_ok"].as_bool().unwrap_or(true),
            ..Default::default()
        };
        let mut deploy = Deploy(run["deploy"].clone());
        let mut audit = Audit(run["audit"].clone());
        let sleeps = RefCell::new(Vec::<f64>::new());
        let (mut out, mut err) = (Vec::<u8>::new(), Vec::<u8>::new());
        let flags: Vec<&str> = run["flags"]
            .as_array()
            .map(|a| a.iter().map(s).collect())
            .unwrap_or_default();
        let lock_hold = (run["lock_held"].as_bool() == Some(true)).then(|| {
            let l = store::RunLock::acquire(
                &cfg.lock_path(),
                "another writer",
                std::time::Duration::ZERO,
            )
            .unwrap();
            std::fs::write(cfg.lock_path(), "another writer\n").unwrap();
            l
        });
        let code = {
            let clock = || now;
            let sleep = |x: f64| sleeps.borrow_mut().push(x);
            let mut deps = Deps {
                venues: &venues,
                market: &market,
                outbox: &outbox,
                deploy: &mut deploy,
                audit: &mut audit,
                clock: &clock,
                sleep: &sleep,
                out: &mut out,
                err: &mut err,
            };
            let f = Flags {
                dry_run: flags.contains(&"--dry-run"),
                verbose: flags.contains(&"--verbose"),
            };
            run::run_locked(&cfg, f, &mut deps).unwrap_or(1)
        };
        drop(lock_hold);

        let norm = |t: &str| t.replace(&dstr, "<dir>");
        let mut check = |r: Result<(), String>| {
            if let Err(e) = r {
                failures.push(format!("{at}: {e}"));
            }
        };
        check(same(&run["exit"], &json!(code), "exit"));
        check(text_eq(
            s(&run["stdout"]),
            &norm(&String::from_utf8_lossy(&out)),
            "stdout",
        ));
        if !run["stderr"].is_null() {
            check(text_eq(
                s(&run["stderr"]),
                &norm(&String::from_utf8_lossy(&err)),
                "stderr",
            ));
        }
        let mails: Vec<Value> = outbox
            .mails
            .borrow()
            .iter()
            .map(|m| serde_json::from_str(&norm(&m.to_string())).unwrap())
            .collect();
        let want = run["mails"].as_array().unwrap();
        if want.len() != mails.len() {
            check(Err(format!(
                "mails: reference {} ours {}",
                want.len(),
                mails.len()
            )));
        } else {
            for (j, (a, b)) in want.iter().zip(&mails).enumerate() {
                check(text_eq(
                    s(&a["subject"]),
                    s(&b["subject"]),
                    &format!("mail[{j}].subject"),
                ));
                check(text_eq(
                    s(&a["text"]),
                    s(&b["text"]),
                    &format!("mail[{j}].text"),
                ));
                check(text_eq(
                    s(&a["html"]),
                    s(&b["html"]),
                    &format!("mail[{j}].html"),
                ));
                check(same(
                    &json!(a["html"].is_null()),
                    &json!(b["html"].is_null()),
                    "html set",
                ));
            }
        }
        let pings: Vec<Value> = outbox
            .pings
            .borrow()
            .iter()
            .map(|p| json!(norm(p)))
            .collect();
        check(same(&run["telegram"], &Value::Array(pings), "telegram"));
        check(same(
            &run["calls"],
            &Value::Array(calls.borrow().clone()),
            "calls",
        ));
        check(same(
            &run["sleeps"],
            &json!(sleeps.borrow().clone()),
            "sleeps",
        ));
        for (k, f) in FILES {
            let ours = std::fs::read_to_string(dir.join(f))
                .ok()
                .map(|t| {
                    serde_json::from_str::<Value>(&norm(&t)).unwrap_or(json!({"__unreadable__": t}))
                })
                .unwrap_or(Value::Null);
            let mut theirs = run["files"][k].clone();
            if k == "ladder" && theirs.get("__unreadable__").is_some() {
                theirs = json!({"__unreadable__": std::fs::read_to_string(dir.join(f)).unwrap_or_default()});
            }
            check(same(&theirs, &ours, &format!("files.{k}")));
        }
        for (k, f) in TEXTS {
            let ours = std::fs::read_to_string(dir.join(f)).ok().map(|t| norm(&t));
            match (run["texts"][k].as_str(), ours.as_deref()) {
                (None, None) => {}
                (Some(a), Some(b)) => check(text_eq(a, b, &format!("texts.{k}"))),
                (a, b) => check(Err(format!("texts.{k}: reference {a:?} ours {b:?}"))),
            }
        }
    }
}

#[test]
// The reference texts name state files by POSIX path, and some cut at a fixed width.
#[cfg_attr(windows, ignore = "reference texts carry POSIX paths")]
fn the_run_matches_the_reference_on_every_scenario() {
    let _offline = OfflineGuard::set();
    let g = golden();
    let mut failures = Vec::new();
    for sc in g["scenarios"].as_array().unwrap() {
        replay(sc, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "{} mismatch(es):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// The replay must never reach the network, whatever a fake forgets.
struct OfflineGuard;

impl OfflineGuard {
    fn set() -> OfflineGuard {
        std::env::set_var("RUNGBOT_OFFLINE", "1");
        OfflineGuard
    }
}

/// The documented example under contrib/ loads, and says what it says.
#[test]
fn the_example_config_loads() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/rungbot-run.example.yaml");
    let Ok(text) = std::fs::read_to_string(&p) else {
        return; // not shipped in the published crate
    };
    let c = RunConfig::from_yaml(&text, &|_| None).unwrap();
    assert_eq!(c.trade_mode, "live");
    assert!(c.live_trading_enabled);
    assert_eq!(c.bands_for("AAA"), (15.0, 7.0));
    assert_eq!(c.sell_giveback["AAA"], 30.0);
    assert_eq!(c.sell_trail_arm["AAA"], 3.0);
    assert_eq!(c.sell_no_tranche, vec!["BBB"]);
    assert_eq!(c.sell_tranches, vec![4.0, 8.0, 16.0, 32.0]);
    assert_eq!(
        c.deploy_alloc,
        vec![("AAA".into(), 3.0), ("BBB".into(), 1.0)]
    );
    assert_eq!(c.route("BBB").unwrap().pair, "BBB_USDT");
    assert_eq!(c.regime_kline_source["AAA/USD"].1, "AAAUSDT");
    assert_eq!(c.deploy_zones["AAA"].weights, vec![30.0, 40.0, 30.0]);
    assert_eq!(c.btc_alert_usd, 50000.0);
    assert!(!c.halt_file.to_string_lossy().contains('~'));
    assert_eq!(c.trail_tp, "off");
    assert_eq!(c.deploy, "off");
    let n: rungbot_notify::Notifier = serde_json::from_value(c.notify.clone()).unwrap();
    assert!(n.email.is_some() && n.telegram.is_some());
}
