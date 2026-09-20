"""Public, unauthenticated price feeds. No API key is used, created or accepted here.

This is deliberately the only module in rungbot that touches the network, and it only
ever issues GETs against public ticker endpoints. There is no signing code in this
package at all: `rungbot plan` cannot place an order even if you asked it to.

Set ``RUNGBOT_OFFLINE=1`` and every call raises instead of reaching the network, so a
test that forgets to inject a fake feed fails loudly rather than hitting an exchange.
"""

from __future__ import annotations

import json
import os
import time
import urllib.error
import urllib.request

USER_AGENT = "rungbot/0.1 (+https://github.com/renezander030/rungbot)"
TIMEOUT_S = 15


class TickerError(RuntimeError):
    """A venue did not give us a usable price."""


class OfflineError(TickerError):
    """RUNGBOT_OFFLINE=1 blocked a network call."""


def _get(url: str):
    if os.environ.get("RUNGBOT_OFFLINE") == "1":
        raise OfflineError(f"RUNGBOT_OFFLINE=1 refuses network call: {url}")
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT,
                                               "Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT_S) as r:
            return r.status, json.loads(r.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        return e.code, e.read()[:200].decode("utf-8", "replace")
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, OSError) as e:
        raise TickerError(f"{url}: {e}") from e


def binance_ticker(pair: str):
    """(price, 24h change %) from Binance's public 24hr ticker. pair e.g. BTCUSDT."""
    st, b = _get(f"https://api.binance.com/api/v3/ticker/24hr?symbol={pair}")
    if st != 200 or not isinstance(b, dict) or "lastPrice" not in b:
        raise TickerError(f"binance {pair} -> {st} {str(b)[:120]}")
    return float(b["lastPrice"]), float(b["priceChangePercent"])


def gate_ticker(pair: str):
    """(price, 24h change %) from Gate.io's public spot tickers. pair e.g. BTC_USDT."""
    st, b = _get(f"https://api.gateio.ws/api/v4/spot/tickers?currency_pair={pair}")
    if st != 200 or not b or not isinstance(b, list):
        raise TickerError(f"gate {pair} -> {st} {str(b)[:120]}")
    return float(b[0]["last"]), float(b[0]["change_percentage"])


def coingecko_ticker(coin_id: str):
    """(price, 24h change %) from CoinGecko. pair is the coingecko id, e.g. bitcoin.

    A fallback for coins that are not on either venue. Rate-limited when unauthenticated;
    fine for a handful of coins on a 30-minute cadence, not for a tight loop.
    """
    st, b = _get("https://api.coingecko.com/api/v3/simple/price"
                 f"?ids={coin_id}&vs_currencies=usd&include_24hr_change=true")
    if st != 200 or not isinstance(b, dict) or coin_id not in b:
        raise TickerError(f"coingecko {coin_id} -> {st} {str(b)[:120]}")
    row = b[coin_id]
    return float(row["usd"]), float(row.get("usd_24h_change") or 0.0)


_FEEDS = {"binance": binance_ticker, "gate": gate_ticker, "coingecko": coingecko_ticker}


def fetch(coins, retries: int = 3, backoff: float = 4.0, sleep=time.sleep):
    """Prices for every coin in `coins`, as {symbol: {"price", "chg_24h"}}.

    A coin whose ticker fails after `retries` is simply omitted, so analyze() reports it
    as an error and leaves that coin's ladder untouched. If every coin fails, raise —
    that is a broken run, not a quiet no-op.
    """
    out, last_err = {}, None
    for coin in coins:
        feed = _FEEDS[coin.venue]
        for attempt in range(retries):
            try:
                price, chg = feed(coin.pair)
                out[coin.symbol] = {"price": price, "chg_24h": chg}
                break
            except TickerError as e:
                last_err = e
                if isinstance(e, OfflineError):
                    break                       # offline is deliberate; do not retry
                if attempt < retries - 1:
                    sleep(backoff)
    if not out:
        raise TickerError(f"every ticker failed; last error: {last_err}")
    return out
