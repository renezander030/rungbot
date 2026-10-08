//! The heartbeat: one GET to a dead-man's-switch URL after each completed run.
//!
//! A monitor such as healthchecks.io or Uptime Kuma alerts when the pings stop, which
//! covers what no error mail can: a timer that stopped firing or a host that is down.

use crate::http::{Request, SendError, Transport};

/// Seconds the ping may take; it never holds up the run for longer.
pub const TIMEOUT_S: u64 = 10;

/// `RUNGBOT_HEARTBEAT_URL` when set, else the config's `heartbeat_url`.
pub fn url(configured: &str, env: Option<String>) -> Option<String> {
    env.filter(|v| !v.trim().is_empty())
        .or_else(|| Some(configured.to_string()))
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
}

/// Send the ping. A 2xx answer is success.
pub fn ping(transport: &dyn Transport, url: &str) -> Result<(), String> {
    let req = Request {
        method: "GET".into(),
        url: url.into(),
        headers: vec![("User-Agent".into(), crate::USER_AGENT.into())],
        body: None,
        timeout_s: TIMEOUT_S,
    };
    match transport.send(&req) {
        Ok(r) if (200..300).contains(&r.status) => Ok(()),
        Ok(r) => Err(format!("heartbeat answered {}", r.status)),
        Err(SendError::Offline(m)) | Err(SendError::Network(m)) => Err(format!("heartbeat: {m}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Response;
    use std::cell::RefCell;

    struct Script {
        status: u16,
        seen: RefCell<Vec<Request>>,
    }

    impl Transport for Script {
        fn send(&self, req: &Request) -> Result<Response, SendError> {
            self.seen.borrow_mut().push(req.clone());
            Ok(Response {
                status: self.status,
                body: "OK".into(),
            })
        }
    }

    #[test]
    fn the_environment_wins_and_blank_means_off() {
        assert_eq!(url("", None), None);
        assert_eq!(url("  ", Some(" ".into())), None);
        assert_eq!(url("https://a/1", None).as_deref(), Some("https://a/1"));
        assert_eq!(
            url("https://a/1", Some("https://b/2".into())).as_deref(),
            Some("https://b/2")
        );
    }

    #[test]
    fn one_get_and_a_non_2xx_is_an_error() {
        let ok = Script {
            status: 200,
            seen: RefCell::new(Vec::new()),
        };
        assert_eq!(ping(&ok, "https://hc.example/abc"), Ok(()));
        let seen = ok.seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].url, "https://hc.example/abc");

        let down = Script {
            status: 404,
            seen: RefCell::new(Vec::new()),
        };
        assert_eq!(
            ping(&down, "https://hc.example/abc"),
            Err("heartbeat answered 404".into())
        );
    }
}
