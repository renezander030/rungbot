# Changelog

## 0.8.3

- Restore current stable Clippy compatibility in exact arithmetic and trailing-stop replay without changing replay calculations.
- `rungbot-exec pairs` compares the host clock with Binance's and Gate's server time, warns past half of the venue's signed-request window and exits 1 outside it, before a drifting clock fails every signed order with a timestamp error.
- Clarify sampled-price window cadence and between-sample limits, with public-interface regression coverage for sampled dips, hourly/daily histories and inclusive trailing-stop dates.
- `rungbot-exec health` warns about open orders that have had no venue order id for over 15 minutes: reconcile polls by venue id, so such an order is never read back and would otherwise look alive.

## 0.8.2

- Report a named input error instead of panicking when a window or monthly backtest history omits a configured coin, for both explicit history files and cached data.
- Preserve sweep exclusion of coins without history and valid replay results, with offline CLI regression coverage.

## 0.8.1

- Treat whitespace-only XDG cache and state directories as unset for backtests, consistently with the other CLI paths. Preserve explicit path overrides and nonempty paths containing spaces.
- Add CLI regression coverage for cache lookup and monthly expectation output under isolated HOME/XDG settings.
