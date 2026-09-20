//! Public, unauthenticated price feeds. No API key is used, created or accepted here.
//!
//! This is the only module in rungbot that touches the network, and it only ever issues
//! GETs against public ticker endpoints. There is no request signing anywhere in this
//! workspace: `rungbot plan` cannot place an order even if you asked it to.
//!
//! Set `RUNGBOT_OFFLINE=1` and every call fails instead of reaching the network, so a
//! test that forgets to inject prices fails loudly rather than hitting an exchange.

use std::collections::BTreeMap;
use std::fmt;

use rungbot_core::{Coin, Price, Venue};
use serde_json::Value;

pub const USER_AGENT: &str = concat!(
    "rungbot/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/renezander030/rungbot)"
);
const TIMEOUT_S: u64 = 15;

#[derive(Debug)]
pub enum TickerError {
    Offline(String),
    Failed(String),
}

impl fmt::Display for TickerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TickerError::Offline(m) | TickerError::Failed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for TickerError {}

/// One public GET returning JSON. The only way this crate reaches the network.
pub fn get_json(url: &str) -> Result<Value, TickerError> {
    if std::env::var("RUNGBOT_OFFLINE").as_deref() == Ok("1") {
        return Err(TickerError::Offline(format!(
            "RUNGBOT_OFFLINE=1 refuses network call: {url}"
        )));
    }
    let resp = minreq::get(url)
        .with_header("User-Agent", USER_AGENT)
        .with_header("Accept", "application/json")
        .with_timeout(TIMEOUT_S)
        .send()
        .map_err(|e| TickerError::Failed(format!("{url}: {e}")))?;
    let body = resp
        .as_str()
        .map_err(|e| TickerError::Failed(format!("{url}: non-UTF8 response: {e}")))?;
    if resp.status_code != 200 {
        let head: String = body.chars().take(120).collect();
        return Err(TickerError::Failed(format!(
            "{url} -> {} {head}",
            resp.status_code
        )));
    }
    serde_json::from_str(body).map_err(|e| TickerError::Failed(format!("{url}: bad JSON: {e}")))
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `(price, 24h change %)` from Binance's public 24hr ticker. `pair` e.g. `BTCUSDT`.
pub fn binance(pair: &str) -> Result<(f64, f64), TickerError> {
    let body = get_json(&format!(
        "https://api.binance.com/api/v3/ticker/24hr?symbol={pair}"
    ))?;
    let (Some(p), Some(c)) = (
        body.get("lastPrice").and_then(num),
        body.get("priceChangePercent").and_then(num),
    ) else {
        return Err(TickerError::Failed(format!(
            "binance {pair}: unexpected response shape"
        )));
    };
    Ok((p, c))
}

/// `(price, 24h change %)` from Gate.io's public spot tickers. `pair` e.g. `BTC_USDT`.
pub fn gate(pair: &str) -> Result<(f64, f64), TickerError> {
    let body = get_json(&format!(
        "https://api.gateio.ws/api/v4/spot/tickers?currency_pair={pair}"
    ))?;
    let first = body
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| TickerError::Failed(format!("gate {pair}: empty ticker list")))?;
    let (Some(p), Some(c)) = (
        first.get("last").and_then(num),
        first.get("change_percentage").and_then(num),
    ) else {
        return Err(TickerError::Failed(format!(
            "gate {pair}: unexpected response shape"
        )));
    };
    Ok((p, c))
}

/// `(price, 24h change %)` from CoinGecko. `pair` is the coingecko id, e.g. `bitcoin`.
///
/// A fallback for coins on neither venue. Rate-limited when unauthenticated; fine for a
/// handful of coins on a 30-minute cadence, not for a tight loop.
pub fn coingecko(id: &str) -> Result<(f64, f64), TickerError> {
    let body = get_json(&format!(
        "https://api.coingecko.com/api/v3/simple/price\
         ?ids={id}&vs_currencies=usd&include_24hr_change=true"
    ))?;
    let row = body
        .get(id)
        .ok_or_else(|| TickerError::Failed(format!("coingecko {id}: not in response")))?;
    let p = row
        .get("usd")
        .and_then(num)
        .ok_or_else(|| TickerError::Failed(format!("coingecko {id}: no usd price")))?;
    Ok((p, row.get("usd_24h_change").and_then(num).unwrap_or(0.0)))
}

/// Every Revolut X pair in one call, as `{symbol: (price, 24h change %)}`.
///
/// Revolut X publishes its whole book from a single endpoint (about 780 pairs), so a
/// watchlist with six Revolut coins costs one request, not six.
///
/// Two quirks of this feed, both of which cost real money if you get them wrong:
///
/// * **`price_change_24h` is an absolute move, not a percentage.** BTC shows `-315.84`,
///   not `-0.39`. Feeding that straight into the ladder as a percentage would read a
///   routine day as a catastrophic crash and fire every dip rung at once.
/// * **`last_price` can be missing on a pair that has not traded**, in which case the
///   mid of bid/ask is the honest price.
fn revx_row(row: &Value) -> Option<(f64, f64)> {
    let last = row
        .get("last_price")
        .and_then(num)
        .filter(|p| *p > 0.0)
        .or_else(
            || match (row.get("bid").and_then(num), row.get("ask").and_then(num)) {
                (Some(b), Some(a)) if b > 0.0 && a > 0.0 => Some((b + a) / 2.0),
                _ => None,
            },
        )?;
    // Absolute -> percentage, measured against the price 24h ago.
    let abs = row.get("price_change_24h").and_then(num).unwrap_or(0.0);
    let prev = last - abs;
    let pct = if prev != 0.0 { abs / prev * 100.0 } else { 0.0 };
    Some((last, pct))
}

pub fn revx_all() -> Result<BTreeMap<String, (f64, f64)>, TickerError> {
    let body = get_json("https://revx.revolut.com/api/1.0/public/tickers")?;
    let rows = body
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| TickerError::Failed("revx: response has no `data` array".into()))?;

    let mut out = BTreeMap::new();
    for row in rows {
        let Some(sym) = row.get("symbol").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(quote) = revx_row(row) {
            out.insert(sym.to_string(), quote);
        }
    }
    if out.is_empty() {
        return Err(TickerError::Failed(
            "revx: no usable tickers in the feed".into(),
        ));
    }
    Ok(out)
}

/// `(price, 24h change %)` for one Revolut X pair, e.g. `BTC/USD`.
pub fn revx(pair: &str) -> Result<(f64, f64), TickerError> {
    revx_all()?
        .get(pair)
        .copied()
        .ok_or_else(|| TickerError::Failed(format!("revx {pair}: not in the ticker feed")))
}

pub fn one(coin: &Coin) -> Result<(f64, f64), TickerError> {
    match coin.venue {
        Venue::Binance => binance(&coin.pair),
        Venue::Gate => gate(&coin.pair),
        Venue::Revx => revx(&coin.pair),
        Venue::Coingecko => coingecko(&coin.pair),
    }
}

/// Feeds that answer for many pairs at once, loaded lazily and only if needed.
#[derive(Default)]
struct Batched {
    revx: Option<BTreeMap<String, (f64, f64)>>,
}

impl Batched {
    fn quote(&mut self, coin: &Coin) -> Result<(f64, f64), TickerError> {
        match coin.venue {
            Venue::Revx => {
                if self.revx.is_none() {
                    self.revx = Some(revx_all()?);
                }
                let all = self.revx.as_ref().expect("just loaded");
                all.get(&coin.pair).copied().ok_or_else(|| {
                    TickerError::Failed(format!("revx {}: not in the ticker feed", coin.pair))
                })
            }
            _ => one(coin),
        }
    }
}

/// Prices for every coin, as `{symbol: Price}`.
///
/// A coin whose ticker fails after `retries` is omitted, so `analyze` reports it as an
/// error and leaves that coin's ladder untouched. If every coin fails, return an error:
/// that is a broken run, not a quiet no-op.
pub fn fetch(coins: &[Coin], retries: u32) -> Result<BTreeMap<String, Price>, TickerError> {
    let mut out = BTreeMap::new();
    let mut last: Option<TickerError> = None;
    let mut batched = Batched::default();
    for coin in coins {
        for attempt in 0..retries.max(1) {
            match batched.quote(coin) {
                Ok((price, chg)) => {
                    out.insert(
                        coin.symbol.clone(),
                        Price {
                            price,
                            chg_24h: Some(chg),
                        },
                    );
                    break;
                }
                Err(e @ TickerError::Offline(_)) => {
                    last = Some(e); // offline is deliberate; do not retry
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    if attempt + 1 < retries.max(1) {
                        std::thread::sleep(std::time::Duration::from_secs(4));
                    }
                }
            }
        }
    }
    if out.is_empty() {
        let msg = last
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no coins".into());
        return match msg.contains("RUNGBOT_OFFLINE") {
            true => Err(TickerError::Offline(msg)),
            false => Err(TickerError::Failed(format!(
                "every ticker failed; last error: {msg}"
            ))),
        };
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_identifies_the_tool() {
        assert!(USER_AGENT.starts_with("rungbot/"));
    }

    /// A real row from the Revolut X feed, kept verbatim so the shape is pinned.
    fn btc_row() -> Value {
        serde_json::json!({
            "symbol": "BTC/USD", "bid": "81091.13", "ask": "81120.45", "mid": "81105.79",
            "last_price": "81136.72", "low_24h": "80109.9", "high_24h": "81518.28",
            "price_change_24h": "-315.84", "volume_24h": "65.45", "region": "EEA"
        })
    }

    #[test]
    fn revx_converts_an_absolute_move_into_a_percentage() {
        // The whole point: -315.84 on a ~81k coin is -0.39%, not -315.84%. Reading it
        // as a percentage would fire every dip rung on a completely ordinary day.
        let (price, pct) = revx_row(&btc_row()).expect("a complete row parses");
        assert!((price - 81_136.72).abs() < 1e-6);
        let expected = -315.84 / (81_136.72 + 315.84) * 100.0;
        assert!((pct - expected).abs() < 1e-9, "got {pct}, want {expected}");
        assert!(
            pct > -1.0 && pct < 0.0,
            "a routine day must read as a routine day: {pct}"
        );
    }

    #[test]
    fn revx_falls_back_to_the_mid_when_a_pair_has_not_traded() {
        let row = serde_json::json!({
            "symbol": "XYZ/USD", "bid": "10.0", "ask": "12.0", "price_change_24h": "0"
        });
        let (price, pct) = revx_row(&row).expect("bid/ask alone is enough");
        assert_eq!(price, 11.0, "the mid of bid and ask");
        assert_eq!(pct, 0.0);

        let zero = serde_json::json!({ "symbol": "Z/USD", "last_price": "0" });
        assert!(
            revx_row(&zero).is_none(),
            "a pair with no price at all is skipped"
        );
    }

    #[test]
    fn revx_handles_a_pair_that_doubled() {
        let row = serde_json::json!({ "last_price": "200", "price_change_24h": "100" });
        let (_, pct) = revx_row(&row).unwrap();
        assert!(
            (pct - 100.0).abs() < 1e-9,
            "200 from 100 is +100%, got {pct}"
        );
    }

    #[test]
    fn offline_is_refused_not_retried() {
        std::env::set_var("RUNGBOT_OFFLINE", "1");
        let e = binance("BTCUSDT").expect_err("offline must block the call");
        assert!(matches!(e, TickerError::Offline(_)), "got {e:?}");
        assert!(e.to_string().contains("RUNGBOT_OFFLINE"), "and says why");
        std::env::remove_var("RUNGBOT_OFFLINE");
    }
}
