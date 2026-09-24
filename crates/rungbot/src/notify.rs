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
//! Dedupe lives in [`rungbot_core::notices`]: this module sends what it is given. The
//! sending itself is `rungbot-notify`, shared with the executor.

use rungbot_core::Outcome;
pub use rungbot_notify::NotifyError;
use rungbot_notify::{TelegramConfig, WebhookConfig};

use crate::tickers::USER_AGENT;

pub const TELEGRAM_TOKEN_ENV: &str = rungbot_notify::telegram::DEFAULT_TOKEN_ENV;

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
        let mut hook = WebhookConfig::new(url);
        hook.user_agent = Some(USER_AGENT.into());
        results.push(("webhook".to_string(), hook.send(&payload)));
    }

    if let Some(chat) = &cfg.telegram_chat_id {
        // Plain text on purpose: a price like `1.0*2` must never be read as formatting
        // and silently drop the message. No 3,900-character cut here: the summary is
        // short, and only Telegram's own 4,096 ceiling applies.
        let mut tg = TelegramConfig::new(chat.as_str());
        tg.token_env = TELEGRAM_TOKEN_ENV.into();
        tg.max_chars = None;
        tg.user_agent = Some(USER_AGENT.into());
        results.push(("telegram".to_string(), tg.send(text)));
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use rungbot_core::{analyze, Coin, Config, Price, Settings, Venue};
    use rungbot_notify::json_escape;
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
