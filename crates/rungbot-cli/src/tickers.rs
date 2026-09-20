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

fn get(url: &str) -> Result<Value, TickerError> {
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
    let body = get(&format!(
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
    let body = get(&format!(
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
    let body = get(&format!(
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

pub fn one(coin: &Coin) -> Result<(f64, f64), TickerError> {
    match coin.venue {
        Venue::Binance => binance(&coin.pair),
        Venue::Gate => gate(&coin.pair),
        Venue::Coingecko => coingecko(&coin.pair),
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
    for coin in coins {
        for attempt in 0..retries.max(1) {
            match one(coin) {
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

    #[test]
    fn offline_is_refused_not_retried() {
        std::env::set_var("RUNGBOT_OFFLINE", "1");
        let e = binance("BTCUSDT").expect_err("offline must block the call");
        assert!(matches!(e, TickerError::Offline(_)), "got {e:?}");
        assert!(e.to_string().contains("RUNGBOT_OFFLINE"), "and says why");
        std::env::remove_var("RUNGBOT_OFFLINE");
    }
}
