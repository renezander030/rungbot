# Changelog

## 0.8.1

- Treat whitespace-only XDG cache and state directories as unset for backtests, consistently with the other CLI paths. Preserve explicit path overrides and nonempty paths containing spaces.
- Add CLI regression coverage for cache lookup and monthly expectation output under isolated HOME/XDG settings.
