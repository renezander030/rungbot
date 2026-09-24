//! Replays of outputs frozen from the Python bot (`tests/golden/*.json`).
//!
//! Kept as unit tests so the fixtures travel inside the packaged crate and no extra test
//! binary has to be linked.

use serde::Deserialize;
use serde_json::Value;

use crate::email::{EmailConfig, EmailMessage};
use crate::signal_notices::{
    filter_signals, subject_suffix, to_json, ExecResult, NoticeState, Signal,
};
use crate::Delivery;

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Sig {
    sym: String,
    rung: i64,
}

impl Signal for Sig {
    fn sym(&self) -> &str {
        &self.sym
    }
    fn rung(&self) -> i64 {
        self.rung
    }
}

#[test]
fn email_bodies_are_byte_identical_to_the_python_sender() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../tests/golden/email_bodies.json")).unwrap();
    assert!(cases.len() >= 4);
    let cfg = EmailConfig::new("alerts@example.com", "me@example.com");
    for c in cases {
        let subject = c["subject"].as_str().unwrap();
        let html = c["html"].as_str();
        let body = match c["kind"].as_str().unwrap() {
            "bot" => {
                let mut msg = EmailMessage::text(subject, c["text"].as_str().unwrap());
                msg.html = html.map(str::to_string);
                cfg.request(&msg, "k").body
            }
            _ => cfg.report_request(subject, html.unwrap(), "k").body,
        };
        assert_eq!(body, c["body"].as_str().unwrap(), "case {subject:?}");
    }
}

#[test]
fn the_telegram_cut_is_the_python_slice() {
    let g: Value = serde_json::from_str(include_str!("../tests/golden/telegram_cut.json")).unwrap();
    let cut = crate::http::head(
        g["input"].as_str().unwrap(),
        crate::telegram::DEFAULT_MAX_CHARS,
    );
    assert_eq!(cut, g["cut_3900"].as_str().unwrap());
}

#[test]
fn delivery_lines_match_the_watchers() {
    let g: Vec<Value> =
        serde_json::from_str(include_str!("../tests/golden/delivery_lines.json")).unwrap();
    for c in g {
        let d = Delivery {
            email: c["email"].as_bool().unwrap(),
            telegram: c["telegram"].as_bool().unwrap(),
        };
        assert_eq!(
            d.log_line(c["subject"].as_str().unwrap()),
            c["line"].as_str().unwrap()
        );
    }
}

#[derive(Deserialize)]
struct Step {
    buys: Vec<Sig>,
    sells: Vec<Sig>,
    execution: Vec<ExecResult>,
    now: f64,
    mail_buys: Vec<Sig>,
    mail_sells: Vec<Sig>,
    suppressed: Vec<String>,
    state: String,
    suffix: String,
}

#[derive(Deserialize)]
struct SuffixCase {
    buys: Vec<Sig>,
    sells: Vec<Sig>,
    execution: Vec<ExecResult>,
    suffix: String,
}

#[derive(Deserialize)]
struct NoticesGolden {
    remind_s: f64,
    steps: Vec<Step>,
    suffix: Vec<SuffixCase>,
}

#[test]
fn signal_notices_replay_the_python_run_byte_for_byte() {
    let g: NoticesGolden =
        serde_json::from_str(include_str!("../tests/golden/signal_notices.json")).unwrap();
    assert!(g.steps.len() >= 10);
    let mut state = NoticeState::new();
    for (i, s) in g.steps.iter().enumerate() {
        let f = filter_signals(
            &mut state,
            &s.buys,
            &s.sells,
            &s.execution,
            s.now,
            g.remind_s,
        );
        let b: Vec<Sig> = f.buys.iter().map(|x| (*x).clone()).collect();
        let se: Vec<Sig> = f.sells.iter().map(|x| (*x).clone()).collect();
        assert_eq!(b, s.mail_buys, "step {i} buys");
        assert_eq!(se, s.mail_sells, "step {i} sells");
        assert_eq!(f.suppressed, s.suppressed, "step {i} suppressed");
        assert_eq!(to_json(&state), s.state, "step {i} state file");
        assert_eq!(
            subject_suffix(&b, &se, &s.execution),
            s.suffix,
            "step {i} suffix"
        );
    }
    for c in &g.suffix {
        assert_eq!(subject_suffix(&c.buys, &c.sells, &c.execution), c.suffix);
    }
}

#[test]
fn a_python_state_file_reads_back_and_rewrites_identically() {
    let g: NoticesGolden =
        serde_json::from_str(include_str!("../tests/golden/signal_notices.json")).unwrap();
    for s in &g.steps {
        let st: NoticeState = serde_json::from_str(&s.state).unwrap();
        assert_eq!(to_json(&st), s.state);
    }
}
