//! The Revolut X spot client.
//!
//! Pairs are USD-quoted and named `AAA/USD`; order bodies use the dash form. Requests
//! are signed with Ed25519 over `{ts_ms}{METHOD}{path}{query}{body}`, where the body is
//! compact JSON, using the private key whose public half is registered to the API key.
//!
//! * Client ids must be UUIDs: a journal id becomes its uuid5 ([`crate::ids`]), so the
//!   same intent derives the same UUID and the venue's duplicate check holds across a
//!   crash. The venue echoes the UUID, not the journal id.
//! * `POST /orders` answers with a thin ack (id and state, no quantities); order methods
//!   follow it with a `GET` so callers get a full body, falling back to the ack.
//! * A partial fill is reported as `partially_filled` and mapped to `PARTIALLY_FILLED`,
//!   the journal's open spelling, so reconcile keeps polling it.
//! * The ticker endpoint returns the whole book at once; it is cached for 5 seconds.
//!
//! Every call, signed or public, goes through the transport and so honours the offline
//! switch.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{self, Http, Reply, VenueError};
use crate::ids;
use crate::pyfmt::{self, float_or_zero, PyVal};
use crate::venue::{shape_err, Balance, Limits, ParsedOrder, Venue};

pub const BASE: &str = "https://revx.revolut.com";
const V: &str = "revx";
const TICKER_TTL: f64 = 5.0;
const MAX_PAGES: usize = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairInfo {
    pub base_step: f64,
    pub quote_step: f64,
    /// Decimal places implied by the steps; only meaningful for steps up to 1.
    pub amount_precision: i64,
    pub price_precision: i64,
    pub min_base: f64,
    pub min_quote: f64,
    pub status: Option<String>,
}

/// The Binance-shaped view of a pair's rules.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Filters {
    pub step: f64,
    #[serde(rename = "minQty")]
    pub min_qty: f64,
    #[serde(rename = "minNotional")]
    pub min_notional: f64,
    pub tick: f64,
}

/// An Ed25519 key for request signing, read from a PKCS#8 PEM file.
pub struct RevxKey(SigningKey);

impl core::fmt::Debug for RevxKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RevxKey(<redacted>)")
    }
}

impl RevxKey {
    pub fn from_pem(pem: &str) -> Result<RevxKey, String> {
        let der = pem_body(pem, "PRIVATE KEY")?;
        let seed = pkcs8_ed25519_seed(&der)?;
        Ok(RevxKey(SigningKey::from_bytes(&seed)))
    }

    pub fn from_file(path: &Path) -> Result<RevxKey, String> {
        let pem = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the private key {}: {e}", path.display()))?;
        Self::from_pem(&pem).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Base64 of the signature over `msg`.
    pub fn sign(&self, msg: &[u8]) -> String {
        b64encode(&self.0.sign(msg).to_bytes())
    }
}

/// Where the credentials come from. The key file is read on the first signed call, so
/// a client for public data works without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevxCredentials {
    pub key: String,
    pub pem_path: PathBuf,
}

pub struct Revx {
    creds: RevxCredentials,
    http: Http,
    signer: RefCell<Option<RevxKey>>,
    pairs: RefCell<BTreeMap<String, PairInfo>>,
    ticks: RefCell<(BTreeMap<String, Value>, f64)>,
}

fn err(method: &str, path: &str, r: &Reply) -> VenueError {
    VenueError::new(
        V,
        r.status,
        format!("{method} {path} -> {} {}", r.status_str(), r.body_str()),
    )
}

impl Revx {
    pub fn new(creds: RevxCredentials) -> Self {
        Self::with_http(creds, Http::default())
    }

    pub fn with_http(creds: RevxCredentials, http: Http) -> Self {
        Revx {
            creds,
            http,
            signer: RefCell::new(None),
            pairs: RefCell::new(BTreeMap::new()),
            ticks: RefCell::new((BTreeMap::new(), 0.0)),
        }
    }

    fn sign(&self, msg: &str) -> Result<String, VenueError> {
        if self.signer.borrow().is_none() {
            if self.creds.key.is_empty() || self.creds.pem_path.as_os_str().is_empty() {
                return Err(shape_err(
                    V,
                    "Revolut X credentials missing: set RUNGBOT_REVX_KEY and \
                     RUNGBOT_REVX_PRIVATE_KEY_PEM, or create revx.env",
                ));
            }
            let k = RevxKey::from_file(&self.creds.pem_path).map_err(|e| shape_err(V, e))?;
            *self.signer.borrow_mut() = Some(k);
        }
        Ok(self
            .signer
            .borrow()
            .as_ref()
            .expect("loaded above")
            .sign(msg.as_bytes()))
    }

    /// The signed headers for one request at `ts_ms`.
    pub fn headers(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: Option<&str>,
        ts_ms: i64,
    ) -> Result<Vec<(String, String)>, VenueError> {
        let sig = self.sign(&format!(
            "{ts_ms}{method}{path}{query}{}",
            body.unwrap_or("")
        ))?;
        let mut h = vec![
            ("X-Revx-API-Key".to_string(), self.creds.key.clone()),
            ("X-Revx-Timestamp".to_string(), ts_ms.to_string()),
            ("X-Revx-Signature".to_string(), sig),
        ];
        if body.is_some() {
            h.push(("Content-Type".into(), "application/json".into()));
        }
        Ok(h)
    }

    fn signed(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: Option<&PyVal>,
    ) -> Result<Value, VenueError> {
        let body_str = body.map(|b| pyfmt::dumps_ordered(b, true));
        let ts = (self.http.now() * 1000.0) as i64;
        let headers = self.headers(method, path, query, body_str.as_deref(), ts)?;
        let url = if query.is_empty() {
            format!("{BASE}{path}")
        } else {
            format!("{BASE}{path}?{query}")
        };
        let req = http::Request {
            method: method.into(),
            url,
            headers,
            body: body_str.filter(|b| !b.is_empty()),
            timeout_s: http::TIMEOUT_S,
        };
        let r = match self.http.transport.send(&req) {
            Err(http::SendError::Offline(m)) => return Err(VenueError::new(V, None, m)),
            Err(http::SendError::Network(m)) => Reply {
                status: None,
                json: None,
                text: format!("<urlopen error {m}>"),
            },
            Ok(resp) if (200..300).contains(&resp.status) => {
                if resp.body.trim().is_empty() {
                    Reply {
                        status: Some(resp.status),
                        json: Some(Value::Object(Default::default())),
                        text: "{}".into(),
                    }
                } else {
                    match serde_json::from_str(&resp.body) {
                        Ok(v) => Reply {
                            status: Some(resp.status),
                            json: Some(v),
                            text: resp.body,
                        },
                        Err(e) => Reply {
                            status: None,
                            json: None,
                            text: e.to_string(),
                        },
                    }
                }
            }
            Ok(resp) => Reply {
                status: Some(resp.status),
                json: serde_json::from_str(&resp.body).ok(),
                text: resp.body,
            },
        };
        if !matches!(r.status, Some(200) | Some(201) | Some(204)) {
            return Err(err(method, path, &r));
        }
        Ok(r.json_or_null())
    }

    fn public(&self, path: &str) -> Result<Reply, VenueError> {
        http::request(&self.http, V, "GET", &format!("{BASE}{path}"), vec![], None)
    }

    fn rows(v: Value, what: &str) -> Result<Vec<Value>, VenueError> {
        v.as_array()
            .cloned()
            .ok_or_else(|| shape_err(V, format!("{what}: not a list")))
    }

    fn tickers(&self) -> Result<BTreeMap<String, Value>, VenueError> {
        let now = self.http.now();
        if now - self.ticks.borrow().1 > TICKER_TTL {
            let r = self.public("/api/1.0/public/tickers")?;
            let body = r.json.as_ref().filter(|v| v.is_object());
            let Some(body) = body.filter(|_| r.status == Some(200)) else {
                return Err(VenueError::new(
                    V,
                    r.status,
                    format!(
                        "tickers -> {} {}",
                        r.status_str(),
                        pyfmt::head(&r.body_str(), 200)
                    ),
                ));
            };
            let mut map = BTreeMap::new();
            for t in body
                .get("data")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default()
            {
                let sym = t
                    .get("symbol")
                    .map(pyfmt::value_str)
                    .ok_or_else(|| shape_err(V, "'symbol'"))?;
                map.insert(sym, t);
            }
            *self.ticks.borrow_mut() = (map, self.http.now());
        }
        Ok(self.ticks.borrow().0.clone())
    }

    /// Precision and minimums for a pair. One call fetches every pair; a pair not yet
    /// cached triggers a refetch.
    pub fn pair_info(&self, pair: &str) -> Result<PairInfo, VenueError> {
        if let Some(p) = self.pairs.borrow().get(pair) {
            return Ok(p.clone());
        }
        let r = self.public("/api/1.0/public/configuration/pairs")?;
        let Some(body) = r
            .json
            .as_ref()
            .filter(|v| v.is_object() && r.status == Some(200))
        else {
            return Err(VenueError::new(
                V,
                r.status,
                format!(
                    "configuration/pairs -> {} {}",
                    r.status_str(),
                    pyfmt::head(&r.body_str(), 200)
                ),
            ));
        };
        let f = |v: &Value, k: &str| float_or_zero(v.get(k)).map_err(|e| shape_err(V, e));
        let prec = |step: f64| -> i64 {
            if step > 0.0 && step <= 1.0 {
                (-step.log10()).round_ties_even().max(0.0) as i64
            } else {
                0
            }
        };
        let mut pairs = self.pairs.borrow_mut();
        for (k, v) in body.as_object().expect("checked above") {
            let (bs, qs) = (f(v, "base_step")?, f(v, "quote_step")?);
            pairs.insert(
                k.clone(),
                PairInfo {
                    base_step: bs,
                    quote_step: qs,
                    amount_precision: prec(bs),
                    price_precision: prec(qs),
                    min_base: f(v, "min_order_size")?,
                    min_quote: f(v, "min_order_size_quote")?,
                    status: v.get("status").and_then(|s| s.as_str()).map(String::from),
                },
            );
        }
        pairs
            .get(pair)
            .cloned()
            .ok_or_else(|| shape_err(V, format!("pair {pair} not in venue configuration")))
    }

    /// The Binance-shaped view of [`Revx::pair_info`].
    pub fn filters(&self, pair: &str) -> Result<Filters, VenueError> {
        let p = self.pair_info(pair)?;
        Ok(Filters {
            step: p.base_step,
            min_qty: p.min_base,
            min_notional: p.min_quote,
            tick: p.quote_step,
        })
    }

    fn order(
        &self,
        pair: &str,
        side: &str,
        config: PyVal,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        let cid = match client_id.filter(|c| !c.is_empty()) {
            Some(c) => ids::safe_revx_cid(c).map_err(|e| shape_err(V, e))?,
            None => random_uuid4(self.http.now()),
        };
        let body = PyVal::dict(vec![
            ("client_order_id", PyVal::str(cid)),
            ("symbol", PyVal::str(pair.replace('/', "-"))),
            ("side", PyVal::str(side)),
            ("order_configuration", config),
        ]);
        let ack = self.signed("POST", "/api/1.0/orders", "", Some(&body))?;
        let data = ack.get("data").filter(|d| pyfmt::truthy(Some(d)));
        let oid = data
            .and_then(|d| d.get("venue_order_id").filter(|v| pyfmt::truthy(Some(v))))
            .or_else(|| data.and_then(|d| d.get("id").filter(|v| pyfmt::truthy(Some(v)))));
        let Some(oid) = oid.map(pyfmt::value_str) else {
            return Ok(ack);
        };
        // The ack has no quantities: return the full order, like the other venues.
        match self.signed("GET", &format!("/api/1.0/orders/{oid}"), "", None) {
            Ok(full) => Ok(full),
            Err(e) if e.is_offline() => Err(e),
            Err(_) => Ok(ack),
        }
    }

    fn limit(
        &self,
        side: &str,
        pair: &str,
        base: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        let (base, price) = (
            self.round_amount(pair, base)?,
            self.round_price(pair, price)?,
        );
        self.order(
            pair,
            side,
            PyVal::dict(vec![(
                "limit",
                PyVal::dict(vec![
                    ("base_size", PyVal::str(pyfmt::fixed_stripped(base, 8))),
                    ("price", PyVal::str(pyfmt::fixed_stripped(price, 8))),
                ]),
            )]),
            client_id,
        )
    }

    /// Public 24h ticker: `(last, change_pct)`. The venue reports the change as an
    /// absolute amount, converted here to a percentage of the price a day ago.
    pub fn ticker(&self, pair: &str) -> Result<(f64, f64), VenueError> {
        let r = self.public("/api/1.0/public/tickers")?;
        let Some(body) = r
            .json
            .as_ref()
            .filter(|v| v.is_object() && r.status == Some(200))
        else {
            return Err(VenueError::new(
                V,
                r.status,
                format!(
                    "ticker {pair} -> {} {}",
                    r.status_str(),
                    pyfmt::head(&r.body_str(), 200)
                ),
            ));
        };
        let t = body
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| {
                a.iter()
                    .find(|x| x.get("symbol").and_then(|s| s.as_str()) == Some(pair))
            })
            .ok_or_else(|| shape_err(V, format!("ticker {pair}: not in feed")))?;
        let f = |k: &str| float_or_zero(t.get(k)).map_err(|e| shape_err(V, e));
        let need = |k: &str| {
            t.get(k)
                .ok_or_else(|| shape_err(V, format!("'{k}'")))
                .and_then(|v| pyfmt::to_float(v).map_err(|e| shape_err(V, e)))
        };
        let mut last = f("last_price")?;
        if last == 0.0 {
            last = (need("bid")? + need("ask")?) / 2.0;
        }
        let chg = f("price_change_24h")?;
        let prev = last - chg;
        Ok((last, if prev != 0.0 { chg / prev * 100.0 } else { 0.0 }))
    }
}

/// Normalise a Revolut X order: the full body, or the thin POST ack.
pub fn parse_order(raw: &Value) -> Result<ParsedOrder, VenueError> {
    let empty = Value::Object(Default::default());
    let r = match raw {
        Value::Object(o) => match o.get("data") {
            None => raw,
            Some(d) if !pyfmt::truthy(Some(d)) => &empty,
            Some(d @ Value::Object(_)) => d,
            Some(_) => return Err(shape_err(V, "order data is not an object")),
        },
        _ => &empty,
    };
    let pick = |a: &str, b: &str| -> String {
        let v = r.get(a).filter(|v| pyfmt::truthy(Some(v)));
        let v = v.or_else(|| r.get(b).filter(|v| pyfmt::truthy(Some(v))));
        v.map(pyfmt::value_str).unwrap_or_default()
    };
    let mut status = pick("status", "state").to_lowercase();
    if status == "partially_filled" {
        status = "PARTIALLY_FILLED".into();
    }
    let f = |k: &str| float_or_zero(r.get(k)).map_err(|e| shape_err(V, e));
    let fq = f("filled_quantity")?;
    let fa = f("filled_amount")?;
    let avg0 = f("average_fill_price")?;
    let avg = if avg0 != 0.0 {
        Some(avg0)
    } else if fq != 0.0 {
        Some(fa / fq)
    } else {
        None
    };
    let price = f("price")?;
    Ok(ParsedOrder {
        order_id: pick("id", "venue_order_id"),
        filled: status == "filled",
        status,
        base_qty: Some(fq),
        quote: fa,
        avg_price: avg,
        price: (price != 0.0).then_some(price),
        fee: f("total_fee")?,
        fee_asset: r
            .get("fee_currency")
            .and_then(|v| v.as_str())
            .map(String::from),
        side: r
            .get("side")
            .map(pyfmt::value_str)
            .unwrap_or_default()
            .to_lowercase(),
        qty: f("quantity")?,
        client_id: match r.get("client_order_id") {
            None | Some(Value::Null) => String::new(),
            Some(v) => pyfmt::value_str(v),
        },
    })
}

impl Venue for Revx {
    fn name(&self) -> &'static str {
        V
    }

    fn parse_order(&self, raw: &Value) -> Result<ParsedOrder, VenueError> {
        parse_order(raw)
    }

    fn order_status(&self, _pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        parse_order(&self.signed("GET", &format!("/api/1.0/orders/{order_id}"), "", None)?)
    }

    /// Open orders on `pair`, following the cursor for at most 10 pages.
    fn open_orders(&self, pair: &str) -> Result<Vec<ParsedOrder>, VenueError> {
        let mut rows = Vec::new();
        let mut cursor = String::new();
        for _ in 0..MAX_PAGES {
            let q = if cursor.is_empty() {
                String::new()
            } else {
                pyfmt::urlencode(&[("cursor", cursor.clone())])
            };
            let resp = self.signed("GET", "/api/1.0/orders/active", &q, None)?;
            if !resp.is_object() {
                return Err(shape_err(V, "active orders: not an object"));
            }
            for o in resp
                .get("data")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if o.get("symbol").and_then(|s| s.as_str()) == Some(pair) {
                    rows.push(o);
                }
            }
            cursor = resp
                .get("metadata")
                .filter(|m| pyfmt::truthy(Some(m)))
                .and_then(|m| m.get("next_cursor"))
                .filter(|c| pyfmt::truthy(Some(c)))
                .map(pyfmt::value_str)
                .unwrap_or_default();
            if cursor.is_empty() {
                break;
            }
        }
        rows.iter().map(parse_order).collect()
    }

    /// `DELETE /orders/{id}` answers 204 with no body; the result is a synthetic
    /// cancelled order carrying no fill.
    fn cancel(&self, _pair: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        self.signed("DELETE", &format!("/api/1.0/orders/{order_id}"), "", None)?;
        Ok(ParsedOrder {
            order_id: order_id.to_string(),
            status: "cancelled".into(),
            base_qty: Some(0.0),
            ..Default::default()
        })
    }

    fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError> {
        Self::rows(
            self.signed("GET", "/api/1.0/balances", "", None)?,
            "balances",
        )?
        .iter()
        .map(|r| {
            Ok((
                currency(r)?,
                float_or_zero(r.get("available")).map_err(|e| shape_err(V, e))?,
            ))
        })
        .collect()
    }

    /// `{asset: {free, locked}}`; the venue calls locked `reserved`.
    fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError> {
        Self::rows(
            self.signed("GET", "/api/1.0/balances", "", None)?,
            "balances",
        )?
        .iter()
        .map(|r| {
            Ok((
                currency(r)?,
                Balance {
                    free: float_or_zero(r.get("available")).map_err(|e| shape_err(V, e))?,
                    locked: float_or_zero(r.get("reserved")).map_err(|e| shape_err(V, e))?,
                },
            ))
        })
        .collect()
    }

    /// Last price, else mid, else the bid/ask midpoint.
    fn price(&self, pair: &str) -> Result<f64, VenueError> {
        let ticks = self.tickers()?;
        let t = ticks
            .get(pair)
            .filter(|t| pyfmt::truthy(Some(t)))
            .ok_or_else(|| shape_err(V, format!("price {pair}: not in ticker feed")))?;
        let truthy = |k: &str| t.get(k).filter(|v| pyfmt::truthy(Some(v)));
        // A quoted "0" is a present value: it is returned as 0.0, as the venue sent it.
        if let Some(v) = truthy("last_price").or_else(|| truthy("mid")) {
            return pyfmt::to_float(v).map_err(|e| shape_err(V, e));
        }
        if let (Some(b), Some(a)) = (truthy("bid"), truthy("ask")) {
            let b = pyfmt::to_float(b).map_err(|e| shape_err(V, e))?;
            let a = pyfmt::to_float(a).map_err(|e| shape_err(V, e))?;
            let mid = (b + a) / 2.0;
            if mid != 0.0 {
                return Ok(mid);
            }
        }
        Err(shape_err(V, format!("price {pair}: empty quotes")))
    }

    fn limits(&self, pair: &str) -> Result<Limits, VenueError> {
        let p = self.pair_info(pair)?;
        Ok(Limits {
            min_base: p.min_base,
            min_quote: p.min_quote,
        })
    }

    /// `floor(x / step) * step`, or `x` unchanged for a zero step.
    fn round_amount(&self, pair: &str, amount: f64) -> Result<f64, VenueError> {
        let step = self.pair_info(pair)?.base_step;
        Ok(if step > 0.0 {
            (amount / step).floor() * step
        } else {
            amount
        })
    }

    fn round_price(&self, pair: &str, price: f64) -> Result<f64, VenueError> {
        let step = self.pair_info(pair)?.quote_step;
        Ok(if step > 0.0 {
            (price / step).floor() * step
        } else {
            price
        })
    }

    fn market_buy(
        &self,
        pair: &str,
        quote: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        self.order(
            pair,
            "buy",
            PyVal::dict(vec![(
                "market",
                PyVal::dict(vec![("quote_size", PyVal::str(pyfmt::fixed(quote, 2)))]),
            )]),
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
            pair,
            "sell",
            PyVal::dict(vec![(
                "market",
                PyVal::dict(vec![(
                    "base_size",
                    PyVal::str(pyfmt::fixed_stripped(base, 8)),
                )]),
            )]),
            client_id,
        )
    }

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

fn currency(r: &Value) -> Result<String, VenueError> {
    r.get("currency")
        .map(pyfmt::value_str)
        .ok_or_else(|| shape_err(V, "'currency'"))
}

/// A version-4-shaped UUID for an order placed without a journal id. Not
/// cryptographically random, and it need not be: nothing matches on it.
fn random_uuid4(now: f64) -> String {
    use sha1::{Digest, Sha1};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = Sha1::new();
    h.update(now.to_bits().to_be_bytes());
    h.update(std::process::id().to_be_bytes());
    h.update(N.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    let d = h.finalize();
    let mut u = [0u8; 16];
    u.copy_from_slice(&d[..16]);
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    ids::fmt_uuid(u)
}

// ------------------------------------------------------------------ PEM / PKCS#8

fn pem_body(pem: &str, label: &str) -> Result<Vec<u8>, String> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem
        .find(&begin)
        .ok_or_else(|| format!("not a PEM {label} (no {begin} line)"))?
        + begin.len();
    let stop = pem[start..]
        .find(&end)
        .ok_or_else(|| format!("no {end} line"))?
        + start;
    let b64: String = pem[start..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    b64decode(&b64).ok_or_else(|| "the PEM body is not base64".into())
}

/// The 32-byte seed inside a PKCS#8 Ed25519 private key (RFC 8410), version 1 or 2.
fn pkcs8_ed25519_seed(der: &[u8]) -> Result<[u8; 32], String> {
    fn tlv(d: &[u8], i: usize) -> Option<(u8, usize, usize)> {
        let tag = *d.get(i)?;
        let l0 = *d.get(i + 1)? as usize;
        if l0 < 0x80 {
            return Some((tag, i + 2, l0));
        }
        let n = l0 & 0x7f;
        if n == 0 || n > 2 {
            return None;
        }
        let mut len = 0usize;
        for k in 0..n {
            len = (len << 8) | *d.get(i + 2 + k)? as usize;
        }
        Some((tag, i + 2 + n, len))
    }
    let bad = || "not an unencrypted PKCS#8 Ed25519 private key".to_string();
    let (t, body, _) = tlv(der, 0).ok_or_else(bad)?;
    if t != 0x30 {
        return Err(bad());
    }
    let (t, vstart, vlen) = tlv(der, body).ok_or_else(bad)?;
    if t != 0x02 || vlen != 1 || der.get(vstart).is_none_or(|v| *v > 1) {
        return Err(bad());
    }
    let (t, astart, alen) = tlv(der, vstart + vlen).ok_or_else(bad)?;
    const ED25519_OID: [u8; 5] = [0x06, 0x03, 0x2b, 0x65, 0x70];
    if t != 0x30 || der.get(astart..astart + 5) != Some(&ED25519_OID[..]) {
        return Err(bad());
    }
    let (t, ostart, olen) = tlv(der, astart + alen).ok_or_else(bad)?;
    if t != 0x04 {
        return Err(bad());
    }
    let (t, sstart, slen) = tlv(der, ostart).ok_or_else(bad)?;
    if t != 0x04 || slen != 32 || sstart + 32 > ostart + olen {
        return Err(bad());
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&der[sstart..sstart + 32]);
    Ok(seed)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * k)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = B64.iter().position(|x| *x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for n in 0..10 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64decode(&b64encode(&data)).unwrap(), data);
        }
        assert_eq!(b64encode(b"hi"), "aGk=");
    }

    #[test]
    fn a_non_ed25519_key_is_refused() {
        let e = RevxKey::from_pem("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----")
            .unwrap_err();
        assert!(e.contains("PKCS#8 Ed25519"), "{e}");
        assert!(RevxKey::from_pem("nothing here").is_err());
    }

    #[test]
    fn a_version_two_key_with_its_public_half_loads_too() {
        // RFC 8410 section 10.3's example: OneAsymmetricKey v2 with the public key.
        let pem = "-----BEGIN PRIVATE KEY-----\n\
                   MHICAQEwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n\
                   oB8wHQYKKoZIhvcNAQkJFDEPDA1DdXJkbGUgQ2hhaXJzgSEAGb9ECWmEzf6FQbrB\n\
                   Z9w7lshQhqowtrbLDFw4rXAxZuE=\n\
                   -----END PRIVATE KEY-----";
        RevxKey::from_pem(pem).expect("v2 loads");
    }

    #[test]
    fn debug_never_prints_the_key() {
        let pem = "-----BEGIN PRIVATE KEY-----\n\
                   MC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n\
                   -----END PRIVATE KEY-----";
        let k = RevxKey::from_pem(pem).unwrap();
        assert_eq!(format!("{k:?}"), "RevxKey(<redacted>)");
    }

    #[test]
    fn signed_calls_honour_the_offline_switch() {
        let _env = crate::testenv::EnvGuard::offline();
        let pem = std::env::temp_dir().join(format!("rungbot-revx-{}.pem", std::process::id()));
        std::fs::write(
            &pem,
            "-----BEGIN PRIVATE KEY-----\n\
             MC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n\
             -----END PRIVATE KEY-----\n",
        )
        .unwrap();
        let r = Revx::new(RevxCredentials {
            key: "k".into(),
            pem_path: pem.clone(),
        });
        // Signed: the request is fully built and signed, then refused at the door.
        assert!(r.balances_full().unwrap_err().is_offline());
        assert!(r
            .limit_buy("AAA/USD", 1.0, 1.0, Some("csAAAb1r1"))
            .unwrap_err()
            .is_offline());
        assert!(r.order_status("AAA/USD", "x").unwrap_err().is_offline());
        // Public.
        assert!(r.price("AAA/USD").unwrap_err().is_offline());
        let _ = std::fs::remove_file(pem);
    }
}
