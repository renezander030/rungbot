//! Sending the report somewhere you will actually see it.
//!
//! Two destinations, chosen because neither needs an exchange credential and both work
//! from a `cron` line:
//!
//! * **Webhook** — POST the JSON report to a URL. Whatever is on the other end is your
//!   problem and none of rungbot's, which is the point.
//! * **Telegram** — a bot message. The bot token is read from the environment and never
//!   from the config file, so a watchlist stays safe to paste into an issue.
//!
//! Dedupe lives in [`rungbot_core::notices`]: this module sends what it is given.

use std::fmt;

use rungbot_core::Outcome;

use crate::tickers::USER_AGENT;

const TIMEOUT_S: u64 = 20;
pub const TELEGRAM_TOKEN_ENV: &str = "RUNGBOT_TELEGRAM_TOKEN";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NotifyConfig {
    pub webhook_url: Option<String>,
    pub telegram_chat_id: Option<String>,
}

impl NotifyConfig {
    pub fn is_configured(&self) -> bool {
        self.webhook_url.is_some() || self.telegram_chat_id.is_some()
    }
}

#[derive(Debug)]
pub enum NotifyError {
    Offline(String),
    Failed(String),
    Misconfigured(String),
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotifyError::Offline(m) | NotifyError::Failed(m) | NotifyError::Misconfigured(m) => {
                write!(f, "{m}")
            }
        }
    }
}

fn post(url: &str, body: String, content_type: &str) -> Result<(), NotifyError> {
    if std::env::var("RUNGBOT_OFFLINE").as_deref() == Ok("1") {
        return Err(NotifyError::Offline(
            "RUNGBOT_OFFLINE=1 refuses to send a notification".into(),
        ));
    }
    let resp = minreq::post(url)
        .with_header("User-Agent", USER_AGENT)
        .with_header("Content-Type", content_type)
        .with_timeout(TIMEOUT_S)
        .with_body(body)
        .send()
        .map_err(|e| NotifyError::Failed(format!("{e}")))?;
    if !(200..300).contains(&resp.status_code) {
        let head: String = resp.as_str().unwrap_or("").chars().take(160).collect();
        return Err(NotifyError::Failed(format!("{} {head}", resp.status_code)));
    }
    Ok(())
}

/// A short human message: the lines that would make someone open the app.
pub fn summary(out: &Outcome) -> String {
    let mut lines = Vec::new();
    for b in &out.buys {
        lines.push(format!(
            "BUY {} rung {} — {:.0}% of base @ {}",
            b.row.sym, b.rung, b.pct, b.row.price
        ));
    }
    for s in &out.sells {
        let why = s
            .policy_reason
            .clone()
            .unwrap_or_else(|| format!("rung {}", s.rung));
        lines.push(format!(
            "SELL {} — {:.0}% of position, {why} @ {}",
            s.row.sym, s.pct, s.row.price
        ));
    }
    for e in &out.errors {
        lines.push(format!("! {e}"));
    }
    if lines.is_empty() {
        lines.push("nothing crossed a rung".into());
    }
    lines.join("\n")
}

fn json_escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

pub fn send(
    cfg: &NotifyConfig,
    out: &Outcome,
    text: &str,
) -> Vec<(String, Result<(), NotifyError>)> {
    let mut results = Vec::new();

    if let Some(url) = &cfg.webhook_url {
        let payload = serde_json::json!({
            "source": "rungbot",
            "text": text,
            "buys": out.buys,
            "sells": out.sells,
            "errors": out.errors,
        });
        let body = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
        results.push(("webhook".to_string(), post(url, body, "application/json")));
    }

    if let Some(chat) = &cfg.telegram_chat_id {
        match std::env::var(TELEGRAM_TOKEN_ENV) {
            Ok(tok) if !tok.trim().is_empty() => {
                let url = format!("https://api.telegram.org/bot{}/sendMessage", tok.trim());
                // Plain text on purpose: a price like `1.0*2` must never be read as
                // formatting and silently drop the message.
                let body = format!(
                    "{{\"chat_id\":{},\"text\":{},\"disable_web_page_preview\":true}}",
                    json_escape(chat),
                    json_escape(text)
                );
                results.push(("telegram".to_string(), post(&url, body, "application/json")));
            }
            _ => results.push((
                "telegram".to_string(),
                Err(NotifyError::Misconfigured(format!(
                    "a chat id is set but {TELEGRAM_TOKEN_ENV} is empty or unset"
                ))),
            )),
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use rungbot_core::{analyze, Coin, Config, Price, Settings, Venue};
    use std::collections::BTreeMap;

    fn outcome(price: f64, chg: f64) -> Outcome {
        let cfg = Config::new(
            vec![Coin {
                symbol: "AAA".into(),
                venue: Venue::Binance,
                pair: "AAAUSDT".into(),
                name: String::new(),
                entry: Some(100.0),
                bands: None,
            }],
            Settings::default(),
        )
        .unwrap();
        let mut p = BTreeMap::new();
        p.insert(
            "AAA".to_string(),
            Price {
                price,
                chg_24h: Some(chg),
            },
        );
        analyze(&cfg, &p, &BTreeMap::new(), 1_700_000_000.0)
    }

    #[test]
    fn a_quiet_run_summarises_as_quiet() {
        assert_eq!(summary(&outcome(100.0, 0.5)), "nothing crossed a rung");
    }

    #[test]
    fn a_buy_is_readable_without_opening_the_app() {
        let s = summary(&outcome(88.0, -12.0));
        assert!(s.starts_with("BUY AAA rung 1"), "{s}");
        assert!(s.contains("10% of base"), "{s}");
    }

    #[test]
    fn nothing_is_configured_by_default() {
        assert!(!NotifyConfig::default().is_configured());
        let c = NotifyConfig {
            webhook_url: Some("https://x".into()),
            ..Default::default()
        };
        assert!(c.is_configured());
    }

    #[test]
    fn a_chat_id_without_a_token_is_a_clear_misconfiguration() {
        let _env = crate::testenv::EnvGuard::set(&[(TELEGRAM_TOKEN_ENV, None)]);
        let cfg = NotifyConfig {
            telegram_chat_id: Some("123".into()),
            ..Default::default()
        };
        let r = send(&cfg, &outcome(100.0, 0.5), "hi");
        assert_eq!(r.len(), 1);
        let msg = r[0].1.as_ref().unwrap_err().to_string();
        assert!(msg.contains(TELEGRAM_TOKEN_ENV), "{msg}");
    }

    #[test]
    fn the_offline_guard_covers_sending_too() {
        let _env = crate::testenv::EnvGuard::offline();
        let cfg = NotifyConfig {
            webhook_url: Some("https://example.invalid".into()),
            ..Default::default()
        };
        let r = send(&cfg, &outcome(100.0, 0.5), "hi");
        assert!(
            matches!(r[0].1, Err(NotifyError::Offline(_))),
            "{:?}",
            r[0].1
        );
    }

    #[test]
    fn a_message_with_json_metacharacters_survives_encoding() {
        let tricky = "SELL \"AAA\" \\ 50% \n@ 1.0";
        let encoded = json_escape(tricky);
        let back: String = serde_json::from_str(&encoded).expect("round-trips");
        assert_eq!(
            back, tricky,
            "quotes and backslashes must not corrupt the payload"
        );
    }
}
