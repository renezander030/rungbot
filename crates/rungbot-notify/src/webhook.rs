//! A JSON POST to a URL you choose. Whatever is on the other end is your business and
//! none of rungbot's, which is the point.

use serde::Deserialize;

use crate::http::{head, HttpRequest};
use crate::NotifyError;

pub const DEFAULT_TIMEOUT_S: u64 = 20;

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_S
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// Defaults to [`crate::USER_AGENT`].
    #[serde(default)]
    pub user_agent: Option<String>,
}

impl WebhookConfig {
    pub fn new(url: impl Into<String>) -> Self {
        WebhookConfig {
            url: url.into(),
            timeout_s: DEFAULT_TIMEOUT_S,
            user_agent: None,
        }
    }

    /// Only `http://` and `https://` URLs are accepted.
    pub fn validate(&self) -> Result<(), NotifyError> {
        if self.url.starts_with("https://") || self.url.starts_with("http://") {
            Ok(())
        } else {
            Err(NotifyError::Misconfigured(
                "the webhook url must be an http(s) URL".into(),
            ))
        }
    }

    pub fn request(&self, payload: &serde_json::Value) -> HttpRequest {
        let body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
        let ua = self.user_agent.as_deref().unwrap_or(crate::USER_AGENT);
        HttpRequest::post(&self.url, body, self.timeout_s)
            .header("User-Agent", ua)
            .header("Content-Type", "application/json")
    }

    /// POST `payload`. A non-2xx answer is `Failed("<status> <first 160 chars>")`.
    pub fn send(&self, payload: &serde_json::Value) -> Result<(), NotifyError> {
        let resp = self.request(payload).send()?;
        if !resp.is_success() {
            return Err(NotifyError::Failed(format!(
                "{} {}",
                resp.status,
                head(&resp.body, 160)
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_is_the_payload_as_compact_json() {
        let c = WebhookConfig::new("https://hooks.example/x");
        let r = c.request(&serde_json::json!({"source": "rungbot", "text": "a\"b"}));
        assert_eq!(r.url, "https://hooks.example/x");
        assert_eq!(r.body, r#"{"source":"rungbot","text":"a\"b"}"#);
        assert_eq!(r.header_value("Content-Type"), Some("application/json"));
        assert_eq!(r.header_value("User-Agent"), Some(crate::USER_AGENT));
        assert_eq!(r.timeout_s, 20);
    }

    #[test]
    fn only_http_urls_validate() {
        assert!(WebhookConfig::new("https://x").validate().is_ok());
        assert!(WebhookConfig::new("ftp://x").validate().is_err());
    }

    #[test]
    fn offline_refuses() {
        let _env = crate::testenv::EnvGuard::offline();
        let r = WebhookConfig::new("https://example.invalid").send(&serde_json::json!({}));
        assert!(matches!(r, Err(NotifyError::Offline(_))));
    }
}
