//! `rungbot-exec snapshot` against goldens frozen from the reference implementation.
//!
//! Each book under `tests/golden/dashboard/<book>/` holds the synthetic inputs
//! (`inputs.json`: balances, prices, the regime reading, every HTTP response keyed by
//! URL, and the run's state files) and what the reference wrote from them
//! (`expected/`): `data.json`, `scenarios.json` and `wallets.json` byte for byte, the
//! log lines, the alert texts, the cache and state files, and every HTTP call in order.
//! The venue clients, the ticker feed and the regime read are fakes fed from the same
//! inputs the reference's fakes were fed.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rungbot_core::watch::json::Json;
use rungbot_exec::dashboard::{self, DashConfig, Io, Parts};
use rungbot_exec::http::{Http, Request, Response, SendError, Transport};
use rungbot_exec::reconcile::VenueSource;
use rungbot_exec::run::config::RunConfig;
use rungbot_exec::run::market::Market;
use rungbot_exec::run::Outbox;
use rungbot_exec::venue::{Balance, Limits, ParsedOrder, Venue};
use rungbot_exec::VenueError;

fn gold() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("dashboard")
}

fn load(p: &Path) -> Json {
    let t = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    serde_json::from_str(&t).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn s(j: &Json) -> String {
    j.as_str().map(str::to_string).unwrap_or_else(|| j.py_str())
}

fn f(j: &Json) -> f64 {
    j.num().expect("a number")
}

// ------------------------------------------------------------------ fakes

struct FakeVenue {
    name: &'static str,
    bal: Option<BTreeMap<String, Balance>>,
    err: String,
    pairs: BTreeMap<String, (f64, f64, i32)>,
}

fn nope(v: &'static str) -> VenueError {
    VenueError::new(v, None, "not used by the dashboard")
}

impl Venue for FakeVenue {
    fn name(&self) -> &'static str {
        self.name
    }
    fn parse_order(&self, _: &serde_json::Value) -> Result<ParsedOrder, VenueError> {
        Err(nope(self.name))
    }
    fn order_status(&self, _: &str, _: &str) -> Result<ParsedOrder, VenueError> {
        Err(nope(self.name))
    }
    fn open_orders(&self, _: &str) -> Result<Vec<ParsedOrder>, VenueError> {
        Err(nope(self.name))
    }
    fn cancel(&self, _: &str, _: &str) -> Result<ParsedOrder, VenueError> {
        Err(nope(self.name))
    }
    fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError> {
        Err(nope(self.name))
    }
    fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError> {
        self.bal
            .clone()
            .ok_or_else(|| VenueError::new(self.name, None, self.err.clone()))
    }
    fn price(&self, _: &str) -> Result<f64, VenueError> {
        Err(nope(self.name))
    }
    fn limits(&self, pair: &str) -> Result<Limits, VenueError> {
        match self.pairs.get(pair) {
            Some((mb, mq, _)) => Ok(Limits {
                min_base: *mb,
                min_quote: *mq,
            }),
            None => Err(VenueError::new(
                self.name,
                None,
                format!("unknown pair {pair}"),
            )),
        }
    }
    fn round_amount(&self, pair: &str, amount: f64) -> Result<f64, VenueError> {
        let p = self.pairs.get(pair).map_or(8, |x| x.2);
        let m = 10f64.powi(p);
        Ok((amount * m).floor() / m)
    }
    fn round_price(&self, _: &str, p: f64) -> Result<f64, VenueError> {
        Ok(p)
    }
    fn market_buy(
        &self,
        _: &str,
        _: f64,
        _: Option<&str>,
    ) -> Result<serde_json::Value, VenueError> {
        Err(nope(self.name))
    }
    fn market_sell(
        &self,
        _: &str,
        _: f64,
        _: Option<&str>,
    ) -> Result<serde_json::Value, VenueError> {
        Err(nope(self.name))
    }
    fn limit_buy(
        &self,
        _: &str,
        _: f64,
        _: f64,
        _: Option<&str>,
    ) -> Result<serde_json::Value, VenueError> {
        Err(nope(self.name))
    }
    fn limit_sell(
        &self,
        _: &str,
        _: f64,
        _: f64,
        _: Option<&str>,
    ) -> Result<serde_json::Value, VenueError> {
        Err(nope(self.name))
    }
}

struct Venues {
    gate: Option<FakeVenue>,
    revx: FakeVenue,
}

impl VenueSource for Venues {
    fn venue(&self, exch: &str) -> Result<&dyn Venue, String> {
        match (exch, &self.gate) {
            ("gate", Some(g)) => Ok(g),
            ("revx", Some(_)) => Ok(&self.revx),
            _ => Err("synthetic: no clients".into()),
        }
    }
}

fn pairs(j: Option<&Json>) -> BTreeMap<String, (f64, f64, i32)> {
    j.map(|p| {
        p.entries()
            .iter()
            .map(|(k, v)| {
                let a = v.items();
                (k.clone(), (f(&a[0]), f(&a[1]), f(&a[2]) as i32))
            })
            .collect()
    })
    .unwrap_or_default()
}

struct Tickers(BTreeMap<String, (f64, f64)>);

impl Market for Tickers {
    fn ticker(&self, _exch: &str, pair: &str) -> Result<(f64, f64), String> {
        self.0
            .get(pair)
            .copied()
            .ok_or_else(|| format!("no ticker for {pair}"))
    }
    fn closes(&self, _: &str, _: &str, _: usize) -> Result<Vec<f64>, String> {
        Err("not used".into())
    }
}

/// Every HTTP response from the book's map, keyed as the reference's fake keyed it.
struct MapTransport {
    map: Json,
    calls: RefCell<Vec<String>>,
}

impl Transport for MapTransport {
    fn send(&self, req: &Request) -> Result<Response, SendError> {
        let key = match &req.body {
            Some(b) => format!("{} {b}", req.url),
            None => req.url.clone(),
        };
        self.calls.borrow_mut().push(key.clone());
        let Some(spec) = self.map.get(&key) else {
            let k: String = key.chars().take(160).collect();
            return Err(SendError::Network(format!("no fixture: {k}")));
        };
        if let Some(n) = spec.get("network") {
            return Err(SendError::Network(s(n)));
        }
        let body = match spec.get("body") {
            Some(Json::Str(t)) => t.clone(),
            Some(other) => other.dumps(None),
            None => String::new(),
        };
        Ok(Response {
            status: f(spec.get("status").expect("status")) as u16,
            body,
        })
    }
}

#[derive(Default)]
struct Pings(RefCell<Vec<String>>);

impl Outbox for Pings {
    fn email(&self, _: &str, _: &str, _: Option<&str>) -> bool {
        false
    }
    fn telegram(&self, text: &str) {
        self.0.borrow_mut().push(text.to_string());
    }
}

// ------------------------------------------------------------------ the book

fn slash(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

fn yaml_map(name: &str, pairs: &[(String, String)], indent: &str) -> String {
    let mut out = format!("{indent}{name}:\n");
    for (k, v) in pairs {
        out.push_str(&format!("{indent}  {k}: \"{v}\"\n"));
    }
    out
}

fn config_yaml(inp: &Json, common: &Json, dir: &Path) -> String {
    let mut y = String::new();
    let mode = s(inp.get("mode").unwrap());
    y.push_str(&format!("trade_mode: {mode}\n"));
    y.push_str(&format!(
        "live_trading_enabled: {}\n",
        s(inp.get("live_enabled").unwrap())
    ));
    y.push_str(&format!("halt_file: \"{}\"\n", slash(&dir.join("halt"))));
    y.push_str(&format!(
        "first_pct: {}\ntarget_pct: {}\n",
        f(inp.get("first_pct").unwrap()),
        f(inp.get("target_pct").unwrap())
    ));
    y.push_str("sell_policy: bull\n");
    y.push_str(&format!(
        "sell_no_tranche: \"{}\"\nsell_no_base_trail: BTC\n",
        s(common.get("no_tranche").unwrap())
    ));
    y.push_str(&format!("state_dir: \"{}\"\n", slash(&dir.join("state"))));
    let pairs_of = |j: &Json| -> Vec<(String, String)> {
        j.entries()
            .iter()
            .map(|(k, v)| (k.clone(), v.py_str()))
            .collect()
    };
    let watch: Vec<(String, String)> = inp
        .get("watchlist")
        .unwrap()
        .items()
        .iter()
        .map(|r| (s(&r.items()[0]), s(&r.items()[1])))
        .collect();
    y.push_str(&yaml_map("watchlist", &watch, ""));
    let routing: Vec<(String, String)> = inp
        .get("routing")
        .unwrap()
        .items()
        .iter()
        .map(|r| {
            let a = r.items();
            (s(&a[0]), format!("{} {} {}", s(&a[1]), s(&a[2]), s(&a[3])))
        })
        .collect();
    y.push_str(&yaml_map("routing", &routing, ""));
    y.push_str(&yaml_map(
        "entries",
        &pairs_of(inp.get("entries").unwrap()),
        "",
    ));
    y.push_str(&yaml_map(
        "deploy_alloc",
        &pairs_of(inp.get("alloc").unwrap()),
        "",
    ));

    y.push_str("dashboard:\n");
    let i = "  ";
    y.push_str(&format!(
        "{i}public_dir: \"{}\"\n",
        slash(&dir.join("public"))
    ));
    y.push_str(&format!("{i}work_dir: \"{}\"\n", slash(&dir.join("work"))));
    y.push_str(&format!(
        "{i}run_log: \"{}\"\n",
        slash(&dir.join("bot.log"))
    ));
    y.push_str(&format!(
        "{i}legacy_before_ts: {}\n",
        f(inp.get("legacy_before_ts").unwrap())
    ));
    y.push_str(&yaml_map(
        "legacy_venues",
        &pairs_of(inp.get("legacy_venues").unwrap()),
        i,
    ));
    y.push_str(&format!(
        "{i}replay_cache: \"{}\"\n",
        slash(&gold().join("replay"))
    ));
    let alts: Vec<String> = common.get("alts").unwrap().items().iter().map(s).collect();
    y.push_str(&format!("{i}alts: \"{}\"\n", alts.join(",")));
    let src: Vec<(String, String)> = common
        .get("src")
        .unwrap()
        .entries()
        .iter()
        .map(|(k, v)| {
            let parts: Vec<String> = v
                .items()
                .iter()
                .map(|x| format!("{} {}", s(&x.items()[0]), s(&x.items()[1])))
                .collect();
            (k.clone(), parts.join(", "))
        })
        .collect();
    y.push_str(&yaml_map("sources", &src, i));
    let ath: Vec<(String, String)> = common
        .get("ath")
        .unwrap()
        .entries()
        .iter()
        .map(|(k, v)| {
            let a = v.items();
            (k.clone(), format!("{} {}", a[0].py_str(), s(&a[1])))
        })
        .collect();
    y.push_str(&yaml_map("ath", &ath, i));
    y.push_str(&format!("{i}scenarios:\n"));
    for w in common.get("scenarios").unwrap().items() {
        let a = w.items();
        y.push_str(&format!("{i}  {}:\n", s(&a[0])));
        y.push_str(&format!("{i}    label: \"{}\"\n", s(&a[1])));
        y.push_str(&format!("{i}    t0: \"{}\"\n", s(&a[2])));
        if !a[3].is_null() {
            y.push_str(&format!("{i}    days: {}\n", a[3].py_str()));
        }
        y.push_str(&format!("{i}    desc: \"{}\"\n", s(&a[4])));
    }
    y.push_str(&yaml_map(
        "forecast_alloc",
        &pairs_of(common.get("forecast_alloc").unwrap()),
        i,
    ));
    // one CoinGecko id per coin, shared by the wallets and the ATH refresh
    let mut ids: Vec<(String, String)> = pairs_of(common.get("cg_ids").unwrap());
    for c in common.get("cosmos").unwrap().items() {
        let a = c.items();
        if !ids.iter().any(|(k, _)| *k == s(&a[0])) {
            ids.push((s(&a[0]), s(&a[4])));
        }
    }
    y.push_str(&yaml_map("coingecko", &ids, i));
    let trail: Vec<String> = common
        .get("trail_only")
        .unwrap()
        .items()
        .iter()
        .map(s)
        .collect();
    y.push_str(&format!("{i}wallet_trail_only: \"{}\"\n", trail.join(",")));
    y.push_str(&format!("{i}wallets:\n"));
    for c in common.get("cosmos").unwrap().items() {
        let a = c.items();
        y.push_str(&format!(
            "{i}  {}:\n{i}    kind: cosmos\n{i}    chain: {}\n{i}    address: {}\n{i}    decimals: {}\n",
            s(&a[0]),
            s(&a[1]),
            s(&a[2]),
            a[3].py_str()
        ));
    }
    let tok: Vec<String> = common.get("tok").unwrap().items().iter().map(s).collect();
    y.push_str(&format!(
        "{i}  VVV:\n{i}    kind: vault\n{i}    address: \"{}\"\n{i}    chain: \"l1 + l2\"\n{i}    unbond_days: 21\n{i}    legs:\n{i}      L1: \"{} {} {}\"\n{i}      L2: \"{} {} {}\"\n",
        s(common.get("evm").unwrap()),
        s(common.get("eth_rpc").unwrap()),
        tok[0],
        tok[1],
        s(common.get("l2_rpc").unwrap()),
        tok[2],
        tok[3]
    ));
    y.push_str(&format!(
        "{i}  BTC:\n{i}    kind: btc\n{i}    address: {}\n{i}    chain: \"bitcoin (cold card)\"\n",
        s(common.get("btc_addr").unwrap())
    ));
    y
}

fn write(p: &Path, v: Option<&Json>, indent: Option<usize>) {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return;
    };
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let body = match v {
        Json::Str(t) => t.clone(),
        other => other.dumps(indent),
    };
    std::fs::write(p, body).unwrap();
}

fn check(book: &str) {
    let inp = load(&gold().join(book).join("inputs.json"));
    let common = load(&gold().join("common.json"));
    let exp = gold().join(book).join("expected");
    let dir = std::env::temp_dir().join(format!("rungbot-dash-{book}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let text = config_yaml(&inp, &common, &dir);
    let cfg = RunConfig::from_yaml(&text, &|_| None).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let env: BTreeMap<String, String> = inp
        .get("scenario_env")
        .unwrap()
        .entries()
        .iter()
        .map(|(k, v)| (k.clone(), s(v)))
        .collect();
    let d = DashConfig::from_run_env(&cfg, &|k| env.get(k).cloned()).unwrap();

    // the run's files, as the reference's sandbox had them
    let state = dir.join("state");
    write(&cfg.ladder_path(), inp.get("ladder_state"), Some(2));
    write(
        &state.join("orders-journal.json"),
        inp.get("journal"),
        Some(2),
    );
    write(&state.join("decisions.jsonl"), inp.get("decisions"), None);
    write(&state.join("audit-state.json"), inp.get("audit"), Some(2));
    let work = dir.join("work");
    write(
        &work.join(".deploy-status.json"),
        inp.get("deploy_status"),
        Some(2),
    );
    write(
        &work.join("manual-fills.json"),
        inp.get("manual_fills"),
        Some(2),
    );
    write(&work.join(".revx-cache.json"), inp.get("revx_cache"), None);
    write(
        &work.join("wallet-targets.json"),
        inp.get("wallet_targets"),
        Some(2),
    );
    write(
        &work.join(".wallets-alert-state.json"),
        inp.get("wallet_state"),
        None,
    );
    if inp.get("wallet_last_good").is_some_and(Json::truthy) {
        write(
            &work.join(".wallets-last-good.json"),
            inp.get("wallet_last_good"),
            None,
        );
    }
    if let Some(ns) = inp.get("log_mtime_ns").and_then(Json::num) {
        let log = dir.join("bot.log");
        std::fs::write(&log, "synthetic\n").unwrap();
        let ns = match inp.get("log_mtime_ns") {
            Some(Json::Int(i)) => *i as u64,
            _ => ns as u64,
        };
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ns);
        std::fs::File::options()
            .write(true)
            .open(&log)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }
    if inp.get("halt").is_some_and(Json::truthy) {
        std::fs::write(dir.join("halt"), "").unwrap();
    }

    // the fakes
    let g = inp.get("gate").unwrap();
    let gate_bal = g.get("balances").filter(|b| !b.is_null()).map(|b| {
        b.entries()
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    Balance {
                        free: f(v.get("free").unwrap()),
                        locked: f(v.get("locked").unwrap()),
                    },
                )
            })
            .collect()
    });
    let venues = Venues {
        gate: g
            .get("clients")
            .is_some_and(Json::truthy)
            .then(|| FakeVenue {
                name: "gate",
                bal: gate_bal,
                err: g.get("balances_error").map(s).unwrap_or_default(),
                pairs: pairs(g.get("pairs")),
            }),
        revx: FakeVenue {
            name: "revx",
            bal: Some(BTreeMap::new()),
            err: String::new(),
            pairs: pairs(inp.get("revx_pairs")),
        },
    };
    let mut tick = BTreeMap::new();
    for r in inp.get("routing").unwrap().items() {
        let a = r.items();
        if let Some(p) = inp.get("prices").unwrap().get(&s(&a[0])) {
            let p = p.items();
            tick.insert(s(&a[2]), (f(&p[0]), f(&p[1])));
        }
    }
    let market = Tickers(tick);
    let regime = |_now: f64| -> Result<(Json, Json), String> {
        match inp.get("regime").filter(|r| !r.is_null()) {
            Some(r) => Ok((r.clone(), inp.get("history").cloned().unwrap_or_default())),
            None => Err(s(inp.get("regime_error").unwrap())),
        }
    };
    let transport = Rc::new(MapTransport {
        map: inp.get("http").cloned().unwrap(),
        calls: RefCell::new(Vec::new()),
    });
    let now = f(inp.get("now").unwrap());
    let http = Http::new(transport.clone(), Rc::new(move || now));
    let auth = |_: &str, ts: i64| -> Result<Vec<(String, String)>, String> {
        Ok(vec![("X-Revx-Timestamp".into(), ts.to_string())])
    };
    let pings = Pings::default();
    let clock = move || now;
    let deploy = |_: &str, _: &mut dyn std::io::Write| -> bool { panic!("no deploy in goldens") };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut io = Io {
        venues: &venues,
        market: &market,
        regime: &regime,
        http,
        revx_auth: &auth,
        outbox: &pings,
        clock: &clock,
        sleep: &|_| {},
        deploy: &deploy,
        out: &mut out,
        err: &mut err,
    };
    let parts = Parts {
        deploy: false,
        ..Parts::default()
    };
    let code = dashboard::cycle(&cfg, &d, parts, &mut io);
    let stderr = String::from_utf8_lossy(&err).into_owned();
    assert_eq!(code, 0, "{book}: exit code; stderr:\n{stderr}");

    // the log, with the output directory named as the reference's golden names it
    let log = String::from_utf8(out)
        .unwrap()
        .replace(&d.public_dir.display().to_string(), "<public>")
        .replace('\\', "/");
    let want_log = std::fs::read_to_string(exp.join("log.txt")).unwrap();
    assert_eq!(log, want_log, "{book}: log lines; stderr:\n{stderr}");

    let files = [
        ("data.json", d.data_path()),
        ("scenarios.json", d.scenarios_path()),
        ("wallets.json", d.wallets_path()),
        ("revx-cache.json", d.revx_cache_path()),
        ("wallets-alert-state.json", d.wallet_state_path()),
        ("wallets-last-good.json", d.wallet_last_good_path()),
    ];
    for (name, got) in files {
        let want = exp.join(name);
        if !want.exists() {
            assert!(!got.exists(), "{book}: {name} should not be written");
            continue;
        }
        let a = std::fs::read_to_string(&want).unwrap();
        let b = std::fs::read_to_string(&got)
            .unwrap_or_else(|e| panic!("{book}: {name} not written ({e}); stderr:\n{stderr}"));
        if a != b {
            let pos = a
                .bytes()
                .zip(b.bytes())
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(b.len()));
            let lo = pos.saturating_sub(200);
            panic!(
                "{book}: {name} differs at byte {pos}\n want …{}…\n  got …{}…",
                &a[lo..(pos + 200).min(a.len())],
                &b[lo..(pos + 200).min(b.len())]
            );
        }
    }
    let alerts: Vec<String> = load(&exp.join("alerts.json"))
        .items()
        .iter()
        .map(s)
        .collect();
    assert_eq!(*pings.0.borrow(), alerts, "{book}: alert texts");
    let calls: Vec<String> = load(&exp.join("calls.json"))
        .items()
        .iter()
        .map(s)
        .collect();
    assert_eq!(
        *transport.calls.borrow(),
        calls,
        "{book}: HTTP calls in order"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_healthy_book_matches_the_reference_byte_for_byte() {
    check("full");
}

#[test]
fn a_degraded_book_reports_its_errors_like_the_reference() {
    check("degraded");
}

#[test]
fn a_stale_onramp_read_serves_the_cache_like_the_reference() {
    check("stale");
}

#[test]
fn the_deploy_failure_streak_matches_the_reference() {
    let steps = load(&gold().join("deploy_status.json"));
    let dir = std::env::temp_dir().join(format!("rungbot-dash-streak-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".deploy-status.json");
    for (i, st) in steps.items().iter().enumerate() {
        let (state, alert) = dashboard::deploy_update(
            &path,
            &s(st.get("result").unwrap()),
            f(st.get("now").unwrap()),
        );
        dashboard::save_deploy_status(&path, &state).unwrap();
        let file = std::fs::read_to_string(&path).unwrap();
        assert_eq!(file, s(st.get("file").unwrap()), "step {i}: file");
        assert_eq!(alert, f(st.get("exit").unwrap()) == 1.0, "step {i}: alert");
        let line = if alert {
            format!(
                "deploy failed {}x in a row\n",
                state.get("consecutive_failures").unwrap().py_str()
            )
        } else {
            String::new()
        };
        assert_eq!(line, s(st.get("stdout").unwrap()), "step {i}: stdout");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The deploy step alone, against a scripted deploy command: two soft failures, the
/// third fails the cycle, a success resets the streak.
#[test]
fn the_third_deploy_failure_in_a_row_fails_the_cycle() {
    let dir = std::env::temp_dir().join(format!("rungbot-dash-cycle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let text = format!(
        "watchlist:\n  AAA: aaa\nrouting:\n  AAA: gate AAA_USDT USDT\nstate_dir: \"{}\"\n\
         dashboard:\n  deploy_command: publish\n",
        slash(&dir)
    );
    let cfg = RunConfig::from_yaml(&text, &|_| None).unwrap();
    let on = |k: &str| (k == "DASHBOARD_DEPLOY").then(|| "1".to_string());
    let d = DashConfig::from_run_env(&cfg, &on).unwrap();
    let venues = Venues {
        gate: None,
        revx: FakeVenue {
            name: "revx",
            bal: None,
            err: String::new(),
            pairs: BTreeMap::new(),
        },
    };
    let market = Tickers(BTreeMap::new());
    let regime = |_: f64| -> Result<(Json, Json), String> { Err("unused".into()) };
    let auth = |_: &str, _: i64| -> Result<Vec<(String, String)>, String> { Ok(Vec::new()) };
    let pings = Pings::default();
    let outcome = RefCell::new(false);
    let deploy = |cmd: &str, out: &mut dyn std::io::Write| -> bool {
        assert_eq!(cmd, "publish");
        let _ = writeln!(out, "published");
        *outcome.borrow()
    };
    let clock = || 1_735_000_000.0;
    let parts = Parts {
        data: false,
        scenarios: false,
        wallets: false,
        deploy: true,
    };
    let run = |ok: bool| -> (i32, String) {
        *outcome.borrow_mut() = ok;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut io = Io {
            venues: &venues,
            market: &market,
            regime: &regime,
            http: Http::default(),
            revx_auth: &auth,
            outbox: &pings,
            clock: &clock,
            sleep: &|_| {},
            deploy: &deploy,
            out: &mut out,
            err: &mut err,
        };
        let code = dashboard::cycle(&cfg, &d, parts, &mut io);
        (code, String::from_utf8(out).unwrap())
    };
    let (c1, l1) = run(false);
    assert_eq!(c1, 0);
    assert!(
        l1.ends_with("published\ndeploy failed (snapshot still regenerated)\n"),
        "{l1}"
    );
    assert_eq!(run(false).0, 0);
    let (c3, l3) = run(false);
    assert_eq!(c3, 1);
    assert!(
        l3.ends_with("deploy failed 3x in a row\ndeploy failure streak -> alerting\n"),
        "{l3}"
    );
    assert_eq!(run(false).0, 0, "the 4th is soft again");
    let (c5, _) = run(true);
    assert_eq!(c5, 0);
    let st = load(&d.deploy_status_path());
    assert_eq!(st.get("consecutive_failures"), Some(&Json::Int(0)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_deploy_command_runs_through_the_shell() {
    let mut out = Vec::new();
    assert!(dashboard::shell_deploy("exit 0", &mut out));
    assert!(!dashboard::shell_deploy("exit 3", &mut out));
}

#[test]
fn deploy_is_skipped_unless_switched_on() {
    let text = "watchlist:\n  AAA: aaa\nrouting:\n  AAA: gate AAA_USDT USDT\n";
    let cfg = RunConfig::from_yaml(text, &|_| None).unwrap();
    let d = DashConfig::from_run_env(&cfg, &|_| None).unwrap();
    assert!(!d.deploy);
    assert!(d.deploy_command.is_none());
    assert_eq!(d.scenarios.seed, 7);
    assert_eq!(d.scenarios.paths, 20_000);
    assert_eq!(d.scenarios.windows.len(), 8);
    assert!(d.scenarios.windows.iter().all(|w| w.days > 0));
}
