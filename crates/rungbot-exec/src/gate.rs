//! The Gate.io spot client.
//!
//! Signing: HMAC-SHA512 over `METHOD\npath\nquery\nsha512hex(body)\nts`, seconds.
//! Market data and pair precision are public calls; everything account-bound is signed.
//!
//! Two response quirks are handled in [`parse_order`], and both have cost money when
//! read naively:
//!
//! * `fill_price` is a deprecated alias for `filled_total`, the TOTAL quote filled, not a
//!   per-unit price. The per-unit price is `avg_deal_price`. Reading `fill_price` as the
//!   price inflates the average by roughly the base amount and pushes a paired limit
//!   sell off the book.
//! * On a MARKET BUY, `amount` and `left` are in the QUOTE asset, so `amount - left` is
//!   quote, not base. The base is derived from `filled_total / avg_deal_price`.
//!
//! A key with no IP allowlist is disabled after 90 days, silently. See
//! [`crate::http::VenueError::hint`].

use std::cell::RefCell;
use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha512};

use crate::http::{self, hex, Http, Reply, VenueError};
use crate::ids;
use crate::keys::Credentials;
use crate::pyfmt::{self, float_or_zero, PyVal};
use crate::venue::{shape_err, Balance, Limits, ParsedOrder, Venue};

pub const BASE: &str = "https://api.gateio.ws";
const V: &str = "gate";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PairInfo {
    pub amount_precision: i64,
    pub price_precision: i64,
    pub min_base: f64,
    pub min_quote: f64,
}

impl PairInfo {
    /// Round down, never up: rounding an amount up can exceed the balance you have.
    pub fn round_amount(&self, amount: f64) -> f64 {
        floor_to(amount, self.amount_precision)
    }

    pub fn round_price(&self, price: f64) -> f64 {
        floor_to(price, self.price_precision)
    }
}

/// `floor(x * 10**p) / 10**p`.
fn floor_to(x: f64, p: i64) -> f64 {
    let f = 10f64.powi(p as i32);
    (x * f).floor() / f
}

/// An on-chain deposit as Gate credited it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deposit {
    pub currency: Option<String>,
    pub amount: f64,
    /// `DONE` means credited.
    pub status: Option<String>,
    pub ts: f64,
}

pub struct Gate {
    creds: Credentials,
    http: Http,
    pairs: RefCell<BTreeMap<String, PairInfo>>,
}

type HmacSha512 = Hmac<Sha512>;

fn err(method: &str, path: &str, r: &Reply) -> VenueError {
    VenueError::new(
        V,
        r.status,
        format!("{method} {path} -> {} {}", r.status_str(), r.body_str()),
    )
}

impl Gate {
    pub fn new(creds: Credentials) -> Self {
        Self::with_http(creds, Http::default())
    }

    pub fn with_http(creds: Credentials, http: Http) -> Self {
        Gate {
            creds,
            http,
            pairs: RefCell::new(BTreeMap::new()),
        }
    }

    /// The signed headers for one request at `now` (whole seconds).
    pub fn headers(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: &str,
        now: i64,
    ) -> Vec<(String, String)> {
        let payload_hash = hex(&Sha512::digest(body.as_bytes()));
        let to_sign = format!("{method}\n{path}\n{query}\n{payload_hash}\n{now}");
        let mut mac = HmacSha512::new_from_slice(self.creds.secret.as_bytes())
            .expect("hmac takes any key length");
        mac.update(to_sign.as_bytes());
        vec![
            ("KEY".into(), self.creds.key.clone()),
            ("Timestamp".into(), now.to_string()),
            ("SIGN".into(), hex(&mac.finalize().into_bytes())),
            ("Accept".into(), "application/json".into()),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn signed(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: Option<&PyVal>,
    ) -> Result<Value, VenueError> {
        let body_str = body
            .map(|b| pyfmt::dumps_ordered(b, false))
            .unwrap_or_default();
        let url = if query.is_empty() {
            format!("{BASE}{path}")
        } else {
            format!("{BASE}{path}?{query}")
        };
        let now = self.http.now() as i64;
        let headers = self.headers(method, path, query, &body_str, now);
        let r = http::request(
            &self.http,
            V,
            method,
            &url,
            headers,
            (!body_str.is_empty()).then_some(body_str),
        )?;
        if !matches!(r.status, Some(200) | Some(201)) {
            return Err(err(method, path, &r));
        }
        Ok(r.json_or_null())
    }

    fn public(&self, url: &str) -> Result<Reply, VenueError> {
        http::request(&self.http, V, "GET", url, vec![], None)
    }

    fn accounts(&self) -> Result<Vec<Value>, VenueError> {
        let rows = self.signed("GET", "/api/v4/spot/accounts", "", None)?;
        rows.as_array()
            .cloned()
            .ok_or_else(|| shape_err(V, "accounts: not a list"))
    }

    fn field(r: &Value, k: &str) -> Result<f64, VenueError> {
        let v = r.get(k).ok_or_else(|| shape_err(V, format!("'{k}'")))?;
        pyfmt::to_float(v).map_err(|e| shape_err(V, e))
    }

    fn currency(r: &Value) -> Result<String, VenueError> {
        r.get("currency")
            .map(pyfmt::value_str)
            .ok_or_else(|| shape_err(V, "'currency'"))
    }

    /// On-chain deposits (`GET /wallet/deposits`, needs the key's wallet-read scope).
    /// `since` is Gate's `from`, an epoch floor; Gate defaults to 7 days.
    pub fn deposits(
        &self,
        currency: Option<&str>,
        since: Option<f64>,
    ) -> Result<Vec<Deposit>, VenueError> {
        let mut q = Vec::new();
        if let Some(c) = currency.filter(|c| !c.is_empty()) {
            q.push(format!("currency={c}"));
        }
        if let Some(s) = since.filter(|s| *s != 0.0) {
            q.push(format!("from={}", s as i64));
        }
        let rows = self.signed("GET", "/api/v4/wallet/deposits", &q.join("&"), None)?;
        let rows = match rows {
            Value::Null => vec![],
            Value::Array(a) => a,
            _ => return Err(shape_err(V, "deposits: not a list")),
        };
        rows.iter()
            .map(|r| {
                Ok(Deposit {
                    currency: r.get("currency").and_then(|v| v.as_str()).map(String::from),
                    amount: float_or_zero(r.get("amount")).map_err(|e| shape_err(V, e))?,
                    status: r.get("status").and_then(|v| v.as_str()).map(String::from),
                    ts: float_or_zero(r.get("timestamp")).map_err(|e| shape_err(V, e))?,
                })
            })
            .collect()
    }

    /// Precision and minimums for a pair, cached for the life of the client.
    pub fn pair_info(&self, pair: &str) -> Result<PairInfo, VenueError> {
        if let Some(p) = self.pairs.borrow().get(pair) {
            return Ok(*p);
        }
        let r = self.public(&format!("{BASE}/api/v4/spot/currency_pairs/{pair}"))?;
        if r.status != Some(200) {
            return Err(VenueError::new(
                V,
                r.status,
                format!(
                    "currency_pairs {pair} -> {} {}",
                    r.status_str(),
                    r.body_str()
                ),
            ));
        }
        let body = r.json_or_null();
        let int = |k: &str, d: i64| -> Result<i64, VenueError> {
            match body.get(k) {
                None => Ok(d),
                Some(v) => py_int(v).ok_or_else(|| shape_err(V, format!("int({k})"))),
            }
        };
        let info = PairInfo {
            amount_precision: int("amount_precision", 0)?,
            price_precision: int("precision", 8)?,
            min_base: float_or_zero(body.get("min_base_amount")).map_err(|e| shape_err(V, e))?,
            min_quote: float_or_zero(body.get("min_quote_amount")).map_err(|e| shape_err(V, e))?,
        };
        self.pairs.borrow_mut().insert(pair.to_string(), info);
        Ok(info)
    }

    fn order(
        &self,
        mut body: Vec<(&str, PyVal)>,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        if let Some(cid) = client_id.filter(|c| !c.is_empty()) {
            let text = ids::safe_gate_text(cid).map_err(|e| shape_err(V, e))?;
            body.push(("text", PyVal::str(text)));
        }
        self.signed("POST", "/api/v4/spot/orders", "", Some(&PyVal::dict(body)))
    }

    /// Public 24h ticker: `(last, change_percentage)`.
    pub fn ticker(&self, pair: &str) -> Result<(f64, f64), VenueError> {
        let r = self.public(&format!("{BASE}/api/v4/spot/tickers?currency_pair={pair}"))?;
        let first = r
            .json
            .as_ref()
            .and_then(|v| v.as_array())
            .and_then(|a| a.first());
        match (r.status, first) {
            (Some(200), Some(t)) => Ok((
                Self::field(t, "last")?,
                Self::field(t, "change_percentage")?,
            )),
            _ => Err(VenueError::new(
                V,
                r.status,
                format!("ticker {pair} -> {} {}", r.status_str(), r.body_str()),
            )),
        }
    }
}

/// Python's `int(v)` for a JSON number or numeric string.
fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

/// Normalise a Gate order or order response. See the module docs for the two quirks.
pub fn parse_order(r: &Value) -> Result<ParsedOrder, VenueError> {
    let f = |k: &str| float_or_zero(r.get(k)).map_err(|e| shape_err(V, e));
    let amount = f("amount")?;
    let left = f("left")?;
    let ft = f("filled_total")?;
    let avg0 = f("avg_deal_price")?;
    let mut avg = (avg0 != 0.0).then_some(avg0);
    let side = r.get("side").map(pyfmt::value_str).unwrap_or_default();
    let otype = r.get("type").map(pyfmt::value_str).unwrap_or_default();
    let market_buy = otype == "market" && side == "buy";
    let base = if market_buy {
        avg.map(|a| ft / a)
    } else {
        let b = amount - left;
        if avg.is_none() && b != 0.0 {
            avg = Some(ft / b);
        }
        Some(b)
    };
    let status = match r.get("status") {
        None | Some(Value::Null) => String::new(),
        Some(v) => pyfmt::value_str(v),
    };
    let text = match r.get("text") {
        Some(v) if pyfmt::truthy(Some(v)) => pyfmt::value_str(v),
        _ => String::new(),
    };
    let price = f("price")?;
    Ok(ParsedOrder {
        order_id: r.get("id").map(pyfmt::value_str).unwrap_or_default(),
        filled: status == "closed" || status == "filled",
        status,
        base_qty: base,
        quote: ft,
        avg_price: avg,
        price: (price != 0.0).then_some(price),
        fee: f("fee")?,
        fee_asset: r
            .get("fee_currency")
            .and_then(|v| v.as_str())
            .map(String::from),
        side: side.to_lowercase(),
        qty: if market_buy { 0.0 } else { amount },
        client_id: text.strip_prefix("t-").unwrap_or(&text).to_string(),
    })
}

impl Venue for Gate {
    fn name(&self) -> &'static str {
        V
    }

    fn parse_order(&self, raw: &Value) -> Result<ParsedOrder, VenueError> {
        parse_order(raw)
    }

    fn order_status(&self, pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        parse_order(&self.signed(
            "GET",
            &format!("/api/v4/spot/orders/{order_id}"),
            &format!("currency_pair={pair}"),
            None,
        )?)
    }

    fn open_orders(&self, pair: &str) -> Result<Vec<ParsedOrder>, VenueError> {
        let rows = self.signed(
            "GET",
            "/api/v4/spot/orders",
            &format!("currency_pair={pair}&status=open"),
            None,
        )?;
        rows.as_array()
            .ok_or_else(|| shape_err(V, "open orders: not a list"))?
            .iter()
            .map(parse_order)
            .collect()
    }

    fn cancel(&self, pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        parse_order(&self.signed(
            "DELETE",
            &format!("/api/v4/spot/orders/{order_id}"),
            &format!("currency_pair={pair}"),
            None,
        )?)
    }

    fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError> {
        self.accounts()?
            .iter()
            .map(|r| Ok((Self::currency(r)?, Self::field(r, "available")?)))
            .collect()
    }

    /// `{asset: {free, locked}}`; locked is what resting orders hold.
    fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError> {
        self.accounts()?
            .iter()
            .map(|r| {
                Ok((
                    Self::currency(r)?,
                    Balance {
                        free: Self::field(r, "available")?,
                        locked: Self::field(r, "locked")?,
                    },
                ))
            })
            .collect()
    }

    fn price(&self, pair: &str) -> Result<f64, VenueError> {
        let r = self.public(&format!("{BASE}/api/v4/spot/tickers?currency_pair={pair}"))?;
        let first = r
            .json
            .as_ref()
            .and_then(|v| v.as_array())
            .and_then(|a| a.first());
        match (r.status, first, pyfmt::truthy(r.json.as_ref())) {
            (Some(200), Some(t), true) => Self::field(t, "last"),
            _ => Err(VenueError::new(
                V,
                r.status,
                format!("price {pair} -> {} {}", r.status_str(), r.body_str()),
            )),
        }
    }

    fn limits(&self, pair: &str) -> Result<Limits, VenueError> {
        let p = self.pair_info(pair)?;
        Ok(Limits {
            min_base: p.min_base,
            min_quote: p.min_quote,
        })
    }

    fn round_amount(&self, pair: &str, amount: f64) -> Result<f64, VenueError> {
        Ok(self.pair_info(pair)?.round_amount(amount))
    }

    fn round_price(&self, pair: &str, price: f64) -> Result<f64, VenueError> {
        Ok(self.pair_info(pair)?.round_price(price))
    }

    /// Market buy spending `quote` of the quote asset, IOC.
    fn market_buy(
        &self,
        pair: &str,
        quote: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        self.order(
            vec![
                ("currency_pair", PyVal::str(pair)),
                ("type", PyVal::str("market")),
                ("side", PyVal::str("buy")),
                ("amount", PyVal::str(pyfmt::fixed(quote, 4))),
                ("time_in_force", PyVal::str("ioc")),
                ("account", PyVal::str("spot")),
            ],
            client_id,
        )
    }

    fn market_sell(
        &self,
        pair: &str,
        base: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        let base = self.round_amount(pair, base)?;
        self.order(
            vec![
                ("currency_pair", PyVal::str(pair)),
                ("type", PyVal::str("market")),
                ("side", PyVal::str("sell")),
                ("amount", PyVal::str(pyfmt::float_repr(base))),
                ("time_in_force", PyVal::str("ioc")),
                ("account", PyVal::str("spot")),
            ],
            client_id,
        )
    }

    /// GTC limit buy; amount and price rounded down to the pair's precision.
    fn limit_buy(
        &self,
        pair: &str,
        base: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        self.limit("buy", pair, base, price, client_id)
    }

    fn limit_sell(
        &self,
        pair: &str,
        base: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        self.limit("sell", pair, base, price, client_id)
    }
}

impl Gate {
    fn limit(
        &self,
        side: &str,
        pair: &str,
        base: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        let info = self.pair_info(pair)?;
        let (base, price) = (info.round_amount(base), info.round_price(price));
        self.order(
            vec![
                ("currency_pair", PyVal::str(pair)),
                ("type", PyVal::str("limit")),
                ("side", PyVal::str(side)),
                ("amount", PyVal::str(pyfmt::float_repr(base))),
                ("price", PyVal::str(pyfmt::float_repr(price))),
                ("time_in_force", PyVal::str("gtc")),
                ("account", PyVal::str("spot")),
            ],
            client_id,
        )
    }
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
    fn the_signature_covers_method_path_query_body_and_time() {
        let g = gate();
        let sign = |h: Vec<(String, String)>| h.into_iter().find(|(n, _)| n == "SIGN").unwrap().1;
        let base = sign(g.headers("GET", "/p", "", "", 1));
        assert_eq!(base.len(), 128);
        for other in [
            g.headers("POST", "/p", "", "", 1),
            g.headers("GET", "/q", "", "", 1),
            g.headers("GET", "/p", "a=1", "", 1),
            g.headers("GET", "/p", "", "{}", 1),
            g.headers("GET", "/p", "", "", 2),
        ] {
            assert_ne!(base, sign(other));
        }
        assert_eq!(
            base,
            sign(g.headers("GET", "/p", "", "", 1)),
            "deterministic"
        );
    }

    #[test]
    fn the_offline_guard_covers_every_gate_call() {
        let _env = crate::testenv::EnvGuard::offline();
        let g = gate();
        assert!(g.balances_full().unwrap_err().is_offline());
        assert!(g.pair_info("AAA_USDT").unwrap_err().is_offline());
        assert!(g
            .limit_buy("AAA_USDT", 1.0, 1.0, Some("x"))
            .unwrap_err()
            .is_offline());
    }

    #[test]
    fn amounts_round_down_so_an_order_never_exceeds_the_balance() {
        let info = PairInfo {
            amount_precision: 2,
            price_precision: 4,
            min_base: 0.01,
            min_quote: 1.0,
        };
        assert_eq!(info.round_amount(1.239), 1.23);
        assert_eq!(info.round_price(10.99999), 10.9999);
        assert_eq!(info.round_amount(0.001), 0.0);
    }
}
