//! Public JSON reads for the dashboard, with the reference's error wording.
//!
//! The dashboard files carry error text (`BTC: HTTP Error 500: Internal Server Error`),
//! so a failed read is described the way the first implementation described it: an
//! error status as `HTTP Error {code}: {reason}`, a failed connection as
//! `<urlopen error {why}>`.

use rungbot_core::watch::json::Json;

use crate::http::{Http, Request, SendError};

/// The reason phrase Python's `http.client.responses` gives a status.
pub fn reason(code: u16) -> &'static str {
    match code {
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        418 => "I'm a Teapot",
        422 => "Unprocessable Content",
        425 => "Too Early",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// Why a read failed.
#[derive(Debug, Clone, PartialEq)]
pub enum NetError {
    /// An HTTP status outside 2xx.
    Status(u16),
    /// Nothing came back, or the offline switch refused the call.
    Url(String),
    /// A body that is not JSON.
    Decode(String),
}

impl NetError {
    /// `str(e)` of the exception the reference raised.
    pub fn text(&self) -> String {
        match self {
            NetError::Status(c) => format!("HTTP Error {c}: {}", reason(*c)),
            NetError::Url(m) => format!("<urlopen error {m}>"),
            NetError::Decode(m) => m.clone(),
        }
    }

    /// `f"{type(e).__name__}: {e}"`.
    pub fn typed(&self) -> String {
        let name = match self {
            NetError::Status(_) => "HTTPError",
            NetError::Url(_) => "URLError",
            NetError::Decode(_) => "JSONDecodeError",
        };
        format!("{name}: {}", self.text())
    }
}

/// One GET (or a POST when `body` is set) whose 2xx body is JSON.
pub fn fetch(
    http: &Http,
    url: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
    timeout_s: u64,
) -> Result<Json, NetError> {
    let req = Request {
        method: if body.is_some() { "POST" } else { "GET" }.into(),
        url: url.into(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.map(str::to_string),
        timeout_s,
    };
    match http.transport.send(&req) {
        Err(SendError::Offline(m)) | Err(SendError::Network(m)) => Err(NetError::Url(m)),
        Ok(r) if (200..300).contains(&r.status) => {
            serde_json::from_str::<Json>(&r.body).map_err(|e| NetError::Decode(e.to_string()))
        }
        Ok(r) => Err(NetError::Status(r.status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_as_the_reference_printed_them() {
        assert_eq!(
            NetError::Status(500).text(),
            "HTTP Error 500: Internal Server Error"
        );
        assert_eq!(
            NetError::Status(429).typed(),
            "HTTPError: HTTP Error 429: Too Many Requests"
        );
        assert_eq!(
            NetError::Url("down".into()).typed(),
            "URLError: <urlopen error down>"
        );
    }
}
