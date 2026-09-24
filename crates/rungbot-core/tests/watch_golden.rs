//! Replays of the watchers against outputs recorded from the reference implementation.
//!
//! Each `tests/golden/watch_*.json` holds inputs and the exact outputs (messages, log
//! lines, state files) the original watchers produced for them, recorded offline on
//! synthetic candles, journals, balances and state. Every string is compared byte for
//! byte. Where this port deliberately fixes a bug, the test says so and checks the fix.

use rungbot_core::regime::RegimeConfig;
use rungbot_core::watch::json::Json;
use rungbot_core::watch::regime_state::{self, Prior, Shift};
use rungbot_core::watch::{
    btc_level, divergence, froth, pyfmt, regime_watch, textwrap, zone, Hints,
};

fn golden(name: &str) -> Json {
    let path = format!("{}/tests/golden/{name}", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn parse(text: &str) -> Option<Json> {
    serde_json::from_str(text).ok()
}

fn at<'a>(j: &'a Json, k: &str) -> &'a Json {
    j.get(k)
        .unwrap_or_else(|| panic!("golden case has no `{k}`"))
}

fn f(j: &Json, k: &str) -> f64 {
    at(j, k)
        .to_float()
        .unwrap_or_else(|| panic!("`{k}` is not a number"))
}

fn s<'a>(j: &'a Json, k: &str) -> &'a str {
    at(j, k)
        .as_str()
        .unwrap_or_else(|| panic!("`{k}` is not a string"))
}

fn b(j: &Json, k: &str) -> bool {
    at(j, k).truthy()
}

fn floats(j: &Json) -> Vec<f64> {
    j.items()
        .iter()
        .map(|v| v.to_float().expect("a number"))
        .collect()
}

fn opt_text(j: &Json, k: &str) -> Option<String> {
    j.get(k).and_then(Json::as_str).map(String::from)
}

// ---- formatting --------------------------------------------------------------------

#[test]
fn python_number_formats() {
    let g = golden("watch_pyfmt.json");
    let mut n = 0;
    for c in at(&g, "cases").items() {
        let v = f(c, "v");
        let got = match s(c, "spec") {
            ".0f" => pyfmt::fixed(v, 0),
            ".1f" => pyfmt::fixed(v, 1),
            ".2f" => pyfmt::fixed(v, 2),
            ".3f" => pyfmt::fixed(v, 3),
            ".4f" => pyfmt::fixed(v, 4),
            ".8f" => pyfmt::fixed(v, 8),
            "+.1f" => pyfmt::signed(v, 1),
            "+.2f" => pyfmt::signed(v, 2),
            "+.4f" => pyfmt::signed(v, 4),
            ",.0f" => pyfmt::comma(v, 0),
            ",.2f" => pyfmt::comma(v, 2),
            "g" => pyfmt::g6(v),
            ".3g" => pyfmt::g(v, 3),
            ".6g" => pyfmt::g(v, 6),
            ",.6g" => pyfmt::comma_g(v, 6),
            "5.1f" => pyfmt::rjust(&pyfmt::fixed(v, 1), 5),
            "9,.2f" => pyfmt::rjust(&pyfmt::comma(v, 2), 9),
            "repr" => pyfmt::repr(v),
            "round2" => pyfmt::repr(pyfmt::round(v, 2)),
            "round8" => pyfmt::repr(pyfmt::round(v, 8)),
            other => panic!("unknown spec {other}"),
        };
        assert_eq!(got, s(c, "out"), "format({v:?}, {:?})", s(c, "spec"));
        n += 1;
    }
    assert!(n > 5000, "the golden covers {n} formats");
    for c in at(&g, "sums").items() {
        let vals = floats(at(c, "vals"));
        assert_eq!(
            pyfmt::sum(vals.iter().copied()),
            f(c, "sum"),
            "sum({vals:?})"
        );
    }
}

#[test]
fn python_textwrap() {
    let g = golden("watch_textwrap.json");
    for c in at(&g, "cases").items() {
        let want: Vec<String> = at(c, "lines").items().iter().map(Json::py_str).collect();
        let w = f(c, "width") as usize;
        assert_eq!(
            textwrap::wrap(s(c, "text"), w),
            want,
            "wrap({:?}, {w})",
            s(c, "text")
        );
    }
}

// ---- regime ------------------------------------------------------------------------

fn cfg(min: f64, ret: f64) -> RegimeConfig {
    RegimeConfig {
        run_min_signals: min as usize,
        run_ret30_min: ret,
    }
}

#[test]
fn regime_run_gate_and_label() {
    let g = golden("watch_regime.json");
    for c in at(&g, "run").items() {
        let closes = floats(at(c, "closes"));
        let (running, sig) = rungbot_core::regime::running_from_series(
            &closes,
            1,
            cfg(f(c, "min_signals"), f(c, "ret30_min")),
        );
        assert_eq!(running, b(c, "running"), "{closes:?}");
        let want = at(c, "signals");
        if sig.insufficient_history {
            assert!(b(want, "insufficient_history"));
        } else {
            assert_eq!(sig.above_sma30, b(want, "above_sma30"));
            assert_eq!(sig.ret30_strong, b(want, "ret30_strong"));
            assert_eq!(sig.fresh_30d_high, b(want, "fresh_30d_high"));
            assert_eq!(sig.higher_lows, b(want, "higher_lows"));
        }
    }
    for c in at(&g, "label").items() {
        let btc = floats(at(c, "btc"));
        for r in at(c, "reads").items() {
            let r = r.items();
            let (breadth, n) = (r[0].num().unwrap() as usize, r[1].num().unwrap() as usize);
            assert_eq!(
                rungbot_core::regime::market_label(&btc, breadth, n).as_str(),
                r[2].as_str().unwrap()
            );
        }
    }
}

fn feed(c: &Json) -> Result<Vec<f64>, String> {
    match c.get("closes").filter(|v| !v.is_null()) {
        Some(v) => Ok(floats(v)),
        None => Err(opt_text(c, "error").unwrap_or_else(|| "fetch failed".into())),
    }
}

#[test]
fn regime_state_matches_the_reference_reading() {
    let g = golden("watch_regime.json");
    for c in at(&g, "compute").items() {
        let coins: Vec<(String, Result<Vec<f64>, String>)> = at(c, "coins")
            .items()
            .iter()
            .map(|x| (s(x, "sym").to_string(), feed(x)))
            .collect();
        let btc = match at(c, "btc") {
            Json::Null => Err(s(c, "btc_error").to_string()),
            v => Ok(floats(v)),
        };
        let got = regime_state::compute_state(&coins, &btc, cfg(3.0, 25.0), f(c, "epoch") as i64);
        assert_eq!(&got, at(c, "out"));
    }
}

/// The one fix: a recomputed, confirmed history forgets the other labels' notices.
fn with_fix(out: &Json, now: f64) -> Json {
    let mut o = out.clone();
    let recomputed = o.get("epoch").and_then(Json::num) == Some(now.trunc());
    let stale = o.get("stale").is_some_and(Json::truthy);
    let label = o
        .get("label")
        .and_then(Json::as_str)
        .unwrap_or("unknown")
        .to_string();
    if recomputed && !stale && label != "unknown" && o.get("confirmed").is_some_and(Json::truthy) {
        if let Some(Json::Obj(n)) = o.get_mut("notified") {
            n.retain(|(k, _)| *k == label);
        }
    }
    o
}

fn hist_feed(c: &Json) -> (Vec<Vec<f64>>, Result<Vec<f64>, String>) {
    let series = at(c, "coins")
        .items()
        .iter()
        .filter_map(|x| x.get("closes").filter(|v| !v.is_null()).map(floats))
        .collect();
    let btc = match c.get("btc") {
        Some(Json::Null) | None => Err(opt_text(c, "btc_error").unwrap_or_default()),
        Some(v) => Ok(floats(v)),
    };
    (series, btc)
}

#[test]
fn regime_label_history() {
    let g = golden("watch_regime.json");
    let mut fixed_cases = 0;
    for c in at(&g, "cases").items() {
        let prior = match at(c, "prior") {
            Json::Null => Prior::Missing,
            Json::Str(t) => parse(t).map(Prior::Parsed).unwrap_or(Prior::Unreadable),
            j => Prior::Parsed(j.clone()),
        };
        let now = f(c, "now");
        let h = regime_state::label_history(
            &prior,
            now,
            f(c, "ttl_h"),
            f(c, "confirm_days") as i64,
            b(c, "force"),
            || hist_feed(c),
        );
        let want = with_fix(at(c, "out"), now);
        if &want != at(c, "out") {
            fixed_cases += 1;
        }
        assert_eq!(h.json, want);
        let after = at(c, "file_after");
        if h.save {
            assert_eq!(h.json, with_fix(after, now));
        } else {
            let before = match &prior {
                Prior::Parsed(j) => Some(j.clone()),
                _ => None,
            };
            assert_eq!(before.as_ref(), Some(after).filter(|a| !a.is_null()));
        }
    }
    let _ = fixed_cases;
}

#[test]
fn regime_announcements_once_per_stage_with_the_return_fix() {
    let g = golden("watch_regime.json");
    let mut differs = 0;
    for c in at(&g, "sequences").items() {
        let btc = floats(at(c, "btc"));
        let coins: Vec<Vec<f64>> = at(c, "coins")
            .items()
            .iter()
            .map(|x| floats(at(x, "closes")))
            .collect();
        let mut file: Option<Json> = None;
        let py = at(c, "py").items();
        for (i, step) in at(c, "fixed").items().iter().enumerate() {
            let n = f(step, "n") as usize;
            let now = f(step, "now");
            let prior = file.clone().map(Prior::Parsed).unwrap_or(Prior::Missing);
            let h = regime_state::label_history(&prior, now, 12.0, 14, b(step, "force"), || {
                (
                    coins.iter().map(|v| v[..n].to_vec()).collect(),
                    Ok(btc[..n].to_vec()),
                )
            });
            let mut hist = h.json;
            if h.save {
                file = Some(hist.clone());
            }
            let shift = regime_state::pending_shift(&hist);
            let want = at(step, "shift");
            assert_eq!(
                shift.as_ref().map(Shift::to_json).unwrap_or(Json::Null),
                *want,
                "step {i} (n={n})"
            );
            if let Some(sh) = &shift {
                if b(step, "delivered") {
                    regime_state::mark_notified(&mut hist, &sh.label, &sh.stage);
                    file = Some(hist.clone());
                }
            }
            assert_eq!(
                file.as_ref().and_then(|f| f.get("notified")),
                step.get("notified").filter(|n| !n.is_null()),
                "step {i}"
            );
            if at(&py[i], "shift") != want {
                differs += 1;
            }
        }
    }
    assert!(
        differs > 0,
        "the goldens include a label returning after another was confirmed"
    );
}

// ---- regime watch + BTC line --------------------------------------------------------

const OPEN: [&str; 6] = [
    "pending",
    "placed",
    "open",
    "new",
    "NEW",
    "PARTIALLY_FILLED",
];

fn open_deploy(journal: &Json) -> Vec<&Json> {
    journal
        .entries()
        .iter()
        .map(|(_, o)| o)
        .filter(|o| {
            o.get("status")
                .and_then(Json::as_str)
                .is_some_and(|st| OPEN.contains(&st))
                && o.get("kind").and_then(Json::as_str) == Some("deploy_buy")
        })
        .collect()
}

fn spot_from(prices: &Json) -> impl FnMut(&str, &str) -> Result<f64, String> + '_ {
    move |_venue: &str, pair: &str| match prices.get(pair) {
        Some(Json::Str(e)) => Err(e.clone()),
        Some(v) => v.to_float().ok_or_else(|| "bad".to_string()),
        None => Err("no ticker".into()),
    }
}

fn shift_of(j: &Json) -> Shift {
    Shift {
        label: s(j, "label").into(),
        prev_label: j.get("prev_label").and_then(Json::as_str).map(String::from),
        stage: s(j, "stage").into(),
        held_days: f(j, "held_days") as i64,
        confirm_days: f(j, "confirm_days") as i64,
        days_covered: f(j, "days_covered") as i64,
    }
}

#[test]
fn regime_watch_message() {
    let g = golden("watch_regime_watch.json");
    let hints = Hints::default();
    for c in at(&g, "cases").items() {
        let zone = match opt_text(c, "zone_error") {
            Some(e) => Err(e),
            None => {
                let rows = open_deploy(at(c, "journal"));
                regime_watch::zone_picture(&rows, spot_from(at(c, "prices")))
            }
        };
        let verdict = opt_text(c, "verdict_raw").and_then(|t| parse(&t));
        let (subject, text) = regime_watch::build_message(
            &shift_of(at(c, "shift")),
            at(c, "reg"),
            zone,
            verdict.as_ref(),
            f(c, "now"),
            &hints,
        );
        assert_eq!(subject, s(c, "subject"));
        assert_eq!(text, s(c, "text"));
    }
}

fn sent_line(email: bool, tg: bool, subject: &str) -> String {
    let py = |x: bool| if x { "True" } else { "False" };
    format!(
        "sent: email={} telegram={} | {subject}\n",
        py(email),
        py(tg)
    )
}

#[test]
fn regime_watch_run() {
    let g = golden("watch_regime_watch.json");
    let hints = Hints::default();
    for c in at(&g, "flows").items() {
        let mut hist = at(c, "hist").clone();
        let now = f(c, "now");
        let (force, dry) = (b(c, "force"), b(c, "dry"));
        let plan = regime_watch::plan(regime_state::pending_shift(&hist), &hist, force);
        let stdout;
        let mut sent = Vec::new();
        match plan {
            regime_watch::Plan::Quiet => stdout = format!("{}\n", regime_watch::QUIET_LINE),
            regime_watch::Plan::Announce(sh) => {
                let (subject, text) = regime_watch::build_message(
                    &sh,
                    at(c, "reg"),
                    Ok(Default::default()),
                    None,
                    now,
                    &hints,
                );
                if dry {
                    stdout = format!("--- would send ---\nSubject: {subject}\n\n{text}\n");
                } else {
                    sent.push(Json::Arr(vec![
                        "email".into(),
                        subject.as_str().into(),
                        text.as_str().into(),
                    ]));
                    sent.push(Json::Arr(vec![
                        "tg".into(),
                        Json::Null,
                        text.as_str().into(),
                    ]));
                    let (e, t) = (b(c, "email_ok"), b(c, "tg_ok"));
                    if e || t {
                        regime_state::mark_notified(&mut hist, &sh.label, &sh.stage);
                    }
                    stdout = sent_line(e, t, &subject);
                }
            }
        }
        assert_eq!(stdout, s(c, "stdout"));
        assert_eq!(&Json::Arr(sent), at(c, "sent"));
        assert_eq!(&hist, at(c, "hist_after"));
    }
}

#[test]
fn btc_line_bands_and_mail() {
    let g = golden("watch_regime_watch.json");
    let hints = Hints::default();
    let mut fires = 0;
    for c in at(&g, "btc").items() {
        let lv = btc_level::Levels {
            alert_usd: f(c, "alert"),
            warn_usd: f(c, "warn"),
        };
        for st in at(c, "steps").items() {
            let stored = opt_text(st, "state_before").and_then(|t| parse(&t));
            let got = btc_level::check(
                at(st, "reg"),
                stored.as_ref(),
                lv,
                b(st, "dry"),
                f(st, "now"),
                &hints,
            );
            let after = opt_text(st, "state_after");
            match &got.state {
                Some(j) => assert_eq!(Some(j.dumps(None)), after),
                None => assert_eq!(after, opt_text(st, "state_before")),
            }
            let want = at(st, "fire");
            match &got.fire {
                Some((subj, text)) => {
                    fires += 1;
                    assert_eq!(subj, want.items()[0].as_str().unwrap());
                    assert_eq!(text, want.items()[1].as_str().unwrap());
                }
                None => assert!(want.is_null(), "{want}"),
            }
        }
    }
    assert!(fires > 5);
}

// ---- zone watch ----------------------------------------------------------------------

#[test]
fn zone_watch_runs() {
    let g = golden("watch_zone.json");
    let hints = Hints::default();
    let zcfg = zone::ZoneConfig {
        alts: at(&g, "alts").items().iter().map(Json::py_str).collect(),
        ..Default::default()
    };
    let routing: Vec<(String, String)> = at(&g, "venue_of")
        .entries()
        .iter()
        .map(|(k, v)| (k.clone(), v.py_str()))
        .collect();
    let mut sent_any = 0;
    for c in at(&g, "cases").items() {
        let now = f(c, "now");
        let state = opt_text(c, "state_raw")
            .and_then(|t| parse(&t))
            .filter(Json::is_obj)
            .unwrap_or_else(zone::empty_state);
        let rows = open_deploy(at(c, "journal"));
        let balances: Vec<(String, Result<Json, String>)> = ["binance", "gate", "revx"]
            .iter()
            .map(|v| {
                let r = match at(c, "balances").get(v) {
                    Some(Json::Str(e)) => Err(e.clone()),
                    Some(j) => Ok(j.clone()),
                    None => Err("no key".into()),
                };
                (v.to_string(), r)
            })
            .collect();
        let reserve = zone::ReserveConfig {
            alloc: at(c, "alloc")
                .entries()
                .iter()
                .map(|(k, v)| (k.clone(), v.to_float().unwrap_or(0.0)))
                .collect(),
            routing: routing.clone(),
        };
        let book = opt_text(c, "book_raw").and_then(|t| parse(&t));
        let inp = zone::Input {
            now,
            state: &state,
            reg: at(c, "reg"),
            deploy_rows: &rows,
            balances: &balances,
            book: book.as_ref(),
            reserve: &reserve,
            cfg: &zcfg,
        };
        let col = zone::collect(&inp, spot_from(at(c, "prices")));
        let (force, dry) = (b(c, "force"), b(c, "dry"));
        let stdout;
        let mut sent = Vec::new();
        let mut written: Option<Json> = None;
        match zone::plan(&state, &col, force, now, &zcfg, &hints) {
            zone::Plan::Quiet { line, state: st } => {
                stdout = format!("{line}\n");
                if !dry {
                    written = Some(st);
                }
            }
            zone::Plan::Send { subject, text } => {
                if dry {
                    stdout = format!("--- would send ---\nSubject: {subject}\n\n{text}\n");
                } else {
                    sent_any += 1;
                    sent.push(Json::Arr(vec![
                        "email".into(),
                        subject.as_str().into(),
                        text.as_str().into(),
                    ]));
                    sent.push(Json::Arr(vec![
                        "tg".into(),
                        Json::Null,
                        text.as_str().into(),
                    ]));
                    let (e, t) = (b(c, "email_ok"), b(c, "tg_ok"));
                    if e || t {
                        written = Some(zone::after_delivery(&state, &col, now));
                    }
                    stdout = sent_line(e, t, &subject);
                }
            }
        }
        assert_eq!(stdout, s(c, "stdout"), "case at {now}");
        assert_eq!(&Json::Arr(sent), at(c, "sent"), "case at {now}");
        match (at(c, "state_after"), written) {
            (Json::Null, None) => {}
            (Json::Null, Some(w)) => assert_eq!(
                Some(w.dumps(Some(1))),
                opt_text(c, "state_raw"),
                "an unchanged state is rewritten byte-identical"
            ),
            (want, Some(w)) => {
                assert_eq!(&w, want, "case at {now}");
            }
            (want, None) => panic!("the reference wrote {want} at {now}"),
        }
    }
    assert!(sent_any > 50);
}

#[test]
fn zone_watch_preview() {
    let g = golden("watch_zone.json");
    let zcfg = zone::ZoneConfig {
        alts: at(&g, "alts").items().iter().map(Json::py_str).collect(),
        ..Default::default()
    };
    assert_eq!(
        zone::preview(1_790_400_000.0, &zcfg, &Hints::default()),
        s(&g, "preview")
    );
}

// ---- froth ---------------------------------------------------------------------------

fn body(src: &Json, k: &str) -> Result<Json, String> {
    let x = at(src, k);
    match x.get("error") {
        Some(e) => Err(e.py_str()),
        None => Ok(at(x, "body").clone()),
    }
}

fn series_tail(src: &Json, k: &str) -> Result<Vec<(f64, f64)>, String> {
    let x = at(src, k);
    if let Some(e) = x.get("error") {
        return Err(e.py_str());
    }
    let n = f(x, "last") as usize;
    let all: Vec<(f64, f64)> = at(src, "series")
        .items()
        .iter()
        .map(|r| (r.items()[0].num().unwrap(), r.items()[1].num().unwrap()))
        .collect();
    Ok(all[all.len() - n..].to_vec())
}

#[test]
fn froth_watch_runs() {
    let g = golden("watch_froth.json");
    let mut sends = 0;
    for c in at(&g, "cases").items() {
        let now = f(c, "now");
        let src = at(c, "src");
        let history = series_tail(src, "history").map(|rows| {
            let closes: Vec<f64> = rows.iter().map(|r| r.1).collect();
            closes[closes.len().saturating_sub(220)..].to_vec()
        });
        let sig = froth::read_signals(
            &body(src, "fng"),
            &body(src, "funding"),
            &body(src, "oi"),
            &history,
        );
        let arm = series_tail(src, "klines").and_then(|rows| froth::btc_arm(&rows, now));
        let st = opt_text(c, "state_raw")
            .and_then(|t| parse(&t))
            .filter(Json::is_obj)
            .unwrap_or_else(Json::obj);
        let (force, dry) = (b(c, "force"), b(c, "dry"));
        let stdout;
        let mut sent = Vec::new();
        let mut written = None;
        match froth::plan(&sig, &st, arm, force, now) {
            froth::Plan::Unchanged { line, state } => {
                stdout = format!("{line}\n");
                if !dry {
                    written = state;
                }
            }
            froth::Plan::Send {
                subject,
                text,
                state,
            } => {
                if dry {
                    stdout = format!("--- would send ---\nSubject: {subject}\n\n{text}\n");
                } else {
                    sends += 1;
                    sent.push(Json::Arr(vec![
                        "email".into(),
                        subject.as_str().into(),
                        text.as_str().into(),
                    ]));
                    sent.push(Json::Arr(vec![
                        "tg".into(),
                        Json::Null,
                        text.as_str().into(),
                    ]));
                    let (e, t) = (b(c, "email_ok"), b(c, "tg_ok"));
                    if e || t {
                        written = Some(state);
                    }
                    stdout = sent_line(e, t, &subject);
                }
            }
        }
        assert_eq!(stdout, s(c, "stdout"), "case at {now}");
        assert_eq!(&Json::Arr(sent), at(c, "sent"), "case at {now}");
        match (at(c, "state_after"), written) {
            (Json::Null, None) => {}
            (Json::Null, Some(w)) => {
                assert_eq!(Some(w.dumps(Some(2))), opt_text(c, "state_raw"))
            }
            (want, Some(w)) => assert_eq!(&w, want, "case at {now}"),
            (want, None) => panic!("the reference wrote {want} at {now}"),
        }
    }
    assert!(sends > 40);
    for c in at(&g, "arm").items() {
        let rows: Vec<(f64, f64)> = at(c, "rows")
            .items()
            .iter()
            .map(|r| (r.items()[0].num().unwrap(), r.items()[1].num().unwrap()))
            .collect();
        assert_eq!(&froth::btc_arm(&rows, f(c, "now")).unwrap(), at(c, "arm"));
    }
    for c in at(&g, "rsi").items() {
        let got = rungbot_core::indicators::rsi(&floats(at(c, "vals")), 14).unwrap_or(0.0);
        assert_eq!(got, f(c, "rsi"));
    }
}

// ---- divergence ------------------------------------------------------------------------

#[test]
fn divergence_runs_match_and_repeat_mails_are_the_fix() {
    let g = golden("watch_divergence.json");
    let hints = Hints::default();
    for c in at(&g, "cases").items() {
        let expect = at(c, "expect");
        if expect.is_null() {
            assert_eq!(s(c, "stdout"), format!("{}\n", divergence::NO_EXPECTATION));
            continue;
        }
        let book = match at(c, "live") {
            Json::Str(e) => Err(e.clone()),
            j => Ok(divergence::Book {
                held: at(j, "held").clone(),
                stable: f(j, "stable"),
            }),
        };
        let prices = match at(c, "px") {
            Json::Str(e) => Err(e.clone()),
            j => Ok(j.clone()),
        };
        let regime = match at(c, "reg") {
            Json::Obj(_) => at(c, "reg")
                .get("market")
                .map(Json::py_str)
                .unwrap_or_else(|| "unknown".into()),
            _ => "unknown".into(),
        };
        let state = at(c, "state");
        let plan = divergence::plan(divergence::Input {
            now: f(c, "now"),
            expect,
            state: (!state.is_null()).then_some(state),
            book,
            prices,
            regime,
            dry_run: b(c, "dry"),
            cfg: divergence::DivergenceConfig::default(),
            hints: &hints,
        });
        let join = |v: &[String]| v.iter().map(|l| format!("{l}\n")).collect::<String>();
        match plan {
            divergence::Plan::Done {
                code,
                stdout,
                stderr,
                state: written,
            } => {
                assert_eq!(code as f64, f(c, "code"));
                assert_eq!(join(&stdout), s(c, "stdout"));
                assert_eq!(join(&stderr), s(c, "stderr"));
                assert!(at(c, "sent").items().is_empty());
                assert_eq!(
                    written.as_ref(),
                    c.get("state_after").filter(|j| !j.is_null())
                );
            }
            divergence::Plan::Send {
                stdout,
                subject,
                body,
                state: on_success,
            } => {
                let ok = b(c, "send_ok");
                let mut out = stdout.clone();
                let mut err = Vec::new();
                if ok {
                    out.push(divergence::SENT_LINE.into());
                } else {
                    err.push(divergence::FAILED_LINE.to_string());
                }
                assert_eq!(join(&out), s(c, "stdout"));
                assert_eq!(join(&err), s(c, "stderr"));
                assert_eq!(f(c, "code"), if ok { 0.0 } else { 1.0 });
                let sent = at(c, "sent").items();
                assert_eq!(sent.len(), 1);
                assert_eq!(sent[0].items()[0].as_str(), Some(subject.as_str()));
                assert_eq!(sent[0].items()[1].as_str(), Some(body.as_str()));
                // The fix: the reference leaves the state alone and mails again tomorrow;
                // the port records which guards it mailed.
                let mut want = state.clone();
                assert!(want.get("alerted").is_none());
                want.set(
                    "alerted",
                    on_success.get("alerted").cloned().unwrap_or(Json::Null),
                );
                assert_eq!(on_success, want);
                let again = divergence::plan(divergence::Input {
                    now: f(c, "now"),
                    expect,
                    state: Some(&on_success),
                    book: Ok(divergence::Book {
                        held: at(at(c, "live"), "held").clone(),
                        stable: f(at(c, "live"), "stable"),
                    }),
                    prices: Ok(at(c, "px").clone()),
                    regime: "chop".into(),
                    dry_run: false,
                    cfg: divergence::DivergenceConfig::default(),
                    hints: &hints,
                });
                assert!(
                    !matches!(again, divergence::Plan::Send { .. }),
                    "the same guard is not mailed twice"
                );
            }
        }
    }
}
