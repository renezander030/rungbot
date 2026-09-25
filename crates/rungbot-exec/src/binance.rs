//! The Binance spot client.
//!
//! Signing: the query string, with `timestamp` (ms) and `recvWindow=5000` appended, is
//! signed with HMAC-SHA256 and sent as `&signature=`; the key rides in `X-MBX-APIKEY`.
//! Order parameters are sent in the query string, never a body.
//!
//! Fees only appear on the immediate order response (`fills[]`), never on a later
//! status poll.

use std::cell::RefCell;
use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

use crate::http::{self, hex, Http, Reply, VenueError};
use crate::ids;
use crate::keys::Credentials;
use crate::pyfmt::{self, float_or_zero};
use crate::venue::{shape_err, Balance, Limits, ParsedOrder, Venue};

pub const BASE: &str = "https://api.binance.com";
const V: &str = "binance";

/// A symbol's trading rules: lot step and minimum, minimum notional, price tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Filters {
    pub step: f64,
    #[serde(rename = "minQty")]
    pub min_qty: f64,
    #[serde(rename = "minNotional")]
    pub min_notional: f64,
    pub tick: f64,
}

pub struct Binance {
    creds: Credentials,
    http: Http,
    filters: RefCell<BTreeMap<String, Filters>>,
}

type HmacSha256 = Hmac<Sha256>;

fn fail(prefix: String, r: &Reply) -> VenueError {
    VenueError::new(
        V,
        r.status,
        format!("{prefix} -> {} {}", r.status_str(), r.body_str()),
    )
}

impl Binance {
    pub fn new(creds: Credentials) -> Self {
        Self::with_http(creds, Http::default())
    }

    pub fn with_http(creds: Credentials, http: Http) -> Self {
        Binance {
            creds,
            http,
            filters: RefCell::new(BTreeMap::new()),
        }
    }

    /// The signed query string for `params` at `ts_ms`.
    pub fn signed_query(&self, params: &[(&str, String)], ts_ms: i64) -> String {
        let mut p: Vec<(&str, String)> = params.to_vec();
        p.push(("timestamp", ts_ms.to_string()));
        p.push(("recvWindow", "5000".into()));
        let qs = pyfmt::urlencode(&p);
        let mut mac = HmacSha256::new_from_slice(self.creds.secret.as_bytes())
            .expect("hmac takes any key length");
        mac.update(qs.as_bytes());
        format!("{qs}&signature={}", hex(&mac.finalize().into_bytes()))
    }

    fn signed(
        &self,
        method: &str,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Value, VenueError> {
        let ts = (self.http.now() * 1000.0) as i64;
        let url = format!("{BASE}{path}?{}", self.signed_query(params, ts));
        let r = http::request(
            &self.http,
            V,
            method,
            &url,
            vec![("X-MBX-APIKEY".into(), self.creds.key.clone())],
            None,
        )?;
        if r.status != Some(200) {
            return Err(fail(format!("{method} {path}"), &r));
        }
        Ok(r.json_or_null())
    }

    fn public(&self, path_query: &str) -> Result<Reply, VenueError> {
        http::request(
            &self.http,
            V,
            "GET",
            &format!("{BASE}{path_query}"),
            vec![],
            None,
        )
    }

    fn account_rows(&self) -> Result<Vec<Value>, VenueError> {
        let acct = self.signed("GET", "/api/v3/account", &[])?;
        Ok(acct
            .get("balances")
            .and_then(|b| b.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// LOT_SIZE, MIN_NOTIONAL / NOTIONAL and PRICE_FILTER for a symbol, cached.
    pub fn filters(&self, symbol: &str) -> Result<Filters, VenueError> {
        if let Some(f) = self.filters.borrow().get(symbol) {
            return Ok(*f);
        }
        let r = self.public(&format!("/api/v3/exchangeInfo?symbol={symbol}"))?;
        if r.status != Some(200) {
            return Err(fail(format!("exchangeInfo {symbol}"), &r));
        }
        let body = r.json_or_null();
        let list = body
            .get("symbols")
            .and_then(|s| s.get(0))
            .and_then(|s| s.get("filters"))
            .and_then(|f| f.as_array())
            .ok_or_else(|| shape_err(V, format!("exchangeInfo {symbol}: no filters")))?;
        let num = |v: Option<&Value>| -> Result<f64, VenueError> {
            pyfmt::to_float(v.unwrap_or(&Value::from(0))).map_err(|e| shape_err(V, e))
        };
        let mut f = Filters::default();
        for filt in list {
            match filt.get("filterType").and_then(|t| t.as_str()) {
                Some("LOT_SIZE") => {
                    f.step = num(filt.get("stepSize"))?;
                    f.min_qty = num(filt.get("minQty"))?;
                }
                Some("MIN_NOTIONAL") | Some("NOTIONAL") => {
                    f.min_notional = num(filt.get("minNotional").or_else(|| filt.get("notional")))?;
                }
                Some("PRICE_FILTER") => f.tick = num(filt.get("tickSize"))?,
                _ => {}
            }
        }
        self.filters.borrow_mut().insert(symbol.to_string(), f);
        Ok(f)
    }

    /// `floor(qty / step) * step`, or `qty` for a zero step.
    pub fn round_qty(&self, symbol: &str, qty: f64) -> Result<f64, VenueError> {
        let step = self.filters(symbol)?.step;
        Ok(if step > 0.0 {
            (qty / step).floor() * step
        } else {
            qty
        })
    }

    fn order(
        &self,
        mut p: Vec<(&str, String)>,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        if let Some(c) = client_id.filter(|c| !c.is_empty()) {
            p.push((
                "newClientOrderId",
                ids::safe_cid(c).map_err(|e| shape_err(V, e))?,
            ));
        }
        self.signed("POST", "/api/v3/order", &p)
    }

    fn limit(
        &self,
        side: &str,
        symbol: &str,
        qty: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        let (qty, price) = (
            self.round_qty(symbol, qty)?,
            self.round_price(symbol, price)?,
        );
        self.order(
            vec![
                ("symbol", symbol.to_string()),
                ("side", side.to_string()),
                ("type", "LIMIT".into()),
                ("timeInForce", "GTC".into()),
                ("quantity", pyfmt::fixed_stripped(qty, 8)),
                ("price", pyfmt::fixed_stripped(price, 8)),
            ],
            client_id,
        )
    }

    /// Public 24h ticker: `(lastPrice, priceChangePercent)`.
    pub fn ticker(&self, symbol: &str) -> Result<(f64, f64), VenueError> {
        let r = self.public(&format!("/api/v3/ticker/24hr?symbol={symbol}"))?;
        let b = r
            .json
            .as_ref()
            .filter(|v| v.is_object() && r.status == Some(200));
        let Some(b) = b else {
            return Err(fail(format!("ticker {symbol}"), &r));
        };
        let need = |k: &str| {
            b.get(k)
                .ok_or_else(|| shape_err(V, format!("'{k}'")))
                .and_then(|v| pyfmt::to_float(v).map_err(|e| shape_err(V, e)))
        };
        Ok((need("lastPrice")?, need("priceChangePercent")?))
    }
}

/// Normalise a Binance order or order response.
pub fn parse_order(r: &Value) -> Result<ParsedOrder, VenueError> {
    let f = |k: &str| float_or_zero(r.get(k)).map_err(|e| shape_err(V, e));
    let ex = f("executedQty")?;
    let cq = f("cummulativeQuoteQty")?;
    let status = match r.get("status") {
        None | Some(Value::Null) => String::new(),
        Some(v) => pyfmt::value_str(v),
    };
    let (mut fee, mut fee_asset) = (0.0, None::<String>);
    if let Some(fills) = r.get("fills").and_then(|f| f.as_array()) {
        for fl in fills {
            fee += float_or_zero(fl.get("commission")).map_err(|e| shape_err(V, e))?;
            if fee_asset.is_none() {
                fee_asset = fl
                    .get("commissionAsset")
                    .filter(|v| pyfmt::truthy(Some(v)))
                    .map(pyfmt::value_str);
            }
        }
    }
    let price = f("price")?;
    Ok(ParsedOrder {
        order_id: r.get("orderId").map(pyfmt::value_str).unwrap_or_default(),
        filled: status == "FILLED",
        status,
        base_qty: Some(ex),
        quote: cq,
        avg_price: (ex != 0.0).then(|| cq / ex),
        price: (price != 0.0).then_some(price),
        fee,
        fee_asset,
        side: r
            .get("side")
            .map(pyfmt::value_str)
            .unwrap_or_default()
            .to_lowercase(),
        qty: f("origQty")?,
        client_id: match r.get("clientOrderId") {
            None | Some(Value::Null) => String::new(),
            Some(v) => pyfmt::value_str(v),
        },
    })
}

impl Venue for Binance {
    fn name(&self) -> &'static str {
        V
    }

    fn parse_order(&self, raw: &Value) -> Result<ParsedOrder, VenueError> {
        parse_order(raw)
    }

    fn order_status(&self, symbol: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        parse_order(&self.signed(
            "GET",
            "/api/v3/order",
            &[("symbol", symbol.into()), ("orderId", order_id.into())],
        )?)
    }

    fn open_orders(&self, symbol: &str) -> Result<Vec<ParsedOrder>, VenueError> {
        let rows = self.signed("GET", "/api/v3/openOrders", &[("symbol", symbol.into())])?;
        rows.as_array()
            .ok_or_else(|| shape_err(V, "open orders: not a list"))?
            .iter()
            .map(parse_order)
            .collect()
    }

    fn cancel(&self, symbol: &str, order_id: &str) -> Result<ParsedOrder, VenueError> {
        parse_order(&self.signed(
            "DELETE",
            "/api/v3/order",
            &[("symbol", symbol.into()), ("orderId", order_id.into())],
        )?)
    }

    fn balances(&self) -> Result<BTreeMap<String, f64>, VenueError> {
        self.account_rows()?
            .iter()
            .map(|b| {
                let asset = b
                    .get("asset")
                    .map(pyfmt::value_str)
                    .ok_or_else(|| shape_err(V, "'asset'"))?;
                let free = b.get("free").ok_or_else(|| shape_err(V, "'free'"))?;
                Ok((asset, pyfmt::to_float(free).map_err(|e| shape_err(V, e))?))
            })
            .collect()
    }

    /// `{asset: {free, locked}}`; locked is what resting orders hold.
    fn balances_full(&self) -> Result<BTreeMap<String, Balance>, VenueError> {
        self.account_rows()?
            .iter()
            .map(|b| {
                let asset = b
                    .get("asset")
                    .map(pyfmt::value_str)
                    .ok_or_else(|| shape_err(V, "'asset'"))?;
                let get = |k: &str| {
                    b.get(k)
                        .ok_or_else(|| shape_err(V, format!("'{k}'")))
                        .and_then(|v| pyfmt::to_float(v).map_err(|e| shape_err(V, e)))
                };
                Ok((
                    asset,
                    Balance {
                        free: get("free")?,
                        locked: get("locked")?,
                    },
                ))
            })
            .collect()
    }

    fn price(&self, symbol: &str) -> Result<f64, VenueError> {
        let r = self.public(&format!("/api/v3/ticker/price?symbol={symbol}"))?;
        if r.status != Some(200) {
            return Err(fail(format!("price {symbol}"), &r));
        }
        let v = r
            .json
            .as_ref()
            .and_then(|b| b.get("price"))
            .ok_or_else(|| shape_err(V, "'price'"))?;
        pyfmt::to_float(v).map_err(|e| shape_err(V, e))
    }

    fn limits(&self, symbol: &str) -> Result<Limits, VenueError> {
        let f = self.filters(symbol)?;
        Ok(Limits {
            min_base: f.min_qty,
            min_quote: f.min_notional,
        })
    }

    fn round_amount(&self, symbol: &str, amount: f64) -> Result<f64, VenueError> {
        self.round_qty(symbol, amount)
    }

    fn qty_step(&self, symbol: &str) -> Result<f64, VenueError> {
        Ok(self.filters(symbol)?.step)
    }

    /// `floor(price / tick) * tick`, or `price` for a zero tick.
    fn round_price(&self, symbol: &str, price: f64) -> Result<f64, VenueError> {
        let tick = self.filters(symbol)?.tick;
        Ok(if tick > 0.0 {
            (price / tick).floor() * tick
        } else {
            price
        })
    }

    /// Spend `quote` of the quote asset (`quoteOrderQty`, two decimals).
    fn market_buy(
        &self,
        symbol: &str,
        quote: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        self.order(
            vec![
                ("symbol", symbol.to_string()),
                ("side", "BUY".into()),
                ("type", "MARKET".into()),
                ("quoteOrderQty", pyfmt::fixed(quote, 2)),
            ],
            client_id,
        )
    }

    fn market_sell(
        &self,
        symbol: &str,
        qty: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        let qty = self.round_qty(symbol, qty)?;
        self.order(
            vec![
                ("symbol", symbol.to_string()),
                ("side", "SELL".into()),
                ("type", "MARKET".into()),
                ("quantity", pyfmt::fixed_stripped(qty, 8)),
            ],
            client_id,
        )
    }

    fn limit_buy(
        &self,
        symbol: &str,
        qty: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        self.limit("BUY", symbol, qty, price, client_id)
    }

    fn limit_sell(
        &self,
        symbol: &str,
        qty: f64,
        price: f64,
        client_id: Option<&str>,
    ) -> Result<Value, VenueError> {
        self.limit("SELL", symbol, qty, price, client_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offline_guard_covers_every_binance_call() {
        let _env = crate::testenv::EnvGuard::offline();
        let b = Binance::new(Credentials {
            key: "k".into(),
            secret: "s".into(),
        });
        assert!(b.balances_full().unwrap_err().is_offline());
        assert!(b.price("AAAUSDC").unwrap_err().is_offline());
        assert!(b
            .market_buy("AAAUSDC", 10.0, Some("csAAAb1r1"))
            .unwrap_err()
            .is_offline());
    }

    #[test]
    fn the_query_is_signed_with_time_and_window_appended() {
        let b = Binance::new(Credentials {
            key: "k".into(),
            secret: "s".into(),
        });
        let q = b.signed_query(&[("symbol", "AAAUSDC".into())], 1_700_000_000_000);
        assert!(q.starts_with("symbol=AAAUSDC&timestamp=1700000000000&recvWindow=5000&signature="));
        assert_eq!(q.rsplit('=').next().unwrap().len(), 64, "HMAC-SHA256 hex");
    }
}
