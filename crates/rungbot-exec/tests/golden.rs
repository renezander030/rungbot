//! Cross-language parity: outputs of the reference implementation on synthetic inputs,
//! frozen in `tests/golden/*.json`, replayed here.
//!
//! * `ids.json`: client ids, root ids and every venue's id coercion.
//! * `format.json`: float repr, fixed-point amounts, urlencode, error-body text, JSON
//!   dumps.
//! * `venues.json`: every client method of all three venues against a scripted
//!   transport and a fixed clock: the exact requests (URL, headers, signature, body)
//!   and the parsed result or error text. Signatures are compared byte for byte.
//! * `reconcile.json`: reconcile, adoption and settle_cancel over a synthetic journal.
//! * `journal_ops.json`: the journal queries and the archive.
//! * `cex-dir/`: a journal and archive written by the reference, imported.
//!
//! Comparison rules: numbers compare by value (`5` equals `5.0`), and in objects a
//! `null` equals an absent key. Everything else must be identical.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;

use rungbot_exec::http::{Http, Request, Response, SendError, Transport, VenueError};
use rungbot_exec::journal::{self, Journal, Side};
use rungbot_exec::reconcile::{self, Settled, VenueSource};
use rungbot_exec::venue::{venue_id_matches, Balance, Limits, ParsedOrder, Venue};
use rungbot_exec::{ids, import, pyfmt, store, Binance, Credentials, Gate, Revx, RevxCredentials};
use serde_json::{json, Value};

fn golden(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).expect("golden file")).expect("json")
}

/// `a` and `b` agree under the comparison rules; the error names the first path that
/// does not.
fn same(a: &Value, b: &Value, at: &str) -> Result<(), String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x
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
        (x, y) => Err(format!("{at}: reference {x} vs ours {y}")),
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

// ------------------------------------------------------------------------ ids

#[test]
fn ids_match_the_reference() {
    let g = golden("ids.json");
    let mut f = Vec::new();
    for c in g["client_id"].as_array().unwrap() {
        let side = if c["side"] == "buy" {
            Side::Buy
        } else {
            Side::Sell
        };
        // The reference truncates a fractional rung with int().
        let rung = c["rung"].as_f64().unwrap().trunc() as i64;
        let got = journal::client_id(
            c["sym"].as_str().unwrap(),
            side,
            c["ts"].as_f64().unwrap(),
            rung,
        );
        check(
            &mut f,
            same(&c["out"], &json!(got), &format!("client_id {c}")),
        );
    }
    for c in g["root_id"].as_array().unwrap() {
        let got = journal::root_id(c["in"].as_str().unwrap());
        check(
            &mut f,
            same(&c["out"], &json!(got), &format!("root_id {}", c["in"])),
        );
    }
    let outcome = |c: &Value, got: Result<Value, String>, what: &str| -> Result<(), String> {
        match (c.get("ok"), got) {
            (Some(want), Ok(got)) => same(want, &got, &format!("{what} {}", c["in"])),
            (None, Err(_)) => Ok(()),
            (want, got) => Err(format!(
                "{what} {}: reference {want:?}, ours {got:?}",
                c["in"]
            )),
        }
    };
    for c in g["safe_cid"].as_array().unwrap() {
        let got = ids::safe_cid(c["in"].as_str().unwrap())
            .map(|s| json!(s))
            .map_err(|e| e.to_string());
        check(&mut f, outcome(c, got, "safe_cid"));
    }
    for c in g["gate"].as_array().unwrap() {
        let raw = c["in"].as_str().unwrap();
        let got = ids::gate_cid(raw)
            .and_then(|v| Ok(json!({"value": v, "text": ids::safe_gate_text(raw)?})))
            .map_err(|e| e.to_string());
        check(&mut f, outcome(c, got, "gate"));
    }
    for c in g["revx"].as_array().unwrap() {
        let got = ids::safe_revx_cid(c["in"].as_str().unwrap())
            .map(|s| json!(s))
            .map_err(|e| e.to_string());
        check(&mut f, outcome(c, got, "revx"));
    }
    for c in g["venue_id_matches"].as_array().unwrap() {
        let got = venue_id_matches(
            c["exch"].as_str().unwrap(),
            c["venue_cid"].as_str().unwrap(),
            c["cid"].as_str().unwrap(),
        );
        check(
            &mut f,
            same(&c["out"], &json!(got), &format!("venue_id_matches {c}")),
        );
    }
    check(
        &mut f,
        same(
            &g["revx_namespace"],
            &json!(ids::fmt_uuid(ids::REVX_NAMESPACE)),
            "namespace",
        ),
    );
    finish("ids", f);
}

// --------------------------------------------------------------------- format

#[test]
fn text_formats_match_the_reference() {
    let g = golden("format.json");
    let mut f = Vec::new();
    let eq = |f: &mut Vec<String>, want: &Value, got: String, what: String| {
        if want.as_str() != Some(got.as_str()) {
            f.push(format!("{what}: reference {want} vs ours {got:?}"));
        }
    };
    for c in g["repr"].as_array().unwrap() {
        let x = c["x"].as_f64().unwrap();
        eq(&mut f, &c["out"], pyfmt::float_repr(x), format!("repr {x}"));
    }
    for c in g["fixed"].as_array().unwrap() {
        let x = c["x"].as_f64().unwrap();
        eq(&mut f, &c["p2"], pyfmt::fixed(x, 2), format!("%.2f {x}"));
        eq(&mut f, &c["p4"], pyfmt::fixed(x, 4), format!("%.4f {x}"));
        eq(
            &mut f,
            &c["p8s"],
            pyfmt::fixed_stripped(x, 8),
            format!("%.8f-strip {x}"),
        );
    }
    for c in g["urlencode"].as_array().unwrap() {
        let text = c["in"].to_string();
        let pairs: Vec<(&str, String)> = c["in"]
            .as_array()
            .unwrap()
            .iter()
            .map(|kv| (s(&kv[0]), s(&kv[1]).to_string()))
            .collect();
        eq(
            &mut f,
            &c["out"],
            pyfmt::urlencode(&pairs),
            format!("urlencode {text}"),
        );
    }
    for c in g["pyrepr"].as_array().unwrap() {
        let t = c["json"].as_str().unwrap();
        eq(
            &mut f,
            &c["out"],
            pyfmt::body_str(t),
            format!("str(json) {t}"),
        );
    }
    for c in g["dumps_sorted"].as_array().unwrap() {
        eq(
            &mut f,
            &c["out"],
            pyfmt::dumps(&c["obj"], None),
            format!("dumps {}", c["obj"]),
        );
    }
    for c in g["dumps_indent"].as_array().unwrap() {
        eq(
            &mut f,
            &c["out"],
            pyfmt::dumps(&c["obj"], Some(2)),
            format!("dumps indent {}", c["obj"]),
        );
    }
    finish("formats", f);
}

// --------------------------------------------------------------------- venues

struct Scripted {
    queue: RefCell<VecDeque<Result<Response, SendError>>>,
    seen: RefCell<Vec<Request>>,
}

impl Transport for Scripted {
    fn send(&self, req: &Request) -> Result<Response, SendError> {
        self.seen.borrow_mut().push(req.clone());
        self.queue
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(SendError::Network("unscripted request".into())))
    }
}

fn request_json(r: &Request) -> Value {
    let headers: serde_json::Map<String, Value> = r
        .headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), json!(v)))
        .collect();
    json!({"method": r.method, "url": r.url, "headers": headers, "body": r.body, "timeout": r.timeout_s})
}

fn result_json<T: serde::Serialize>(r: Result<T, VenueError>) -> Result<Value, String> {
    r.map(|v| serde_json::to_value(v).unwrap())
        .map_err(|e| e.to_string())
}

// One client per scenario, built once; size is irrelevant here.
#[allow(clippy::large_enum_variant)]
enum Client {
    Gate(Gate),
    Binance(Binance),
    Revx(Revx),
    Public(Box<(Gate, Binance, Revx)>),
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn opt_s(v: &Value) -> Option<&str> {
    v.as_str()
}

fn n(v: &Value) -> f64 {
    v.as_f64().unwrap()
}

fn call(c: &Client, name: &str, a: &[Value]) -> Result<Value, String> {
    let parsed = |v: &dyn Venue, r: Result<Value, VenueError>| {
        result_json(r.and_then(|raw| v.parse_order(&raw)))
    };
    let common = |v: &dyn Venue| -> Option<Result<Value, String>> {
        Some(match name {
            "balances" => result_json(v.balances()),
            "balances_full" => result_json::<BTreeMap<String, Balance>>(v.balances_full()),
            "price" => result_json(v.price(s(&a[0]))),
            "round_amount" | "round_qty" => result_json(v.round_amount(s(&a[0]), n(&a[1]))),
            "round_price" => result_json(v.round_price(s(&a[0]), n(&a[1]))),
            "open_orders" => result_json(v.open_orders(s(&a[0]))),
            "cancel" => result_json(v.cancel(s(&a[0]), s(&a[1]))),
            "order_status" => result_json(v.order_status(s(&a[0]), s(&a[1]))),
            "market_buy" | "market_buy_quote" => {
                parsed(v, v.market_buy(s(&a[0]), n(&a[1]), opt_s(&a[2])))
            }
            "market_sell" => parsed(v, v.market_sell(s(&a[0]), n(&a[1]), opt_s(&a[2]))),
            "limit_buy" => parsed(v, v.limit_buy(s(&a[0]), n(&a[1]), n(&a[2]), opt_s(&a[3]))),
            "limit_sell" => parsed(v, v.limit_sell(s(&a[0]), n(&a[1]), n(&a[2]), opt_s(&a[3]))),
            _ => return None,
        })
    };
    match c {
        Client::Gate(g) => common(g).unwrap_or_else(|| match name {
            "deposits" => result_json(g.deposits(
                a.first().and_then(|v| v.as_str()),
                a.get(1).and_then(|v| v.as_f64()),
            )),
            "pair_info" => result_json(g.pair_info(s(&a[0]))),
            _ => panic!("gate: no call {name}"),
        }),
        Client::Binance(b) => common(b).unwrap_or_else(|| match name {
            "filters" => result_json(b.filters(s(&a[0]))),
            _ => panic!("binance: no call {name}"),
        }),
        Client::Revx(r) => common(r).unwrap_or_else(|| match name {
            "pair_info" => result_json(r.pair_info(s(&a[0]))),
            "filters" => result_json(r.filters(s(&a[0]))),
            _ => panic!("revx: no call {name}"),
        }),
        Client::Public(p) => match name {
            "binance_ticker" => result_json(p.1.ticker(s(&a[0]))),
            "gate_ticker" => result_json(p.0.ticker(s(&a[0]))),
            "revx_ticker" => result_json(p.2.ticker(s(&a[0]))),
            _ => panic!("public: no call {name}"),
        },
    }
}

#[test]
fn venue_clients_match_the_reference_request_for_request() {
    let g = golden("venues.json");
    let mut f = Vec::new();
    let tmp = std::env::temp_dir().join(format!("rungbot-golden-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for sc in g["scenarios"].as_array().unwrap() {
        let venue = sc["venue"].as_str().unwrap();
        let script = Rc::new(Scripted {
            queue: RefCell::new(VecDeque::new()),
            seen: RefCell::new(Vec::new()),
        });
        let clock = Rc::new(Cell::new(0.0));
        let c2 = clock.clone();
        let http = Http::new(script.clone(), Rc::new(move || c2.get()));
        let creds = || Credentials {
            key: s(&sc["key"]).into(),
            secret: s(&sc["secret"]).into(),
        };
        let revx_creds = || {
            let pem = tmp.join(format!("{venue}.pem"));
            std::fs::write(&pem, s(&sc["pem"])).unwrap();
            RevxCredentials {
                key: s(&sc["key"]).into(),
                pem_path: pem,
            }
        };
        let client = match venue {
            "gate" => Client::Gate(Gate::with_http(creds(), http)),
            "binance" => Client::Binance(Binance::with_http(creds(), http)),
            "revx" => Client::Revx(Revx::with_http(revx_creds(), http)),
            _ => Client::Public(Box::new((
                Gate::with_http(creds(), http.clone()),
                Binance::with_http(creds(), http.clone()),
                Revx::with_http(
                    RevxCredentials {
                        key: String::new(),
                        pem_path: PathBuf::new(),
                    },
                    http,
                ),
            ))),
        };
        for (i, step) in sc["steps"].as_array().unwrap().iter().enumerate() {
            let name = step["call"].as_str().unwrap();
            let at = format!("{venue}#{i} {name}{}", step["args"]);
            clock.set(n(&step["now"]));
            script.seen.borrow_mut().clear();
            *script.queue.borrow_mut() = step["responses"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| match r.get("network") {
                    Some(m) => Err(SendError::Network(s(m).into())),
                    None => Ok(Response {
                        status: r["status"].as_u64().unwrap() as u16,
                        body: s(&r["body"]).into(),
                    }),
                })
                .collect();
            let got = call(&client, name, step["args"].as_array().unwrap());
            let left = script.queue.borrow().len();
            if left != 0 {
                f.push(format!("{at}: {left} scripted response(s) unused"));
            }
            let seen: Vec<Value> = script.seen.borrow().iter().map(request_json).collect();
            check(
                &mut f,
                same(&step["requests"], &json!(seen), &format!("{at} requests")),
            );
            match (step.get("ok"), got) {
                (Some(want), Ok(got)) => check(&mut f, same(want, &got, &format!("{at} result"))),
                (None, Err(e)) => {
                    if step["error"].as_str() != Some(e.as_str()) {
                        f.push(format!(
                            "{at}: reference error {} vs ours {e:?}",
                            step["error"]
                        ));
                    }
                }
                (want, got) => f.push(format!(
                    "{at}: reference {} / {:?}, ours {got:?}",
                    want.map(|v| v.to_string()).unwrap_or_default(),
                    step.get("error")
                )),
            }
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    finish("venues", f);
}

#[test]
fn parse_order_matches_the_reference_for_every_venue() {
    let g = golden("venues.json");
    let mut f = Vec::new();
    for (venue, parse) in [
        ("gate", rungbot_exec::gate::parse_order as fn(&Value) -> _),
        ("binance", rungbot_exec::binance::parse_order),
        ("revx", rungbot_exec::revx::parse_order),
    ] {
        for (i, c) in g["parse_order"][venue]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let got = parse(&c["raw"]).map(|p| serde_json::to_value(p).unwrap());
            match (c.get("ok"), got) {
                (Some(want), Ok(got)) => check(&mut f, same(want, &got, &format!("{venue}#{i}"))),
                (None, Err(_)) => {}
                (want, got) => f.push(format!("{venue}#{i}: reference {want:?}, ours {got:?}")),
            }
        }
    }
    finish("parse_order", f);
}

// ------------------------------------------------------------------ reconcile

struct Fake {
    name: &'static str,
    spec: Value,
}

fn venue_err(name: &'static str, msg: &Value) -> VenueError {
    VenueError::new(name, None, s(msg))
}

impl Venue for Fake {
    fn name(&self) -> &'static str {
        self.name
    }
    fn parse_order(&self, _: &Value) -> Result<ParsedOrder, VenueError> {
        unimplemented!()
    }
    fn order_status(&self, _pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        let v = &self.spec["status"][order_id];
        if let Some(m) = v.get("raise") {
            return Err(venue_err(self.name, m));
        }
        if v.is_null() {
            return Err(VenueError::new(
                self.name,
                None,
                format!("no scripted status for {order_id}"),
            ));
        }
        Ok(serde_json::from_value(v.clone()).unwrap())
    }
    fn open_orders(&self, pair: &str) -> Result<Vec<ParsedOrder>, VenueError> {
        let v = &self.spec["open"][pair];
        if let Some(m) = v.get("raise") {
            return Err(venue_err(self.name, m));
        }
        Ok(serde_json::from_value(if v.is_null() { json!([]) } else { v.clone() }).unwrap())
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
    fn limit_sell(&self, _: &str, _: f64, _: f64, _: Option<&str>) -> Result<Value, VenueError> {
        unimplemented!()
    }
}

struct Fakes {
    venues: BTreeMap<String, Fake>,
    missing: Value,
}

impl VenueSource for Fakes {
    fn venue(&self, exch: &str) -> Result<&dyn Venue, String> {
        match self.venues.get(exch) {
            Some(v) => Ok(v),
            None => Err(s(&self.missing[exch]).to_string()),
        }
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn journal_of(v: &Value) -> Journal {
    store::journal_from_map(v.as_object().unwrap().clone()).expect("golden journal reads")
}

fn jv<T: serde::Serialize>(x: &T) -> Value {
    serde_json::to_value(x).unwrap()
}

#[test]
fn reconcile_matches_the_reference() {
    let g = golden("reconcile.json");
    let mut f = Vec::new();
    let fakes = Fakes {
        venues: g["venues"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, spec)| {
                (
                    k.clone(),
                    Fake {
                        name: leak(k),
                        spec: spec.clone(),
                    },
                )
            })
            .collect(),
        missing: g["missing_clients"].clone(),
    };
    let mut j = journal_of(&g["journal"]);
    let r = reconcile::reconcile(&mut j, &fakes, n(&g["now"]));
    assert!(r.changed);
    check(&mut f, same(&g["after"], &jv(&j), "after"));
    let ids = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|o| s(&o["client_id"]).to_string())
            .collect()
    };
    let got_ids: Vec<String> = r.filled.iter().map(|o| o.client_id.clone()).collect();
    if ids(&g["filled"]) != got_ids {
        f.push(format!(
            "filled order: reference {:?}, ours {got_ids:?}",
            ids(&g["filled"])
        ));
    }
    check(&mut f, same(&g["filled"], &jv(&r.filled), "filled"));
    let adopted: Vec<Value> = r
        .adopted
        .iter()
        .map(|a| {
            let mut v = jv(&a.order);
            v["was_quote"] = json!(a.was_quote);
            v
        })
        .collect();
    check(&mut f, same(&g["adopted"], &json!(adopted), "adopted"));

    let r2 = reconcile::reconcile(&mut j, &fakes, n(&g["second"]["now"]));
    check(
        &mut f,
        same(&g["second"]["filled"], &jv(&r2.filled), "second.filled"),
    );
    check(&mut f, same(&g["second"]["after"], &jv(&j), "second.after"));

    let st = &g["settle"];
    let fake = Fake {
        name: "gate",
        spec: st["venue"].clone(),
    };
    for c in st["cases"].as_array().unwrap() {
        let cid = s(&c["cid"]);
        let mut j = journal_of(&st["journal"]);
        let out = reconcile::settle_cancel(&mut j, &fake, cid, n(&st["now"]));
        let got = match out {
            Settled::Booked(o) => jv(&o),
            _ => Value::Null,
        };
        check(&mut f, same(&c["out"], &got, &format!("settle {cid} out")));
        check(
            &mut f,
            same(&c["after"], &jv(&j), &format!("settle {cid} after")),
        );
    }
    finish("reconcile", f);
}

#[test]
fn journal_queries_and_the_archive_match_the_reference() {
    let g = golden("journal_ops.json");
    let q = &g["queries"];
    let now = n(&g["now"]);
    let day = 86_400.0;
    let mut j = journal_of(&g["journal"]);
    let ids =
        |v: Vec<&journal::Order>| json!(v.iter().map(|o| o.client_id.clone()).collect::<Vec<_>>());
    let mut f = Vec::new();
    check(
        &mut f,
        same(&q["open_all"], &ids(j.open_orders(None)), "open_all"),
    );
    check(
        &mut f,
        same(
            &q["open_deploy"],
            &ids(j.open_orders(Some("deploy_buy"))),
            "open_deploy",
        ),
    );
    check(
        &mut f,
        same(&q["errored"], &ids(j.errored(None)), "errored"),
    );
    check(
        &mut f,
        same(
            &q["filled_since_strict"],
            &ids(j.filled_since(now, false)),
            "strict",
        ),
    );
    check(
        &mut f,
        same(
            &q["filled_since_incl"],
            &ids(j.filled_since(now, true)),
            "inclusive",
        ),
    );
    check(
        &mut f,
        same(
            &q["filled_since_old"],
            &ids(j.filled_since(now - 89.0 * day - 1.0, false)),
            "old",
        ),
    );
    check(
        &mut f,
        same(
            &q["unswept_deploy"],
            &ids(j.venue_cancelled_unswept(Some("deploy_buy"))),
            "unswept",
        ),
    );
    check(
        &mut f,
        same(
            &q["unswept_limit"],
            &ids(j.venue_cancelled_unswept(Some("limit_sell"))),
            "unswept_limit",
        ),
    );
    let booked: serde_json::Map<String, Value> = j
        .orders
        .values()
        .map(|o| (o.client_id.clone(), json!(o.booked_base())))
        .collect();
    check(
        &mut f,
        same(&q["booked_base"], &Value::Object(booked), "booked_base"),
    );

    let a = &g["archive"];
    let moved = j.archive_old(n(&a["days"]), now);
    check(
        &mut f,
        same(&a["moved"], &json!(moved.len()), "archived count"),
    );
    let theirs: Vec<&str> = a["archive_text"].as_str().unwrap().lines().collect();
    let ours: Vec<String> = moved.iter().map(store::archive_line).collect();
    for (i, (t, o)) in theirs.iter().zip(&ours).enumerate() {
        // Byte for byte, unless the reference wrote a null the journal does not keep.
        if !t.contains("null") && *t != o {
            f.push(format!("archive line {i}: reference {t}\n  ours {o}"));
        }
        let tv: Value = serde_json::from_str(t).unwrap();
        let ov: Value = serde_json::from_str(o).unwrap();
        check(&mut f, same(&tv, &ov, &format!("archive line {i}")));
    }
    let left: Vec<&String> = j.orders.keys().collect();
    check(&mut f, same(&a["after"], &json!(left), "after archive"));
    j.mark_swept(&["a08".to_string(), "nope".to_string()]);
    check(
        &mut f,
        same(
            &g["mark_swept_a08"],
            &json!(j.get("a08").and_then(|o| o.swept)),
            "swept",
        ),
    );
    finish("journal ops", f);
}

// --------------------------------------------------------------------- import

#[test]
fn a_journal_written_by_the_reference_imports_losslessly() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/cex-dir");
    let meta = golden("cex_dir_meta.json");
    let imp = import::read_dir(&dir).unwrap();
    let r = &imp.report;
    assert!(r.mismatches.is_empty(), "{:?}", r.mismatches);
    assert_eq!(r.rows as u64, meta["rows"].as_u64().unwrap());
    assert_eq!(r.archive_rows as u64, meta["archived"].as_u64().unwrap());
    assert_eq!(
        r.unmodelled.get("custom_field"),
        Some(&1),
        "{:?}",
        r.unmodelled
    );

    // Written out and read back, it is the same journal; the archive is byte-identical.
    let out = std::env::temp_dir().join(format!("rungbot-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let target = out.join("orders-journal.json");
    let apath = import::write(&imp, &target, false).unwrap();
    assert_eq!(store::load_journal(&target).unwrap(), imp.journal);
    // A Windows checkout may have turned the fixture's line endings into CRLF.
    let src_archive = std::fs::read_to_string(dir.join("orders-archive.jsonl"))
        .unwrap()
        .replace("\r\n", "\n");
    if !src_archive.contains("null") {
        assert_eq!(std::fs::read_to_string(&apath).unwrap(), src_archive);
    }
    // A second import of the same source is a no-op; over a journal that moved on it
    // refuses without --force.
    import::write(&imp, &target, false).unwrap();
    let mut moved = imp.journal.clone();
    moved.orders.shift_remove_index(0);
    store::save_journal(&target, &moved).unwrap();
    assert!(import::write(&imp, &target, false)
        .unwrap_err()
        .contains("--force"));
    import::write(&imp, &target, true).unwrap();
    let _ = std::fs::remove_dir_all(&out);
}
