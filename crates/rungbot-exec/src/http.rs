//! The one door every venue call goes through.
//!
//! [`Transport`] sends a request; [`Minreq`] is the real one and the only place in the
//! crate that opens a connection. It refuses every call, signed or public, while
//! `RUNGBOT_OFFLINE` is set, so a test or a dry run that forgets to fake its client
//! fails loudly instead of trading. Tests swap in a scripted transport and a fixed
//! [`Clock`], which is also how request signatures are pinned byte for byte.

use std::rc::Rc;

use crate::pyfmt;

/// Seconds a venue call may take, for every venue.
pub const TIMEOUT_S: u64 = 15;

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout_s: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SendError {
    /// The offline switch refused the call before anything was sent.
    Offline(String),
    /// Nothing usable came back: DNS, TLS, timeout, reset.
    Network(String),
}

pub trait Transport {
    fn send(&self, req: &Request) -> Result<Response, SendError>;
}

/// Is the offline switch on? Any value but empty, `0`, `no` or `false` turns it on.
pub fn offline() -> bool {
    match std::env::var("RUNGBOT_OFFLINE") {
        Ok(v) => !matches!(v.trim(), "" | "0" | "no" | "false"),
        Err(_) => false,
    }
}

/// The real transport.
#[derive(Debug, Default, Clone, Copy)]
pub struct Minreq;

impl Transport for Minreq {
    fn send(&self, req: &Request) -> Result<Response, SendError> {
        if offline() {
            return Err(SendError::Offline(format!(
                "RUNGBOT_OFFLINE is set: refusing network call {} {}",
                req.method, req.url
            )));
        }
        let mut r = match req.method.as_str() {
            "GET" => minreq::get(&req.url),
            "POST" => minreq::post(&req.url),
            "DELETE" => minreq::delete(&req.url),
            "PUT" => minreq::put(&req.url),
            m => return Err(SendError::Network(format!("unsupported method {m}"))),
        }
        .with_timeout(req.timeout_s);
        for (k, v) in &req.headers {
            r = r.with_header(k.as_str(), v.as_str());
        }
        if let Some(b) = &req.body {
            r = r.with_body(b.as_str());
        }
        let resp = r.send().map_err(|e| SendError::Network(e.to_string()))?;
        Ok(Response {
            status: resp.status_code as u16,
            body: String::from_utf8_lossy(resp.as_bytes()).into_owned(),
        })
    }
}

/// Wall-clock seconds, as a float. Injected so signatures and caches are testable.
pub type Clock = Rc<dyn Fn() -> f64>;

pub fn system_clock() -> Clock {
    Rc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    })
}

/// A transport and a clock: what every client is built on.
#[derive(Clone)]
pub struct Http {
    pub transport: Rc<dyn Transport>,
    pub clock: Clock,
}

impl Default for Http {
    fn default() -> Self {
        Http {
            transport: Rc::new(Minreq),
            clock: system_clock(),
        }
    }
}

impl Http {
    pub fn new(transport: Rc<dyn Transport>, clock: Clock) -> Http {
        Http { transport, clock }
    }

    pub fn now(&self) -> f64 {
        (self.clock)()
    }
}

/// A failed venue call. The message is the text a caller stores in the journal and
/// mails, `{METHOD} {path} -> {status} {body}` for a refused request.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueError {
    pub venue: &'static str,
    /// The HTTP status, when one came back.
    pub status: Option<u16>,
    pub message: String,
}

impl VenueError {
    pub fn new(venue: &'static str, status: Option<u16>, message: impl Into<String>) -> Self {
        VenueError {
            venue,
            status,
            message: message.into(),
        }
    }

    /// Did the offline switch refuse it?
    pub fn is_offline(&self) -> bool {
        self.message.starts_with("RUNGBOT_OFFLINE is set")
    }

    /// The venue refused the key from this address. On Gate a key without an IP
    /// allowlist is disabled after 90 days, silently, and that is what this looks like.
    pub fn is_ip_refusal(&self) -> bool {
        let m = self.message.to_lowercase();
        m.contains("ip_forbidden")
            || m.contains("not in the ip whitelist")
            || m.contains("ip not allowed")
    }

    /// A human hint for the failures that look like bugs and are not.
    pub fn hint(&self) -> Option<&'static str> {
        if self.is_ip_refusal() {
            return Some(
                "the venue refused this key from this IP. Either your address changed, or a \
                 key without an IP allowlist passed its 90-day expiry. Both are fixed in the \
                 venue's API management page, not here.",
            );
        }
        if matches!(self.status, Some(401)) {
            return Some(
                "the venue rejected the credentials: wrong key, wrong secret, or a revoked key.",
            );
        }
        None
    }
}

impl core::fmt::Display for VenueError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for VenueError {}

/// What came back from one call, before a client decides whether it is a success.
#[derive(Debug, Clone)]
pub struct Reply {
    /// `None` when nothing usable came back (network failure, or a success body that is
    /// not JSON).
    pub status: Option<u16>,
    pub json: Option<serde_json::Value>,
    /// The body text, or the failure text when `status` is `None`.
    pub text: String,
}

impl Reply {
    /// The body as `f"{body}"` prints it.
    pub fn body_str(&self) -> String {
        if self.status.is_none() || self.json.is_none() {
            return self.text.clone();
        }
        pyfmt::body_str(&self.text)
    }

    /// The status as `f"{st}"` prints it.
    pub fn status_str(&self) -> String {
        self.status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "None".into())
    }

    pub fn json_or_null(&self) -> serde_json::Value {
        self.json.clone().unwrap_or(serde_json::Value::Null)
    }
}

/// Send one request the way every public and HMAC-signed call does.
///
/// A 2xx body must be JSON; one that is not reads as no reply at all. An error status
/// keeps its body, parsed when it is JSON. A network failure gives no status. Only the
/// offline switch is an immediate error, because nothing was attempted.
pub fn request(
    http: &Http,
    venue: &'static str,
    method: &str,
    url: &str,
    headers: Vec<(String, String)>,
    body: Option<String>,
) -> Result<Reply, VenueError> {
    let req = Request {
        method: method.into(),
        url: url.into(),
        headers,
        body,
        timeout_s: TIMEOUT_S,
    };
    match http.transport.send(&req) {
        Err(SendError::Offline(m)) => Err(VenueError::new(venue, None, m)),
        // The wording journals have always stored for a network failure.
        Err(SendError::Network(m)) => Ok(Reply {
            status: None,
            json: None,
            text: format!("<urlopen error {m}>"),
        }),
        Ok(r) if (200..300).contains(&r.status) => match serde_json::from_str(&r.body) {
            Ok(v) => Ok(Reply {
                status: Some(r.status),
                json: Some(v),
                text: r.body,
            }),
            Err(e) => Ok(Reply {
                status: None,
                json: None,
                text: e.to_string(),
            }),
        },
        Ok(r) => Ok(Reply {
            status: Some(r.status),
            json: serde_json::from_str(&r.body).ok(),
            text: r.body,
        }),
    }
}

/// Header value lookup, case-insensitive.
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_transport_refuses_everything_offline() {
        let _env = crate::testenv::EnvGuard::offline();
        let e = Minreq
            .send(&Request {
                method: "POST".into(),
                url: "https://example.invalid/order".into(),
                headers: vec![],
                body: Some("{}".into()),
                timeout_s: 1,
            })
            .unwrap_err();
        assert!(matches!(e, SendError::Offline(_)), "{e:?}");
    }

    #[test]
    fn the_offline_switch_reads_like_a_flag() {
        for (v, on) in [
            ("1", true),
            ("yes", true),
            ("0", false),
            ("false", false),
            ("", false),
        ] {
            let _env = crate::testenv::EnvGuard::set(&[("RUNGBOT_OFFLINE", Some(v))]);
            assert_eq!(offline(), on, "{v:?}");
        }
    }
}
