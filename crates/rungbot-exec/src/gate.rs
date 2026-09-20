//! The Gate.io spot client. One venue, deliberately.
//!
//! Gate is first because its authentication is the simplest of the three and its
//! minimums are the friendliest to a small first order. A second venue is a second set
//! of rounding rules, minimums and error shapes — worth doing once this one is proven,
//! not before.
//!
//! **Only limit orders.** A GTC limit order rests at the venue and fills while your
//! machine is asleep, which is what makes a ladder work on a laptop. Market orders need
//! you present and are not implemented here at all.
//!
//! The 90-day rule is the thing to know: a Gate key with **no IP allowlist is disabled
//! after 90 days**, silently. [`GateError::IpNotAllowed`] exists because the failure
//! that follows looks like a bug and is not.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

use crate::journal::Side;
use crate::keys::Credentials;

const BASE: &str = "https://api.gateio.ws";
const TIMEOUT_S: u64 = 20;

#[derive(Debug, Clone, PartialEq)]
pub enum GateError {
    /// The key is not allowed from this IP, or has lapsed past the 90-day rule.
    IpNotAllowed(String),
    /// Bad key, bad secret, or a permission the key does not have.
    Unauthorized(String),
    /// The venue refused the order itself: too small, bad precision, no balance.
    Rejected(String),
    Network(String),
    Unexpected(String),
}

impl core::fmt::Display for GateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GateError::IpNotAllowed(m) => write!(
                f,
                "Gate refused this key from this IP ({m}). Either your address changed, \
                 or an un-allowlisted key passed its 90-day expiry. Both are fixed in \
                 Gate's API management page, not here."
            ),
            GateError::Unauthorized(m) => write!(f, "Gate rejected the credentials: {m}"),
            GateError::Rejected(m) => write!(f, "Gate rejected the order: {m}"),
            GateError::Network(m) => write!(f, "network: {m}"),
            GateError::Unexpected(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for GateError {}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PairInfo {
    pub amount_precision: u32,
    pub price_precision: u32,
    pub min_base: f64,
    pub min_quote: f64,
}

impl PairInfo {
    /// Round down, never up: rounding an amount up can exceed the balance you have.
    pub fn round_amount(&self, amount: f64) -> f64 {
        let f = 10f64.powi(self.amount_precision as i32);
        (amount * f).floor() / f
    }

    pub fn round_price(&self, price: f64) -> f64 {
        let f = 10f64.powi(self.price_precision as i32);
        (price * f).floor() / f
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VenueOrder {
    pub id: String,
    pub text: Option<String>,
    pub status: String,
    pub price: f64,
    pub amount: f64,
    pub filled_amount: f64,
    pub filled_quote: f64,
}

/// Classify a venue response so the caller can tell "fix your setup" from "try later".
fn classify(status: u16, body: &str) -> GateError {
    let lower = body.to_lowercase();
    if lower.contains("ip_forbidden")
        || lower.contains("not in the ip whitelist")
        || lower.contains("ip not allowed")
    {
        return GateError::IpNotAllowed(body.chars().take(160).collect());
    }
    if status == 401 || lower.contains("invalid_key") || lower.contains("invalid_signature") {
        return GateError::Unauthorized(body.chars().take(160).collect());
    }
    if status == 403 {
        // Gate uses 403 both for allowlist refusals and for missing scopes.
        return GateError::Unauthorized(body.chars().take(160).collect());
    }
    if (400..500).contains(&status) {
        return GateError::Rejected(body.chars().take(200).collect());
    }
    GateError::Unexpected(format!(
        "{status}: {}",
        body.chars().take(200).collect::<String>()
    ))
}

pub struct Gate {
    creds: Credentials,
    pairs: std::cell::RefCell<std::collections::BTreeMap<String, PairInfo>>,
}

type HmacSha512 = Hmac<Sha512>;

impl Gate {
    pub fn new(creds: Credentials) -> Self {
        Gate {
            creds,
            pairs: std::cell::RefCell::new(Default::default()),
        }
    }

    /// Gate signs `METHOD\npath\nquery\nSHA512(body)\ntimestamp` with HMAC-SHA512.
    fn headers(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: &str,
        now: u64,
    ) -> Vec<(String, String)> {
        let payload_hash = {
            let mut h = Sha512::new();
            h.update(body.as_bytes());
            hex(&h.finalize())
        };
        let to_sign = format!("{method}\n{path}\n{query}\n{payload_hash}\n{now}");
        let mut mac = HmacSha512::new_from_slice(self.creds.secret.as_bytes())
            .expect("hmac takes any key length");
        mac.update(to_sign.as_bytes());
        let sign = hex(&mac.finalize().into_bytes());
        vec![
            ("KEY".into(), self.creds.key.clone()),
            ("Timestamp".into(), now.to_string()),
            ("SIGN".into(), sign),
            ("Accept".into(), "application/json".into()),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn signed(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, GateError> {
        if std::env::var("RUNGBOT_OFFLINE").as_deref() == Ok("1") {
            return Err(GateError::Network(
                "RUNGBOT_OFFLINE=1 refuses every venue call".into(),
            ));
        }
        let body_str = body.map(|b| b.to_string()).unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let url = if query.is_empty() {
            format!("{BASE}{path}")
        } else {
            format!("{BASE}{path}?{query}")
        };

        let mut req = match method {
            "GET" => minreq::get(&url),
            "POST" => minreq::post(&url),
            "DELETE" => minreq::delete(&url),
            m => return Err(GateError::Unexpected(format!("unsupported method {m}"))),
        }
        .with_timeout(TIMEOUT_S);
        for (k, v) in self.headers(method, path, query, &body_str, now) {
            req = req.with_header(k, v);
        }
        if !body_str.is_empty() {
            req = req.with_body(body_str);
        }

        let resp = req.send().map_err(|e| GateError::Network(e.to_string()))?;
        let text = resp.as_str().unwrap_or("").to_string();
        if !(200..300).contains(&resp.status_code) {
            return Err(classify(resp.status_code, &text));
        }
        serde_json::from_str(&text)
            .map_err(|e| GateError::Unexpected(format!("unreadable response: {e}")))
    }

    /// `{asset: (free, locked)}`. Locked is what resting orders are holding.
    pub fn balances(&self) -> Result<std::collections::BTreeMap<String, (f64, f64)>, GateError> {
        let rows = self.signed("GET", "/api/v4/spot/accounts", "", None)?;
        let arr = rows
            .as_array()
            .ok_or_else(|| GateError::Unexpected("accounts is not an array".into()))?;
        Ok(arr
            .iter()
            .filter_map(|r| {
                let c = r.get("currency")?.as_str()?.to_string();
                let free = r.get("available")?.as_str()?.parse().ok()?;
                let locked = r
                    .get("locked")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok());
                Some((c, (free, locked.unwrap_or(0.0))))
            })
            .collect())
    }

    pub fn pair_info(&self, pair: &str) -> Result<PairInfo, GateError> {
        if let Some(p) = self.pairs.borrow().get(pair) {
            return Ok(*p);
        }
        let body = self.signed(
            "GET",
            &format!("/api/v4/spot/currency_pairs/{pair}"),
            "",
            None,
        )?;
        let num = |k: &str, d: f64| body.get(k).and_then(as_f64).unwrap_or(d);
        let info = PairInfo {
            amount_precision: num("amount_precision", 0.0) as u32,
            price_precision: num("precision", 8.0) as u32,
            min_base: num("min_base_amount", 0.0),
            min_quote: num("min_quote_amount", 0.0),
        };
        self.pairs.borrow_mut().insert(pair.to_string(), info);
        Ok(info)
    }

    /// Place a **GTC limit** order. The only order this crate can place.
    pub fn limit_order(
        &self,
        pair: &str,
        side: Side,
        base_amount: f64,
        price: f64,
        client_id: &str,
    ) -> Result<VenueOrder, GateError> {
        let info = self.pair_info(pair)?;
        let amount = info.round_amount(base_amount);
        let price = info.round_price(price);
        if amount <= 0.0 || price <= 0.0 {
            return Err(GateError::Rejected(format!(
                "rounded to {amount} @ {price}, which is not an order"
            )));
        }
        if amount < info.min_base {
            return Err(GateError::Rejected(format!(
                "{amount} is below the venue minimum of {} {pair}",
                info.min_base
            )));
        }
        let body = serde_json::json!({
            "currency_pair": pair,
            "type": "limit",
            "account": "spot",
            "side": match side { Side::Buy => "buy", Side::Sell => "sell" },
            "amount": format!("{amount}"),
            "price": format!("{price}"),
            "time_in_force": "gtc",
            // Gate requires a user-supplied id to start with `t-`.
            "text": format!("t-{client_id}"),
        });
        parse_order(&self.signed("POST", "/api/v4/spot/orders", "", Some(&body))?)
    }

    pub fn open_orders(&self, pair: &str) -> Result<Vec<VenueOrder>, GateError> {
        let body = self.signed(
            "GET",
            "/api/v4/spot/orders",
            &format!("currency_pair={pair}&status=open"),
            None,
        )?;
        let arr = body.as_array().cloned().unwrap_or_default();
        Ok(arr.iter().filter_map(|o| parse_order(o).ok()).collect())
    }

    pub fn order_status(&self, pair: &str, order_id: &str) -> Result<VenueOrder, GateError> {
        parse_order(&self.signed(
            "GET",
            &format!("/api/v4/spot/orders/{order_id}"),
            &format!("currency_pair={pair}"),
            None,
        )?)
    }

    pub fn cancel(&self, pair: &str, order_id: &str) -> Result<VenueOrder, GateError> {
        parse_order(&self.signed(
            "DELETE",
            &format!("/api/v4/spot/orders/{order_id}"),
            &format!("currency_pair={pair}"),
            None,
        )?)
    }
}

fn as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn parse_order(v: &serde_json::Value) -> Result<VenueOrder, GateError> {
    let id = v
        .get("id")
        .and_then(|x| {
            x.as_str()
                .map(String::from)
                .or_else(|| x.as_i64().map(|n| n.to_string()))
        })
        .ok_or_else(|| GateError::Unexpected("order has no id".into()))?;
    let amount = v.get("amount").and_then(as_f64).unwrap_or(0.0);
    let left = v.get("left").and_then(as_f64).unwrap_or(0.0);
    Ok(VenueOrder {
        id,
        text: v.get("text").and_then(|x| x.as_str()).map(String::from),
        status: v
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string(),
        price: v.get("price").and_then(as_f64).unwrap_or(0.0),
        amount,
        filled_amount: (amount - left).max(0.0),
        filled_quote: v.get("filled_total").and_then(as_f64).unwrap_or(0.0),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> Gate {
        Gate::new(Credentials {
            key: "testkey".into(),
            secret: "testsecret".into(),
        })
    }

    #[test]
    fn the_headers_match_gates_documented_scheme() {
        let h = gate().headers("GET", "/api/v4/spot/accounts", "", "", 1_700_000_000);
        let get = |k: &str| {
            h.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("KEY"), "testkey");
        assert_eq!(get("Timestamp"), "1700000000");
        let sign = get("SIGN");
        assert_eq!(sign.len(), 128, "HMAC-SHA512 is 64 bytes of hex");
        assert!(sign.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_signature_covers_method_path_query_body_and_time() {
        let g = gate();
        let base = g.headers("GET", "/p", "", "", 1);
        let sign = |h: &[(String, String)]| h.iter().find(|(n, _)| n == "SIGN").unwrap().1.clone();
        assert_ne!(
            sign(&base),
            sign(&g.headers("POST", "/p", "", "", 1)),
            "method"
        );
        assert_ne!(
            sign(&base),
            sign(&g.headers("GET", "/q", "", "", 1)),
            "path"
        );
        assert_ne!(
            sign(&base),
            sign(&g.headers("GET", "/p", "a=1", "", 1)),
            "query"
        );
        assert_ne!(
            sign(&base),
            sign(&g.headers("GET", "/p", "", "{}", 1)),
            "body"
        );
        assert_ne!(
            sign(&base),
            sign(&g.headers("GET", "/p", "", "", 2)),
            "timestamp"
        );
    }

    #[test]
    fn the_same_request_signs_identically() {
        let g = gate();
        let a = g.headers("GET", "/p", "x=1", "{}", 42);
        let b = g.headers("GET", "/p", "x=1", "{}", 42);
        assert_eq!(a, b, "signing must be deterministic or retries break");
    }

    #[test]
    fn an_ip_refusal_is_named_and_explained() {
        let e = classify(403, r#"{"label":"IP_FORBIDDEN","message":"not allowed"}"#);
        assert!(matches!(e, GateError::IpNotAllowed(_)), "{e:?}");
        let msg = e.to_string();
        assert!(
            msg.contains("90-day"),
            "the 90-day rule is the likely cause: {msg}"
        );
        assert!(msg.contains("API management"), "{msg}");
    }

    #[test]
    fn auth_order_and_network_failures_are_told_apart() {
        assert!(matches!(
            classify(401, "bad key"),
            GateError::Unauthorized(_)
        ));
        assert!(matches!(
            classify(400, r#"{"label":"TOO_SMALL"}"#),
            GateError::Rejected(_)
        ));
        assert!(matches!(classify(500, "boom"), GateError::Unexpected(_)));
    }

    #[test]
    fn amounts_round_down_so_an_order_never_exceeds_the_balance() {
        let info = PairInfo {
            amount_precision: 2,
            price_precision: 4,
            min_base: 0.01,
            min_quote: 1.0,
        };
        assert_eq!(info.round_amount(1.239), 1.23, "down, never up");
        assert_eq!(info.round_price(10.99999), 10.9999);
        assert_eq!(info.round_amount(0.001), 0.0, "dust rounds to nothing");
    }

    #[test]
    fn a_venue_order_parses_and_derives_the_filled_amount() {
        let v = serde_json::json!({
            "id": "123", "text": "t-csAAAb944444r1", "status": "open",
            "price": "10.5", "amount": "2.0", "left": "0.5", "filled_total": "15.75"
        });
        let o = parse_order(&v).unwrap();
        assert_eq!(o.id, "123");
        assert_eq!(o.filled_amount, 1.5, "amount minus what is left");
        assert_eq!(o.filled_quote, 15.75);
    }

    #[test]
    fn a_numeric_id_is_accepted_as_well_as_a_string() {
        let v = serde_json::json!({ "id": 987, "amount": "1", "left": "1" });
        assert_eq!(parse_order(&v).unwrap().id, "987");
    }

    #[test]
    fn the_offline_guard_covers_the_venue_too() {
        std::env::set_var("RUNGBOT_OFFLINE", "1");
        let e = gate().balances().unwrap_err();
        assert!(matches!(e, GateError::Network(_)), "{e:?}");
        assert!(e.to_string().contains("RUNGBOT_OFFLINE"));
        std::env::remove_var("RUNGBOT_OFFLINE");
    }
}
