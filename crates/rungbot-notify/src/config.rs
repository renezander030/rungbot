//! The `notify:` block: which channels exist, and the one-call alert the watchers use.
//!
//! ```yaml
//! notify:
//!   email:
//!     from: alerts@example.com
//!     to: me@example.com
//!     api_key_env: RESEND_API_KEY          # the default
//!     api_key_file: ~/.config/resend.env   # optional fallback
//!   telegram:
//!     chat_id: "123456"
//!     token_env: RUNGBOT_TELEGRAM_TOKEN    # the default
//!   webhook:
//!     url: https://hooks.example/rungbot
//! ```
//!
//! Every block is optional. No secret is ever part of this struct.

use serde::Deserialize;

use crate::email::{EmailConfig, EmailMessage};
use crate::telegram::TelegramConfig;
use crate::webhook::WebhookConfig;
use crate::NotifyError;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notifier {
    #[serde(default)]
    pub email: Option<EmailConfig>,
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
}

/// Where an alert went. The watchers count an alert as delivered when either channel
/// took it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delivery {
    pub email: bool,
    pub telegram: bool,
}

fn py_bool(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

impl Delivery {
    pub fn any(&self) -> bool {
        self.email || self.telegram
    }

    /// The watchers' log line: `sent: email=True telegram=False | <subject>`.
    pub fn log_line(&self, subject: &str) -> String {
        format!(
            "sent: email={} telegram={} | {subject}",
            py_bool(self.email),
            py_bool(self.telegram)
        )
    }
}

impl Notifier {
    pub fn is_configured(&self) -> bool {
        self.email.is_some() || self.telegram.is_some() || self.webhook.is_some()
    }

    /// Apply the `EMAIL_FROM` / `EMAIL_TO` environment overrides to the email block.
    pub fn with_env_overrides(mut self) -> Self {
        self.email = self.email.map(EmailConfig::with_env_overrides);
        self
    }

    /// Email `subject` + `text` (and HTML when given). `Misconfigured` without a block.
    pub fn email(&self, msg: &EmailMessage) -> Result<(), NotifyError> {
        match &self.email {
            Some(e) => e.send(msg),
            None => Err(NotifyError::Misconfigured(
                "no `email` notify channel configured".into(),
            )),
        }
    }

    /// The bot's `send_email`: `true` on success, the reason on stderr otherwise.
    pub fn send_email(&self, subject: &str, text: &str, html: Option<&str>) -> bool {
        let mut msg = EmailMessage::text(subject, text);
        msg.html = html.map(str::to_string);
        match self.email(&msg) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("{e}");
                false
            }
        }
    }

    pub fn telegram(&self, text: &str) -> Result<(), NotifyError> {
        match &self.telegram {
            Some(t) => t.send(text),
            None => Err(NotifyError::Misconfigured(
                "no `telegram` notify channel configured".into(),
            )),
        }
    }

    pub fn webhook(&self, payload: &serde_json::Value) -> Result<(), NotifyError> {
        match &self.webhook {
            Some(w) => w.send(payload),
            None => Err(NotifyError::Misconfigured(
                "no `webhook` notify channel configured".into(),
            )),
        }
    }

    /// The watchers' pattern: the same text by email (with `subject`) and by Telegram.
    /// A channel that is missing or fails counts as not delivered; nothing is printed
    /// except the email failure on stderr, as before.
    pub fn alert(&self, subject: &str, text: &str) -> Delivery {
        Delivery {
            email: self.email.is_some() && self.send_email(subject, text, None),
            telegram: self.telegram(text).is_ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_block_configures_nothing() {
        let n: Notifier = serde_json::from_str("{}").unwrap();
        assert!(!n.is_configured());
        assert_eq!(n, Notifier::default());
    }

    #[test]
    fn a_full_block_deserializes_with_defaults() {
        let n: Notifier = serde_json::from_str(
            r#"{"email": {"from": "a@example.com", "to": "b@example.com"},
                "telegram": {"chat_id": "42"},
                "webhook": {"url": "https://hooks.example/x"}}"#,
        )
        .unwrap();
        let e = n.email.as_ref().unwrap();
        assert_eq!(e.api_key_env, "RESEND_API_KEY");
        assert_eq!(e.timeout_s, 15);
        assert_eq!(e.api_key_file, None);
        let t = n.telegram.as_ref().unwrap();
        assert_eq!(t.token_env, "RUNGBOT_TELEGRAM_TOKEN");
        assert_eq!(n.webhook.as_ref().unwrap().timeout_s, 20);
    }

    #[test]
    fn a_secret_in_the_config_is_rejected_not_silently_used() {
        // There is no field for a key; a pasted one is an error, not a stored secret.
        let r: Result<Notifier, _> =
            serde_json::from_str(r#"{"telegram": {"chat_id": "1", "token": "123:abc"}}"#);
        assert!(r.is_err());
    }

    #[test]
    fn an_alert_with_no_channels_is_delivered_nowhere() {
        let d = Notifier::default().alert("S", "T");
        assert!(!d.any());
    }

    #[test]
    fn the_offline_switch_holds_for_alerts() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_OFFLINE", Some("1")),
            ("RUNGBOT_NOTIFY_TEST_ALERT_KEY", Some("k")),
            ("RUNGBOT_NOTIFY_TEST_ALERT_TG", Some("t")),
        ]);
        let mut e = EmailConfig::new("a@example.com", "b@example.com");
        e.api_key_env = "RUNGBOT_NOTIFY_TEST_ALERT_KEY".into();
        let mut t = TelegramConfig::new("1");
        t.token_env = "RUNGBOT_NOTIFY_TEST_ALERT_TG".into();
        let n = Notifier {
            email: Some(e),
            telegram: Some(t),
            webhook: Some(WebhookConfig::new("https://example.invalid")),
        };
        assert_eq!(n.alert("S", "T"), Delivery::default());
        assert!(matches!(
            n.webhook(&serde_json::json!({})),
            Err(NotifyError::Offline(_))
        ));
    }
}
