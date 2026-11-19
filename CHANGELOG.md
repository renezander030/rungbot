# Changelog

## 0.8.2

- Report a named input error instead of panicking when a window or monthly backtest history omits a configured coin, for both explicit history files and cached data.
- Preserve sweep exclusion of coins without history and valid replay results, with offline CLI regression coverage.

## 0.8.1

- Treat whitespace-only XDG cache and state directories as unset for backtests, consistently with the other CLI paths. Preserve explicit path overrides and nonempty paths containing spaces.
- Add CLI regression coverage for cache lookup and monthly expectation output under isolated HOME/XDG settings.
