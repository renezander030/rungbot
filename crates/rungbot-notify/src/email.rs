//! Email through the Resend HTTP API.
//!
//! One `POST https://api.resend.com/emails` with a JSON body
//! `{"from": ..., "to": [...], "subject": ..., "text": ...[, "html": ...]}` and a Bearer
//! key. The body is written byte for byte as the Python bot's `json.dumps` wrote it
//! (see [`crate::pyjson`]), so a captured request can be diffed against the old one.
//!
//! Two flavours, because the reference bot had two senders:
//!
//! * [`EmailConfig::send_email`] — the bot's `send_email(subject, text, html=None)`:
//!   text always, HTML only when non-empty, 15 s timeout, `true` on a 2xx, and a failure
//!   printed to stderr in the old wording. No retry.
//! * [`EmailConfig::send_report_html`] — the weekly report's HTML-only sender, which
//!   returns a status string (`sent`, `no resend key`, `email failed <code> <body>`).
//!
//! The Resend key is read from the environment variable the config names (default
//! `RESEND_API_KEY`), else from an env-style file (`RESEND_API_KEY=...`, quotes
//! stripped). It is never stored in the config and never printed.

use std::path::PathBuf;

use serde::Deserialize;

use crate::http::{head, HttpRequest};
use crate::{pyjson, NotifyError};

pub const RESEND_URL: &str = "https://api.resend.com/emails";
pub const DEFAULT_KEY_ENV: &str = "RESEND_API_KEY";
pub const DEFAULT_TIMEOUT_S: u64 = 15;
/// The environment overrides the reference wrappers exported.
pub const FROM_ENV: &str = "EMAIL_FROM";
pub const TO_ENV: &str = "EMAIL_TO";
/// The weekly report's sender used a browser User-Agent and a longer timeout.
pub const REPORT_USER_AGENT: &str = "Mozilla/5.0";
pub const REPORT_TIMEOUT_S: u64 = 45;

fn default_key_env() -> String {
    DEFAULT_KEY_ENV.into()
}
fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_S
}
fn default_endpoint() -> String {
    RESEND_URL.into()
}

#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailConfig {
    pub from: String,
    pub to: String,
    /// Name of the environment variable holding the Resend key.
    #[serde(default = "default_key_env")]
    pub api_key_env: String,
    /// Optional env-style file read when the variable is unset or empty. `~/` expands.
    #[serde(default)]
    pub api_key_file: Option<String>,
    /// Defaults to [`crate::USER_AGENT`].
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
}

impl std::fmt::Debug for EmailConfig {
    // Addresses are personal; a debug print of a config must not leak them into a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailConfig")
            .field("from", &"<set>")
            .field("to", &"<set>")
            .field("api_key_env", &self.api_key_env)
            .field("api_key_file", &self.api_key_file)
            .field("user_agent", &self.user_agent)
            .field("timeout_s", &self.timeout_s)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// What to send. `text: None` omits the text part (the report flavour); an empty or
/// absent `html` omits the HTML part, as `if html:` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailMessage {
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
}

impl EmailMessage {
    pub fn text(subject: impl Into<String>, text: impl Into<String>) -> Self {
        EmailMessage {
            subject: subject.into(),
            text: Some(text.into()),
            html: None,
        }
    }

    pub fn html_only(subject: impl Into<String>, html: impl Into<String>) -> Self {
        EmailMessage {
            subject: subject.into(),
            text: None,
            html: Some(html.into()),
        }
    }

    pub fn with_html(mut self, html: impl Into<String>) -> Self {
        self.html = Some(html.into());
        self
    }
}

/// Why a send did not happen, before it is worded for one flavour or the other.
enum Attempt {
    NoKey,
    Offline(String),
    Status(u16, String),
    Transport(String),
}

impl EmailConfig {
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        EmailConfig {
            from: from.into(),
            to: to.into(),
            api_key_env: default_key_env(),
            api_key_file: None,
            user_agent: None,
            timeout_s: DEFAULT_TIMEOUT_S,
            endpoint: default_endpoint(),
        }
    }

    /// Apply `EMAIL_FROM` / `EMAIL_TO` when set, as the reference bot read them.
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(v) = std::env::var(FROM_ENV) {
            self.from = v;
        }
        if let Ok(v) = std::env::var(TO_ENV) {
            self.to = v;
        }
        self
    }

    /// The key: the named variable if non-empty, else the key file's line.
    pub fn resolve_key(&self) -> Option<String> {
        if let Ok(v) = std::env::var(&self.api_key_env) {
            if !v.is_empty() {
                return Some(v);
            }
        }
        let path = expand_home(self.api_key_file.as_deref()?)?;
        let body = std::fs::read_to_string(path).ok()?;
        let prefix = format!("{}=", self.api_key_env);
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix(&prefix) {
                let v = rest.trim().trim_matches('"').trim_matches('\'');
                return (!v.is_empty()).then(|| v.to_string());
            }
        }
        None
    }

    /// The JSON body, byte-identical to the Python sender's `json.dumps(body)`.
    pub fn body(&self, msg: &EmailMessage) -> String {
        let mut pairs = vec![
            ("from", pyjson::string(&self.from)),
            ("to", pyjson::array(&[pyjson::string(&self.to)])),
            ("subject", pyjson::string(&msg.subject)),
        ];
        if let Some(t) = &msg.text {
            pairs.push(("text", pyjson::string(t)));
        }
        if let Some(h) = msg.html.as_deref().filter(|h| !h.is_empty()) {
            pairs.push(("html", pyjson::string(h)));
        }
        pyjson::object(&pairs)
    }

    /// The bot flavour's request: Bearer, JSON, the configured User-Agent and timeout.
    pub fn request(&self, msg: &EmailMessage, key: &str) -> HttpRequest {
        let ua = self.user_agent.as_deref().unwrap_or(crate::USER_AGENT);
        HttpRequest::post(&self.endpoint, self.body(msg), self.timeout_s)
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", ua)
            .secret(key)
    }

    /// The report flavour's request: browser User-Agent, 45 s, HTML only.
    pub fn report_request(&self, subject: &str, html: &str, key: &str) -> HttpRequest {
        let msg = EmailMessage::html_only(subject, html);
        HttpRequest::post(&self.endpoint, self.body(&msg), REPORT_TIMEOUT_S)
            .header("Content-Type", "application/json")
            .header("User-Agent", REPORT_USER_AGENT)
            .header("Authorization", format!("Bearer {key}"))
            .secret(key)
    }

    fn attempt(&self, build: impl FnOnce(&str) -> HttpRequest) -> Result<(), Attempt> {
        let key = self.resolve_key().ok_or(Attempt::NoKey)?;
        let req = build(&key);
        match req.send() {
            Ok(r) if r.is_success() => Ok(()),
            Ok(r) => Err(Attempt::Status(r.status, r.body)),
            Err(NotifyError::Offline(m)) => Err(Attempt::Offline(m)),
            Err(e) => Err(Attempt::Transport(e.to_string())),
        }
    }

    /// Send `msg`. The error text is the reference bot's stderr line.
    pub fn send(&self, msg: &EmailMessage) -> Result<(), NotifyError> {
        self.attempt(|key| self.request(msg, key))
            .map_err(|a| match a {
                Attempt::NoKey => NotifyError::Misconfigured(format!(
                    "Email skipped: {} not set",
                    self.api_key_env
                )),
                Attempt::Offline(m) => NotifyError::Offline(m),
                Attempt::Status(code, body) => {
                    NotifyError::Failed(format!("Failed to send email (HTTP {code}): {body}"))
                }
                Attempt::Transport(e) => NotifyError::Failed(format!("Failed to send email: {e}")),
            })
    }

    /// The bot's `send_email(subject, text, html=None)`: `true` on success, otherwise
    /// the reason on stderr and `false`.
    pub fn send_email(&self, subject: &str, text: &str, html: Option<&str>) -> bool {
        let mut msg = EmailMessage::text(subject, text);
        msg.html = html.map(str::to_string);
        match self.send(&msg) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("{e}");
                false
            }
        }
    }

    /// The weekly report's sender: `sent`, `no resend key`, or
    /// `email failed <code> <first 200 chars of the body>`.
    pub fn send_report_html(&self, subject: &str, html: &str) -> String {
        match self.attempt(|key| self.report_request(subject, html, key)) {
            Ok(()) => "sent".into(),
            Err(Attempt::NoKey) => "no resend key".into(),
            Err(Attempt::Status(code, body)) => format!("email failed {code} {}", head(&body, 200)),
            Err(Attempt::Offline(m)) | Err(Attempt::Transport(m)) => format!("email failed {m}"),
        }
    }
}

fn expand_home(p: &str) -> Option<PathBuf> {
    match p.strip_prefix("~/") {
        Some(rest) => Some(PathBuf::from(std::env::var_os("HOME")?).join(rest)),
        None => Some(PathBuf::from(p)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;

    const KEY_ENV: &str = "RUNGBOT_NOTIFY_TEST_RESEND";

    fn cfg() -> EmailConfig {
        let mut c = EmailConfig::new("alerts@example.com", "me@example.com");
        c.api_key_env = KEY_ENV.into();
        c
    }

    #[test]
    fn the_request_carries_the_bearer_key_json_and_user_agent() {
        let r = cfg().request(&EmailMessage::text("S", "T"), "re_test");
        assert_eq!(r.url, RESEND_URL);
        assert_eq!(r.timeout_s, 15);
        assert_eq!(r.header_value("authorization"), Some("Bearer re_test"));
        assert_eq!(r.header_value("content-type"), Some("application/json"));
        assert_eq!(r.header_value("user-agent"), Some(crate::USER_AGENT));
        assert_eq!(
            r.body,
            r#"{"from": "alerts@example.com", "to": ["me@example.com"], "subject": "S", "text": "T"}"#
        );
        assert!(!format!("{r:?}").contains("re_test"));
    }

    #[test]
    fn a_configured_user_agent_wins() {
        let mut c = cfg();
        c.user_agent = Some("my-bot/1.0".into());
        let r = c.request(&EmailMessage::text("S", "T"), "k");
        assert_eq!(r.header_value("User-Agent"), Some("my-bot/1.0"));
    }

    #[test]
    fn the_report_request_is_html_only_with_its_own_agent_and_timeout() {
        let r = cfg().report_request("S", "<p>x</p>", "k");
        assert_eq!(r.timeout_s, 45);
        assert_eq!(r.header_value("User-Agent"), Some("Mozilla/5.0"));
        assert!(!r.body.contains("\"text\""), "{}", r.body);
        assert!(r.body.ends_with(r#""html": "<p>x</p>"}"#), "{}", r.body);
    }

    #[test]
    fn a_missing_key_is_skipped_in_the_old_words() {
        let _env = EnvGuard::set(&[(KEY_ENV, None), ("RUNGBOT_OFFLINE", Some("1"))]);
        let e = cfg().send(&EmailMessage::text("S", "T")).unwrap_err();
        assert_eq!(
            e,
            NotifyError::Misconfigured(format!("Email skipped: {KEY_ENV} not set"))
        );
        assert!(!cfg().send_email("S", "T", None));
        assert_eq!(cfg().send_report_html("S", "<p/>"), "no resend key");
    }

    #[test]
    fn offline_builds_nothing_that_leaves() {
        let _env = EnvGuard::set(&[(KEY_ENV, Some("k")), ("RUNGBOT_OFFLINE", Some("1"))]);
        assert!(matches!(
            cfg().send(&EmailMessage::text("S", "T")),
            Err(NotifyError::Offline(_))
        ));
        assert!(!cfg().send_email("S", "T", Some("<b>x</b>")));
        assert!(cfg()
            .send_report_html("S", "x")
            .starts_with("email failed "));
    }

    #[test]
    fn the_key_file_is_read_when_the_variable_is_empty() {
        let dir = std::env::temp_dir().join(format!("rungbot-notify-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("resend.env");
        std::fs::write(&file, format!("OTHER=1\n{KEY_ENV}= \"re_from_file\" \n")).unwrap();
        let _env = EnvGuard::set(&[(KEY_ENV, Some(""))]);
        let mut c = cfg();
        c.api_key_file = Some(file.to_string_lossy().into_owned());
        assert_eq!(c.resolve_key().as_deref(), Some("re_from_file"));
        std::fs::write(&file, format!("{KEY_ENV}=''\n")).unwrap();
        assert_eq!(c.resolve_key(), None, "an empty key is no key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_overrides_replace_the_addresses() {
        let _env = EnvGuard::set(&[
            (FROM_ENV, Some("a@example.org")),
            (TO_ENV, Some("b@example.org")),
        ]);
        let c = cfg().with_env_overrides();
        assert_eq!(
            (c.from.as_str(), c.to.as_str()),
            ("a@example.org", "b@example.org")
        );
    }

    #[test]
    fn debug_hides_the_addresses() {
        let d = format!("{:?}", cfg());
        assert!(!d.contains("example.com"), "{d}");
    }
}
