//! Replays of outputs frozen from the reference implementation (`tests/golden/*.json`).
//!
//! Each file holds the synthetic inputs (market data, search results, LLM answers) and
//! what the reference wrote and printed for them: ledger and index files byte for
//! byte, stdout, request bodies and prompts. The fakes below serve the same inputs in
//! the same order, so any difference is a behavioural difference.
//!
//! Three literals are deliberately not the reference's, and the recorded text was
//! adjusted once, at recording time, to the port's defaults: the command named in the
//! catalyst stage's "next:" hint, the note's regime line (the reference named its own
//! script files there) and the email button's text (the reference named its note app).
//! The theses' `how` text says "LLM" where the reference named a model. All four are
//! configurable, so the reference wording can be restored in a config.
//!
//! Kept as unit tests so the fixtures travel inside the packaged crate.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use rungbot_notify::EmailConfig;

use crate::clients::{TickTickSink, TICKTICK_API, TICKTICK_LINK};
use crate::date::Date;
use crate::py::{self, Json};
use crate::report::{self, Logger, Mailer, NoteSink, RegimeRead};
use crate::theses::{Step, Theses};
use crate::{
    catalyst, oppscan, survivor, unlocks, BufConsole, Console, Ctx, Fetch, Llm, Paths, Search,
    SearchError,
};

fn golden(name: &str) -> Json {
    let text = match name {
        "pyfmt" => include_str!("../tests/golden/pyfmt.json"),
        "oppscan" => include_str!("../tests/golden/oppscan.json"),
        "catalyst" => include_str!("../tests/golden/catalyst.json"),
        "survivor" => include_str!("../tests/golden/survivor.json"),
        "unlocks" => include_str!("../tests/golden/unlocks.json"),
        "report" => include_str!("../tests/golden/report.json"),
        _ => unreachable!(),
    };
    py::parse(text).expect("golden file parses")
}

fn s<'a>(v: &'a Json, k: &str) -> &'a str {
    v.get(k)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("golden field {k:?} missing"))
}

fn arr<'a>(v: &'a Json, k: &str) -> &'a [Json] {
    v.get(k)
        .and_then(Json::as_arr)
        .unwrap_or_else(|| panic!("golden list {k:?} missing"))
}

fn strings(v: &Json, k: &str) -> Vec<String> {
    arr(v, k).iter().map(Json::display).collect()
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "rungbot-research-golden-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn read(dir: &Path, f: &str) -> String {
    std::fs::read_to_string(dir.join(f)).unwrap()
}

/// Equality that names the first differing line, for readable failures.
#[track_caller]
fn same(got: &str, want: &str, what: &str) {
    if got == want {
        return;
    }
    let (g, w): (Vec<&str>, Vec<&str>) = (got.split('\n').collect(), want.split('\n').collect());
    for i in 0..g.len().max(w.len()) {
        if g.get(i) != w.get(i) {
            panic!(
                "{what}: first difference at line {}\n  rust:      {:?}\n  reference: {:?}",
                i + 1,
                g.get(i),
                w.get(i)
            );
        }
    }
    panic!("{what}: differs");
}

// ---------------------------------------------------------------------------------
// Fakes

#[derive(Default)]
struct FakeFetch {
    routes: Vec<(String, Option<String>)>,
}

impl FakeFetch {
    fn route(mut self, url: &str, body: Option<&str>) -> Self {
        self.routes.push((url.into(), body.map(String::from)));
        self
    }
}

impl Fetch for FakeFetch {
    fn get_json(&self, url: &str, _ua: &str, _t: u64) -> Result<Json, String> {
        match self.routes.iter().find(|(u, _)| u == url) {
            Some((_, Some(body))) => py::parse(body),
            Some((_, None)) => Err("HTTPError: HTTP Error 404: Not Found".into()),
            None => panic!("unexpected GET {url}"),
        }
    }
}

#[derive(Default)]
struct FakeSearch {
    replies: RefCell<VecDeque<Json>>,
    bodies: RefCell<Vec<String>>,
}

impl FakeSearch {
    fn new(script: &[Json]) -> Self {
        FakeSearch {
            replies: RefCell::new(script.iter().cloned().collect()),
            bodies: RefCell::default(),
        }
    }
}

impl Search for FakeSearch {
    fn search(&self, body: &str) -> Result<Json, SearchError> {
        let r = self
            .replies
            .borrow_mut()
            .pop_front()
            .expect("a scripted search reply");
        if r.has("nokey") {
            // The key is checked before the request is built: no body goes out.
            return Err(SearchError::NoKey);
        }
        self.bodies.borrow_mut().push(body.to_string());
        if let Some(e) = r.get("err") {
            return Err(SearchError::Failed(e.display()));
        }
        py::parse(s(&r, "ok")).map_err(SearchError::Failed)
    }
}

#[derive(Default)]
struct FakeLlm {
    replies: RefCell<VecDeque<Json>>,
    prompts: RefCell<Vec<String>>,
}

impl FakeLlm {
    fn new(script: &[Json]) -> Self {
        FakeLlm {
            replies: RefCell::new(script.iter().cloned().collect()),
            prompts: RefCell::default(),
        }
    }
}

impl Llm for FakeLlm {
    fn ask(&self, prompt: &str) -> Result<String, String> {
        self.prompts.borrow_mut().push(prompt.to_string());
        let r = self
            .replies
            .borrow_mut()
            .pop_front()
            .expect("a scripted LLM reply");
        match r.get("err") {
            Some(e) => Err(e.display()),
            None => Ok(s(&r, "ok").to_string()),
        }
    }
}

fn today() -> Date {
    Date::new(2026, 9, 27)
}

fn ctx<'a>(dir: &Path, fetch: &'a dyn Fetch, search: &'a dyn Search, llm: &'a dyn Llm) -> Ctx<'a> {
    Ctx {
        paths: Paths::new(dir),
        fetch,
        search,
        llm: Some(llm),
        today: today(),
    }
}

#[track_caller]
fn same_prompts(got: &[String], want: &[String], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: number of prompts");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        same(g, w, &format!("{what} {i}"));
    }
}

// ---------------------------------------------------------------------------------
// Number and text formatting

#[test]
fn float_text_and_rounding_match_the_reference_on_every_sample() {
    let g = golden("pyfmt");
    let rows = arr(&g, "floats");
    assert!(rows.len() > 300);
    for r in rows {
        let at = s(r, "x");
        let x: f64 = at.parse().unwrap();
        assert_eq!(py::float_repr(x), s(r, "repr"), "repr {at}");
        assert_eq!(py::float_repr(py::round_f(x, 1)), s(r, "r1"), "round1 {at}");
        assert_eq!(py::float_repr(py::round_f(x, 2)), s(r, "r2"), "round2 {at}");
        assert_eq!(py::float_repr(py::round_f(x, 3)), s(r, "r3"), "round3 {at}");
        if x.abs() < 1e38 {
            assert_eq!(py::round_i(x).to_string(), s(r, "r0"), "round0 {at}");
        }
        assert_eq!(py::fixed(x, 0), s(r, "f0"), "%.0f {at}");
        assert_eq!(py::fixed(x, 1), s(r, "f1"), "%.1f {at}");
        assert_eq!(py::fixed(x, 2), s(r, "f2"), "%.2f {at}");
    }
}

// ---------------------------------------------------------------------------------
// oppscan

fn parse_opp_args(argv: &[String]) -> oppscan::Args {
    let mut a = oppscan::Args::default();
    let mut it = argv.iter();
    while let Some(flag) = it.next() {
        if flag == "--json" {
            a.json = true;
            continue;
        }
        let v = it.next().unwrap();
        match flag.as_str() {
            "--limit" => a.limit = v.parse().unwrap(),
            "--min-vol" => a.min_vol = v.parse().unwrap(),
            "--min-dd" => a.min_dd = v.parse().unwrap(),
            "--max-dd" => a.max_dd = v.parse().unwrap(),
            "--min-rank" => a.min_rank = v.parse().unwrap(),
            "--max-rank" => a.max_rank = v.parse().unwrap(),
            other => panic!("unhandled flag {other}"),
        }
    }
    a
}

#[test]
fn oppscan_writes_and_prints_what_the_reference_did() {
    let g = golden("oppscan");
    let fetch = FakeFetch::default().route(oppscan::PAPRIKA, Some(s(&g, "tickers_text")));
    let (search, llm) = (FakeSearch::default(), FakeLlm::default());
    let cases = arr(&g, "cases");
    assert_eq!(cases.len(), 4);
    for (n, c) in cases.iter().enumerate() {
        let dir = tmpdir(&format!("opp{n}"));
        let a = parse_opp_args(&strings(c, "argv"));
        let mut con = BufConsole::default();
        oppscan::run(&ctx(&dir, &fetch, &search, &llm), &a, &mut con).unwrap();
        same(
            &read(&dir, crate::LEDGER),
            s(c, "ledger"),
            &format!("oppscan {n} ledger"),
        );
        same(&con.out, s(c, "stdout"), &format!("oppscan {n} stdout"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------------
// catalyst

#[test]
fn catalyst_search_and_synthesis_match_the_reference() {
    let g = golden("catalyst");
    assert_eq!(s(&g, "today"), today().to_string());
    let dir = tmpdir("cat");
    std::fs::write(dir.join(crate::LEDGER), s(&g, "ledger_in")).unwrap();
    let fetch = FakeFetch::default();

    let sr = g.get("search").unwrap();
    let search = FakeSearch::new(arr(sr, "exa"));
    let llm = FakeLlm::default();
    let mut con = BufConsole::default();
    catalyst::search(&ctx(&dir, &fetch, &search, &llm), &mut con).unwrap();
    assert_eq!(
        *search.bodies.borrow(),
        strings(sr, "bodies"),
        "search bodies"
    );
    same(
        &read(&dir, crate::LEDGER),
        s(sr, "ledger"),
        "catalyst search ledger",
    );
    same(&con.out, s(sr, "stdout"), "catalyst search stdout");

    let sy = g.get("synth").unwrap();
    let llm = FakeLlm::new(arr(sy, "llm"));
    let mut con = BufConsole::default();
    catalyst::synthesize(&ctx(&dir, &fetch, &search, &llm), &mut con).unwrap();
    same_prompts(
        &llm.prompts.borrow(),
        &strings(sy, "prompts"),
        "catalyst prompt",
    );
    same(
        &read(&dir, crate::LEDGER),
        s(sy, "ledger"),
        "catalyst synth ledger",
    );
    same(&con.out, s(sy, "stdout"), "catalyst synth stdout");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------
// survivor

#[test]
fn survivor_index_select_and_run_match_the_reference() {
    let g = golden("survivor");
    let dir = tmpdir("surv");
    let fetch = FakeFetch::default()
        .route(survivor::PROTOCOLS, Some(s(&g, "protocols_text")))
        .route(survivor::FEES, Some(s(&g, "fees_text")))
        .route(oppscan::PAPRIKA, Some(s(&g, "tickers_text")));
    let (search0, llm0) = (FakeSearch::default(), FakeLlm::default());

    let b = g.get("build").unwrap();
    let mut con = BufConsole::default();
    survivor::build_index(&ctx(&dir, &fetch, &search0, &llm0), &mut con).unwrap();
    same(
        &read(&dir, crate::VALUE_INDEX),
        s(b, "index"),
        "value index",
    );
    same(&con.out, s(b, "stdout"), "build-index stdout");

    let sel = g.get("select").unwrap();
    let mut con = BufConsole::default();
    survivor::select(&ctx(&dir, &fetch, &search0, &llm0), &mut con).unwrap();
    same(
        &read(&dir, crate::LEDGER),
        s(sel, "ledger"),
        "select ledger",
    );
    same(&con.out, s(sel, "stdout"), "select stdout");

    let r = g.get("run").unwrap();
    let search = FakeSearch::new(arr(r, "exa"));
    let llm = FakeLlm::new(arr(r, "llm"));
    let mut con = BufConsole::default();
    survivor::run_synth(&ctx(&dir, &fetch, &search, &llm), &mut con).unwrap();
    assert_eq!(
        *search.bodies.borrow(),
        strings(r, "bodies"),
        "search bodies"
    );
    same_prompts(
        &llm.prompts.borrow(),
        &strings(r, "prompts"),
        "survivor prompt",
    );
    same(
        &read(&dir, crate::LEDGER),
        s(r, "ledger"),
        "survivor run ledger",
    );
    same(&con.out, s(r, "stdout"), "survivor run stdout");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------
// unlocks

#[test]
fn unlock_index_and_enrichment_match_the_reference() {
    let g = golden("unlocks");
    let mut fetch = FakeFetch::default();
    for (url, body) in g.get("routes").and_then(Json::as_obj).unwrap() {
        fetch = fetch.route(url, body.as_str());
    }
    let (search, llm) = (FakeSearch::default(), FakeLlm::default());
    let dir = tmpdir("unl");

    let b = g.get("build").unwrap();
    let mut con = BufConsole::default();
    unlocks::build_index(&ctx(&dir, &fetch, &search, &llm), &mut con).unwrap();
    same(
        &read(&dir, crate::UNLOCK_INDEX),
        s(b, "index"),
        "unlock index",
    );
    same(&con.out, s(b, "stdout"), "build-index stdout");
    same(&con.err, s(b, "stderr"), "build-index progress");

    let e = g.get("enrich").unwrap();
    std::fs::write(dir.join(crate::LEDGER), s(e, "ledger_in")).unwrap();
    let now = e.get("now").and_then(Json::as_f64).unwrap() as i64;
    let mut con = BufConsole::default();
    unlocks::enrich(&ctx(&dir, &fetch, &search, &llm), now, &mut con).unwrap();
    same(
        &read(&dir, crate::LEDGER),
        s(e, "ledger"),
        "enriched ledger",
    );
    same(&con.out, s(e, "stdout"), "enrich stdout");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------
// report

/// Replies to the note and mail calls, in the order the reference made them.
struct Replies(RefCell<VecDeque<Json>>);

impl Replies {
    /// `Err("<code> <body[:200]>")` for a scripted HTTP error, else the reply body.
    fn next(&self) -> Result<String, String> {
        let r = self.0.borrow_mut().pop_front().expect("a scripted reply");
        match r.get("code") {
            Some(code) => Err(format!(
                "{} {}",
                code.display(),
                py::head(s(&r, "body"), 200)
            )),
            None => Ok(s(&r, "ok").to_string()),
        }
    }
}

struct ScriptedSink<'a> {
    replies: &'a Replies,
    notes: RefCell<Vec<(String, String)>>,
}

impl NoteSink for ScriptedSink<'_> {
    fn create(&self, title: &str, content: &str) -> Result<Option<String>, String> {
        self.notes
            .borrow_mut()
            .push((title.to_string(), content.to_string()));
        let body = self.replies.next()?;
        let created = if body.trim().is_empty() {
            Json::obj()
        } else {
            py::parse(&body).unwrap()
        };
        let Some(id) = created.get_some("id").map(Json::display) else {
            return Ok(None);
        };
        self.replies.next()?;
        Ok(Some(id))
    }
    fn link(&self, id: &str) -> Option<String> {
        Some(format!("https://ticktick.com/webapp/#p/proj1/tasks/{id}"))
    }
}

struct ScriptedMailer<'a> {
    replies: &'a Replies,
    sent: RefCell<Vec<(String, String)>>,
}

impl Mailer for ScriptedMailer<'_> {
    fn send(&self, subject: &str, html: &str) -> String {
        self.sent
            .borrow_mut()
            .push((subject.to_string(), html.to_string()));
        match self.replies.next() {
            Ok(_) => "sent".into(),
            Err(e) => format!("email failed {e}"),
        }
    }
}

fn regime_of(r: &Json) -> Result<RegimeRead, String> {
    if let Some(f) = r.get("fail") {
        return Err(f.display());
    }
    let d = r.get("data").unwrap();
    let market = match d.get("market") {
        Some(m) if m.truthy() => m.display(),
        _ => "bear".into(),
    };
    let coins = d
        .get("coins")
        .and_then(Json::as_obj)
        .unwrap_or_default()
        .iter()
        .map(|(sym, c)| {
            let sig = c.get("signals").and_then(Json::as_obj).map(|e| {
                e.iter()
                    .map(|(k, v)| (k.clone(), v.truthy()))
                    .collect::<Vec<_>>()
            });
            (sym.clone(), sig)
        })
        .collect();
    Ok(RegimeRead { market, coins })
}

#[test]
fn the_report_matches_the_reference_in_every_scenario() {
    let g = golden("report");
    assert_eq!(s(&g, "today"), today().to_string());
    // The port's defaults: the built-in theses and the default button text.
    let theses = Theses::builtin();
    let email = g.get("email").unwrap();
    let email_cfg = EmailConfig::new(s(email, "from"), s(email, "to"));
    let tick = TickTickSink {
        project_id: s(&g, "note_project").into(),
        token: "tok_test".into(),
        api_base: TICKTICK_API.into(),
        link_template: TICKTICK_LINK.into(),
    };
    let scenarios = arr(&g, "scenarios");
    assert_eq!(scenarios.len(), 6);
    for sc in scenarios {
        let name = s(sc, "name");
        let dir = tmpdir(&format!("rep-{name}"));
        std::fs::write(dir.join(crate::LEDGER), s(sc, "ledger")).unwrap();
        let argv = strings(sc, "argv");
        let flag = |f: &str| argv.iter().any(|a| a == f);
        let opts = report::Opts {
            commit: flag("--commit"),
            refresh: flag("--refresh"),
            no_synth: flag("--no-synth"),
            regime: argv
                .iter()
                .position(|a| a == "--regime")
                .map(|i| argv[i + 1].clone()),
        };
        let replies = Replies(RefCell::new(arr(sc, "replies").iter().cloned().collect()));
        let sink = ScriptedSink {
            replies: &replies,
            notes: RefCell::default(),
        };
        let mailer = ScriptedMailer {
            replies: &replies,
            sent: RefCell::default(),
        };
        let stamp = s(&g, "stamp").to_string();
        let logger = Logger {
            stamp: Box::new(move || stamp.clone()),
            file: None,
        };
        let steps = RefCell::new(Vec::<String>::new());
        let mut step = |st: Step, _: &mut dyn Console| steps.borrow_mut().push(st.to_string());
        let regime_json = sc.get("regime").unwrap().clone();
        let regime = move || regime_of(&regime_json);
        let (fetch, search, llm) = (
            FakeFetch::default(),
            FakeSearch::default(),
            FakeLlm::default(),
        );
        let mut con = BufConsole::default();
        let deps = report::Deps {
            theses: &theses,
            regime: &regime,
            step: &mut step,
            sink: Some(&sink),
            mailer: Some(&mailer),
            logger: &logger,
            link_label: crate::cli::DEFAULT_LINK_LABEL,
        };
        report::run(&ctx(&dir, &fetch, &search, &llm), &opts, deps, &mut con).unwrap();

        same(&con.out, s(sc, "stdout"), &format!("report {name} stdout"));
        assert_eq!(*steps.borrow(), strings(sc, "steps"), "report {name} steps");
        assert_eq!(
            read(&dir, crate::LEDGER),
            s(sc, "ledger"),
            "the report never writes the ledger"
        );

        // Every request the reference sent, rebuilt from what the fakes were handed.
        let (notes, sent) = (sink.notes.borrow(), mailer.sent.borrow());
        let (mut n, mut m) = (0, 0);
        for w in arr(sc, "posts") {
            let url = s(w, "url");
            let req = if url.ends_with("/task") {
                n += 1;
                tick.create_request(&notes[n - 1].0, &notes[n - 1].1)
            } else if url.contains("/task/") {
                tick.kind_request(url.rsplit('/').next().unwrap())
            } else {
                m += 1;
                email_cfg.report_request(&sent[m - 1].0, &sent[m - 1].1, "re_test")
            };
            assert_eq!(req.url, url, "report {name} URL");
            same(
                &req.body,
                s(w, "body"),
                &format!("report {name} body of {url}"),
            );
        }
        assert_eq!(
            (n, m),
            (notes.len(), sent.len()),
            "report {name}: extra sends"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
