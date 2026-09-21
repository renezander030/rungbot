//! Public market and protocol-revenue data for the research screen.
//!
//! Two keyless sources, one bulk call each:
//!
//! * **CoinPaprika** `/v1/tickers` — rank, volume and distance from the all-time high
//!   for the whole market.
//! * **DefiLlama** `/protocols` and `/overview/fees` — TVL, category and 30-day fees,
//!   which is what turns "90% off its high" into "90% off its high *and* earning money".
//!
//! Neither needs an account. The screen stays free to run, which matters because it is
//! meant to run weekly and be ignored most weeks.

use std::collections::BTreeMap;

use rungbot_core::research::{Candidate, ValueFacts};
use serde_json::Value;

use crate::tickers::{get_json, TickerError};

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The whole market from CoinPaprika, as screen candidates.
///
/// A coin with no all-time-high figure is skipped rather than defaulted: the entire
/// screen is a function of distance from that high, so guessing it would be inventing
/// the answer.
pub fn market() -> Result<Vec<Candidate>, TickerError> {
    let body = get_json("https://api.coinpaprika.com/v1/tickers")?;
    let rows = body
        .as_array()
        .ok_or_else(|| TickerError::Failed("coinpaprika: not an array".into()))?;

    let mut out = Vec::new();
    for c in rows {
        let q = c.get("quotes").and_then(|q| q.get("USD"));
        let Some(fath) = num(q.and_then(|q| q.get("percent_from_price_ath"))) else {
            continue;
        };
        let (Some(symbol), Some(name)) = (
            c.get("symbol").and_then(|v| v.as_str()),
            c.get("name").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let chg_7d = num(q.and_then(|q| q.get("percent_change_7d")));
        out.push(Candidate {
            symbol: symbol.to_string(),
            name: name.to_string(),
            rank: num(c.get("rank")).unwrap_or(0.0) as u32,
            price: num(q.and_then(|q| q.get("price"))),
            // The feed reports a negative percentage; the screen works in magnitudes.
            drawdown_pct: -fath,
            vol_24h: num(q.and_then(|q| q.get("volume_24h"))).unwrap_or(0.0),
            market_cap: num(q.and_then(|q| q.get("market_cap"))),
            chg_24h: num(q.and_then(|q| q.get("percent_change_24h"))),
            chg_7d,
            chg_1y: num(q.and_then(|q| q.get("percent_change_1y"))),
            basing: chg_7d.is_some_and(|d| d > 0.0),
            value: None,
            verdict: None,
        });
    }
    if out.is_empty() {
        return Err(TickerError::Failed("coinpaprika: no usable rows".into()));
    }
    Ok(out)
}

/// TVL, category and 30-day fees per ticker, from DefiLlama.
///
/// Fees are published per protocol *name* and TVL per *symbol*, so the two are joined on
/// a lower-cased name. A protocol whose fees cannot be matched keeps its TVL and simply
/// has no fee figure, which the gate then reads as "no measurable revenue".
pub fn values() -> Result<BTreeMap<String, ValueFacts>, TickerError> {
    let protos = get_json("https://api.llama.fi/protocols")?;
    let fees = get_json("https://api.llama.fi/overview/fees")?;

    let mut fee_by_name: BTreeMap<String, (Option<f64>, Option<f64>)> = BTreeMap::new();
    if let Some(list) = fees.get("protocols").and_then(|p| p.as_array()) {
        for p in list {
            if let Some(n) = p.get("name").and_then(|v| v.as_str()) {
                fee_by_name.insert(
                    n.trim().to_lowercase(),
                    (num(p.get("total30d")), num(p.get("revenue30d"))),
                );
            }
        }
    }

    let mut out: BTreeMap<String, ValueFacts> = BTreeMap::new();
    let Some(list) = protos.as_array() else {
        return Err(TickerError::Failed(
            "defillama: protocols is not an array".into(),
        ));
    };
    for p in list {
        let Some(sym) = p.get("symbol").and_then(|v| v.as_str()) else {
            continue;
        };
        let sym = sym.trim().to_uppercase();
        if sym.is_empty() || sym == "-" {
            continue;
        }
        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        let (fees_30d, revenue_30d) = fee_by_name
            .get(&name.trim().to_lowercase())
            .copied()
            .unwrap_or((None, None));
        let tvl = num(p.get("tvl"));
        let category = p.get("category").and_then(|v| v.as_str()).map(String::from);

        // A ticker can front several protocols; keep the one earning the most, which is
        // the one the token is actually about.
        let better = match out.get(&sym) {
            Some(prev) => fees_30d.unwrap_or(0.0) > prev.fees_30d.unwrap_or(0.0),
            None => true,
        };
        if better {
            out.insert(
                sym,
                ValueFacts {
                    fees_30d,
                    revenue_30d,
                    tvl,
                    category,
                },
            );
        }
    }
    Ok(out)
}

/// Pipe a brief to an external model and read its answer back.
///
/// rungbot calls no provider itself and holds no model credential: you name a command,
/// it gets the brief on stdin. `claude -p`, `ollama run`, a shell script — all the same
/// to this function.
pub fn ask_model(cmd: &str, prompt: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut parts = cmd.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| "--llm needs a command".to_string())?;
    let mut child = Command::new(program)
        .args(parts)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {program:?}: {e}"))?;
    // Taking stdin means it is dropped, and so closed, at the end of this block — the
    // child needs that EOF or it waits forever.
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(prompt.as_bytes()) {
            // A command that exited or closed stdin before reading is not itself the
            // error worth reporting: its exit status and stderr say far more than a
            // broken pipe does, so fall through and let those speak.
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(format!("cannot write to {program:?}: {e}"));
            }
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{program:?} failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{program:?} exited {}: {}", out.status, err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_feeds_are_blocked_by_the_offline_guard() {
        let _env = crate::testenv::EnvGuard::offline();
        assert!(market().is_err());
        assert!(values().is_err());
    }

    #[test]
    fn the_model_hook_pipes_stdin_to_stdout() {
        let got = ask_model("cat", "BTC is 60% off its high").expect("cat echoes stdin");
        assert_eq!(got, "BTC is 60% off its high");
    }

    #[test]
    fn a_missing_model_command_fails_with_its_name() {
        let e = ask_model("definitely-not-a-real-binary-xyz", "hi").unwrap_err();
        assert!(e.contains("definitely-not-a-real-binary-xyz"), "{e}");
    }

    #[test]
    fn a_failing_model_command_surfaces_its_exit() {
        // `false` exits immediately, so the write to its stdin may or may not land
        // depending on scheduling. Either way the useful report is its exit status,
        // not a broken pipe.
        for _ in 0..25 {
            let e = ask_model("false", "hi").unwrap_err();
            assert!(e.contains("exited"), "{e}");
        }
    }

    #[test]
    fn a_command_that_ignores_its_input_still_succeeds() {
        // `true` never reads stdin; that must not be reported as a failure.
        for _ in 0..25 {
            assert_eq!(ask_model("true", "hi").expect("ignoring input is fine"), "");
        }
    }

    #[test]
    fn an_empty_model_command_is_refused() {
        assert!(ask_model("   ", "hi").is_err());
    }
}
