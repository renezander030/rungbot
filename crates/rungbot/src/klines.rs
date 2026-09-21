//! Daily candles for the steering layer. Public endpoints, no authentication.
//!
//! The regime read needs about 220 daily closes per coin — enough to cover a 200-day
//! SMA. Two things make that more than a loop over `GET`:
//!
//! * **Venue response shapes differ.** Binance puts the close at index 4 of each row,
//!   Gate at index 2, CoinGecko returns `[timestamp, price]` pairs.
//! * **A thin venue is the wrong place to read the market.** Revolut X books are
//!   shallow with short candle history, so a Revolut-routed coin proxies its candles to
//!   a deep venue. Where an order goes and where the truth comes from are two different
//!   questions.

use rungbot_core::{Coin, Venue};

use crate::tickers::{get_json, TickerError};

/// Where a coin's candles come from, which is not always where it trades.
pub fn kline_source(coin: &Coin, override_spec: Option<&str>) -> Result<(Venue, String), String> {
    if let Some(spec) = override_spec {
        let (v, p) = spec
            .split_once(':')
            .ok_or_else(|| format!("`klines: {spec}` should look like `binance:BTCUSDT`"))?;
        let venue = Venue::parse(v).map_err(|e| e.0)?;
        return Ok((venue, p.to_string()));
    }
    match coin.venue {
        // A Revolut pair like `BTC/USD` proxies to Binance `BTCUSDT` by default. Set
        // `klines:` on the coin when that guess is wrong.
        Venue::Revx => {
            let base = coin.pair.split('/').next().unwrap_or(&coin.pair);
            Ok((Venue::Binance, format!("{base}USDT")))
        }
        v => Ok((v, coin.pair.clone())),
    }
}

fn as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Daily closes with their candle open times, oldest first.
///
/// The weekly RSI needs the timestamps to group days into ISO weeks; the regime read
/// does not, which is why the plain [`closes`] exists alongside this.
pub fn closes_with_times(
    venue: Venue,
    pair: &str,
    days: usize,
) -> Result<(Vec<f64>, Vec<f64>), TickerError> {
    let (url, close_idx, time_idx) = match venue {
        Venue::Binance => (
            format!("https://api.binance.com/api/v3/klines?symbol={pair}&interval=1d&limit={days}"),
            4usize,
            0usize,
        ),
        Venue::Gate => (
            format!(
                "https://api.gateio.ws/api/v4/spot/candlesticks\
                 ?currency_pair={pair}&interval=1d&limit={days}"
            ),
            2usize,
            0usize,
        ),
        // CoinGecko returns [timestamp_ms, price] pairs and Revolut has no history;
        // both fall back to closes without times, so the weekly read is simply blank.
        _ => return closes(venue, pair, days).map(|c| (c, Vec::new())),
    };
    let body = get_json(&url)?;
    let rows = body.as_array().ok_or_else(|| {
        TickerError::Failed(format!("{} {pair}: not a candle array", venue.as_str()))
    })?;
    let mut cs = Vec::with_capacity(rows.len());
    let mut ts = Vec::with_capacity(rows.len());
    for r in rows {
        let (Some(c), Some(t)) = (
            r.get(close_idx).and_then(as_f64),
            r.get(time_idx).and_then(as_f64),
        ) else {
            continue;
        };
        cs.push(c);
        // Binance reports milliseconds, Gate seconds. Anything past year 10000 in
        // seconds is obviously the former.
        ts.push(if t > 1e11 { t / 1000.0 } else { t });
    }
    if cs.is_empty() {
        return Err(TickerError::Failed(format!(
            "{} {pair}: no closes in the candle response",
            venue.as_str()
        )));
    }
    Ok((cs, ts))
}

/// Daily closes, oldest first.
pub fn closes(venue: Venue, pair: &str, days: usize) -> Result<Vec<f64>, TickerError> {
    let (url, idx) = match venue {
        Venue::Binance => (
            format!("https://api.binance.com/api/v3/klines?symbol={pair}&interval=1d&limit={days}"),
            4usize,
        ),
        Venue::Gate => (
            format!(
                "https://api.gateio.ws/api/v4/spot/candlesticks\
                 ?currency_pair={pair}&interval=1d&limit={days}"
            ),
            2usize,
        ),
        Venue::Coingecko => {
            let body = get_json(&format!(
                "https://api.coingecko.com/api/v3/coins/{pair}/market_chart\
                 ?vs_currency=usd&days={days}&interval=daily"
            ))?;
            let rows = body
                .get("prices")
                .and_then(|p| p.as_array())
                .ok_or_else(|| TickerError::Failed(format!("coingecko {pair}: no prices")))?;
            // `[timestamp_ms, price]` pairs.
            return Ok(rows
                .iter()
                .filter_map(|r| r.get(1).and_then(as_f64))
                .collect());
        }
        Venue::Revx => {
            return Err(TickerError::Failed(format!(
                "revx has no usable candle history for {pair}; \
                 set `klines: binance:<PAIR>` on the coin"
            )))
        }
    };

    let body = get_json(&url)?;
    let rows = body.as_array().ok_or_else(|| {
        TickerError::Failed(format!("{} {pair}: not a candle array", venue.as_str()))
    })?;
    let out: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.get(idx).and_then(as_f64))
        .collect();
    if out.is_empty() {
        return Err(TickerError::Failed(format!(
            "{} {pair}: no closes in the candle response",
            venue.as_str()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coin(venue: Venue, pair: &str) -> Coin {
        Coin {
            symbol: "X".into(),
            venue,
            pair: pair.into(),
            name: String::new(),
            entry: None,
            bands: None,
        }
    }

    #[test]
    fn a_deep_venue_reads_its_own_candles() {
        let (v, p) = kline_source(&coin(Venue::Binance, "BTCUSDT"), None).unwrap();
        assert_eq!((v, p.as_str()), (Venue::Binance, "BTCUSDT"));
    }

    #[test]
    fn a_revolut_pair_proxies_to_a_deep_venue() {
        let (v, p) = kline_source(&coin(Venue::Revx, "BTC/USD"), None).unwrap();
        assert_eq!(
            (v, p.as_str()),
            (Venue::Binance, "BTCUSDT"),
            "signals must read the real market, not a thin book"
        );
    }

    #[test]
    fn an_override_wins_over_the_guess() {
        let (v, p) = kline_source(&coin(Venue::Revx, "AKT/USD"), Some("gate:AKT_USDT")).unwrap();
        assert_eq!((v, p.as_str()), (Venue::Gate, "AKT_USDT"));
    }

    #[test]
    fn a_malformed_override_says_what_it_wanted() {
        let e = kline_source(&coin(Venue::Revx, "X/USD"), Some("binance")).unwrap_err();
        assert!(e.contains("binance:BTCUSDT"), "{e}");
        let e2 = kline_source(&coin(Venue::Revx, "X/USD"), Some("kraken:XBTUSD")).unwrap_err();
        assert!(e2.contains("venue must be one of"), "{e2}");
    }

    #[test]
    fn revx_candles_are_refused_with_the_fix_in_the_message() {
        let _env = crate::testenv::EnvGuard::offline();
        let e = closes(Venue::Revx, "BTC/USD", 220).unwrap_err().to_string();
        assert!(e.contains("klines: binance:"), "{e}");
    }
}
