//! One POST, built as a value first and sent second.
//!
//! Building and sending are separate on purpose. A test builds the request and asserts
//! its URL, headers and body byte for byte with no network, and [`HttpRequest::send`]
//! is the single place that checks `RUNGBOT_OFFLINE` before anything leaves.
//!
//! A request can carry secrets (an `Authorization` header, a bot token inside the URL).
//! They are registered with the request, and its `Debug` output and every error it
//! returns have them replaced with `***`.

use std::fmt;

use crate::NotifyError;

#[derive(Clone, PartialEq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub timeout_s: u64,
    secrets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl HttpRequest {
    pub fn post(url: impl Into<String>, body: impl Into<String>, timeout_s: u64) -> Self {
        HttpRequest {
            url: url.into(),
            headers: Vec::new(),
            body: body.into(),
            timeout_s,
            secrets: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// Mark a value that must never appear in logs or errors.
    pub fn secret(mut self, value: &str) -> Self {
        if !value.is_empty() {
            self.secrets.push(value.to_string());
        }
        self
    }

    /// The value of a header, by case-insensitive name.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `s` with every registered secret replaced by `***`.
    pub fn redact(&self, s: &str) -> String {
        let mut out = s.to_string();
        for secret in &self.secrets {
            out = out.replace(secret.as_str(), "***");
        }
        out
    }

    /// Send it, unless `RUNGBOT_OFFLINE=1`. A non-2xx status is a response, not an
    /// error: each channel words its own failure.
    pub fn send(&self) -> Result<HttpResponse, NotifyError> {
        if crate::offline() {
            return Err(NotifyError::Offline(
                "RUNGBOT_OFFLINE=1 refuses to send a notification".into(),
            ));
        }
        let mut req = minreq::post(&self.url)
            .with_timeout(self.timeout_s)
            .with_body(self.body.clone());
        for (k, v) in &self.headers {
            req = req.with_header(k, v);
        }
        let resp = req
            .send()
            .map_err(|e| NotifyError::Failed(self.redact(&e.to_string())))?;
        Ok(HttpResponse {
            status: resp.status_code,
            body: self.redact(resp.as_str().unwrap_or("")),
        })
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), self.redact(v)))
            .collect();
        f.debug_struct("HttpRequest")
            .field("url", &self.redact(&self.url))
            .field("headers", &headers)
            .field("body", &self.redact(&self.body))
            .field("timeout_s", &self.timeout_s)
            .finish()
    }
}

/// The first `n` characters of `s`: the `s[:n]` slice the reference bot used.
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_shows_a_secret() {
        let r = HttpRequest::post("https://api.example/botTOKEN123/x", "{}", 5)
            .header("Authorization", "Bearer TOKEN123")
            .secret("TOKEN123");
        let d = format!("{r:?}");
        assert!(!d.contains("TOKEN123"), "{d}");
        assert!(d.contains("Bearer ***"), "{d}");
        assert!(d.contains("/bot***/x"), "{d}");
    }

    #[test]
    fn offline_refuses_before_anything_leaves() {
        let _env = crate::testenv::EnvGuard::offline();
        let r = HttpRequest::post("https://example.invalid", "{}", 1);
        assert!(matches!(r.send(), Err(NotifyError::Offline(_))));
    }

    #[test]
    fn head_counts_characters_not_bytes() {
        assert_eq!(head("caf\u{e9}\u{1F680}xyz", 5), "caf\u{e9}\u{1F680}");
        assert_eq!(head("ab", 10), "ab");
    }
}
