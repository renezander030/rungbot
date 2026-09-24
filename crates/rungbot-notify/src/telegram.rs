//! Telegram through the Bot API `sendMessage`.
//!
//! Plain text on purpose: with a parse mode set, a price like `1.0*2` or a name with an
//! underscore is read as formatting and the whole message is rejected. The body is
//! `{"chat_id": ..., "text": ..., "disable_web_page_preview": true}`, plus
//! `"disable_notification": true` when the config asks for a silent push.
//!
//! Length: every caller of the reference bot cut its text to the first 3,900 characters
//! before handing it over (`text[:3900]`), leaving room for a `[host] ` prefix under
//! Telegram's 4,096 limit. [`TelegramConfig::max_chars`] is that cut. After the prefix
//! a hard ceiling of 4,096 UTF-16 units (what Telegram counts) still applies, so a text
//! of wide characters is shortened instead of rejected.
//!
//! The bot token comes from the environment variable the config names, never from the
//! config itself, and is redacted from every `Debug` output and error.

use serde::{Deserialize, Deserializer};

use crate::http::{head, HttpRequest};
use crate::{json_escape, NotifyError};

pub const DEFAULT_TOKEN_ENV: &str = "RUNGBOT_TELEGRAM_TOKEN";
/// The reference bot's `text[:3900]`.
pub const DEFAULT_MAX_CHARS: usize = 3900;
/// Telegram's own limit on a message, in UTF-16 code units.
pub const TELEGRAM_LIMIT_UTF16: usize = 4096;
pub const DEFAULT_TIMEOUT_S: u64 = 20;
pub const API_BASE: &str = "https://api.telegram.org";

fn default_token_env() -> String {
    DEFAULT_TOKEN_ENV.into()
}
fn default_max_chars() -> Option<usize> {
    Some(DEFAULT_MAX_CHARS)
}
fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_S
}
fn yes() -> bool {
    true
}

/// A chat id written in YAML is often a bare number; accept either.
fn chat_id<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        S(String),
        I(i64),
    }
    Ok(match Id::deserialize(d)? {
        Id::S(s) => s,
        Id::I(i) => i.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    #[serde(deserialize_with = "chat_id")]
    pub chat_id: String,
    /// Name of the environment variable holding the bot token.
    #[serde(default = "default_token_env")]
    pub token_env: String,
    /// Cut the text to this many characters first; `null` for no cut.
    #[serde(default = "default_max_chars")]
    pub max_chars: Option<usize>,
    /// Prefix `[<hostname>] `, as the reference notify script did.
    #[serde(default)]
    pub host_prefix: bool,
    #[serde(default)]
    pub disable_notification: bool,
    #[serde(default = "yes")]
    pub disable_web_page_preview: bool,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// Defaults to [`crate::USER_AGENT`].
    #[serde(default)]
    pub user_agent: Option<String>,
}

impl TelegramConfig {
    pub fn new(chat_id: impl Into<String>) -> Self {
        TelegramConfig {
            chat_id: chat_id.into(),
            token_env: default_token_env(),
            max_chars: default_max_chars(),
            host_prefix: false,
            disable_notification: false,
            disable_web_page_preview: true,
            timeout_s: DEFAULT_TIMEOUT_S,
            user_agent: None,
        }
    }

    /// The token, or the misconfiguration that names the variable to set.
    pub fn token(&self) -> Result<String, NotifyError> {
        match std::env::var(&self.token_env) {
            Ok(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            _ => Err(NotifyError::Misconfigured(format!(
                "a chat id is set but {} is empty or unset",
                self.token_env
            ))),
        }
    }

    /// The text as it will be sent: cut, prefixed, then held under Telegram's limit.
    pub fn prepare(&self, text: &str, host: Option<&str>) -> String {
        let cut = match self.max_chars {
            Some(n) => head(text, n),
            None => text.to_string(),
        };
        let full = match host {
            Some(h) if self.host_prefix => format!("[{h}] {cut}"),
            _ => cut,
        };
        clamp_utf16(&full, TELEGRAM_LIMIT_UTF16)
    }

    /// The request for an already prepared text.
    pub fn request(&self, text: &str, token: &str) -> HttpRequest {
        let mut body = format!(
            "{{\"chat_id\":{},\"text\":{}",
            json_escape(&self.chat_id),
            json_escape(text)
        );
        if self.disable_web_page_preview {
            body.push_str(",\"disable_web_page_preview\":true");
        }
        if self.disable_notification {
            body.push_str(",\"disable_notification\":true");
        }
        body.push('}');
        let ua = self.user_agent.as_deref().unwrap_or(crate::USER_AGENT);
        HttpRequest::post(
            format!("{API_BASE}/bot{token}/sendMessage"),
            body,
            self.timeout_s,
        )
        .header("User-Agent", ua)
        .header("Content-Type", "application/json")
        .secret(token)
    }

    /// Send `text`. An empty text is refused, as the notify script refused it.
    pub fn send(&self, text: &str) -> Result<(), NotifyError> {
        if text.is_empty() {
            return Err(NotifyError::Misconfigured("no message provided".into()));
        }
        let token = self.token()?;
        let host = if self.host_prefix { hostname() } else { None };
        let req = self.request(&self.prepare(text, host.as_deref()), &token);
        let resp = req.send()?;
        if !resp.is_success() {
            return Err(NotifyError::Failed(format!(
                "{} {}",
                resp.status,
                head(&resp.body, 160)
            )));
        }
        Ok(())
    }

    /// `true` when sent. Failures are swallowed, as the reference callers swallowed them.
    pub fn send_ok(&self, text: &str) -> bool {
        self.send(text).is_ok()
    }
}

/// At most `limit` UTF-16 units, never splitting a character.
pub fn clamp_utf16(s: &str, limit: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for c in s.chars() {
        used += c.len_utf16();
        if used > limit {
            break;
        }
        out.push(c);
    }
    out
}

/// The machine's name, for the optional `[host]` prefix.
pub fn hostname() -> Option<String> {
    let from_cmd = std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok());
    let name = from_cmd.or_else(|| std::fs::read_to_string("/etc/hostname").ok())?;
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;

    const TOKEN_ENV: &str = "RUNGBOT_NOTIFY_TEST_TG";

    #[test]
    fn the_request_is_plain_text_json_with_the_token_in_the_path_only() {
        let c = TelegramConfig::new("-100200");
        let r = c.request("BUY \"AAA\" 1.0*2_x", "123:abc");
        assert_eq!(r.url, "https://api.telegram.org/bot123:abc/sendMessage");
        assert_eq!(
            r.body,
            r#"{"chat_id":"-100200","text":"BUY \"AAA\" 1.0*2_x","disable_web_page_preview":true}"#
        );
        assert!(!r.body.contains("parse_mode"));
        assert_eq!(r.timeout_s, 20);
        assert!(!format!("{r:?}").contains("123:abc"));
    }

    #[test]
    fn silent_adds_disable_notification() {
        let mut c = TelegramConfig::new("1");
        c.disable_notification = true;
        assert!(c
            .request("x", "t")
            .body
            .ends_with(",\"disable_notification\":true}"));
    }

    #[test]
    fn the_cut_matches_the_reference_slice() {
        let text = "\u{1F680}ab".repeat(1400);
        let c = TelegramConfig::new("1");
        assert_eq!(head(&text, 3900).chars().count(), 3900);
        // 1300 rockets (2 UTF-16 units each) + 2600 letters = 5200 units, over Telegram's
        // 4096: the old bot sent that and was refused; the ceiling shortens it instead.
        let p = c.prepare(&text, None);
        assert_eq!(p.encode_utf16().count(), TELEGRAM_LIMIT_UTF16);
        assert!(text.starts_with(&p));
        let ascii = "x".repeat(5000);
        assert_eq!(c.prepare(&ascii, None).len(), 3900);
    }

    #[test]
    fn the_host_prefix_goes_on_after_the_cut() {
        let mut c = TelegramConfig::new("1");
        c.max_chars = Some(3);
        c.host_prefix = true;
        assert_eq!(c.prepare("abcdef", Some("box")), "[box] abc");
        c.host_prefix = false;
        assert_eq!(c.prepare("abcdef", Some("box")), "abc");
    }

    #[test]
    fn a_missing_token_names_the_variable() {
        let _env = EnvGuard::set(&[(TOKEN_ENV, None)]);
        let mut c = TelegramConfig::new("1");
        c.token_env = TOKEN_ENV.into();
        let e = c.send("hi").unwrap_err().to_string();
        assert_eq!(
            e,
            format!("a chat id is set but {TOKEN_ENV} is empty or unset")
        );
        assert!(!c.send_ok("hi"));
    }

    #[test]
    fn empty_text_is_refused_and_offline_refuses_the_rest() {
        let _env = EnvGuard::set(&[(TOKEN_ENV, Some("t")), ("RUNGBOT_OFFLINE", Some("1"))]);
        let mut c = TelegramConfig::new("1");
        c.token_env = TOKEN_ENV.into();
        assert!(matches!(c.send(""), Err(NotifyError::Misconfigured(_))));
        assert!(matches!(c.send("hi"), Err(NotifyError::Offline(_))));
    }

    #[test]
    fn a_numeric_chat_id_deserializes() {
        let c: TelegramConfig = serde_json::from_str(r#"{"chat_id": -100123}"#).unwrap();
        assert_eq!(c.chat_id, "-100123");
        assert_eq!(c.max_chars, Some(3900));
        assert!(c.disable_web_page_preview);
    }
}
