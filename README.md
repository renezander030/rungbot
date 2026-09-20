# rungbot

A dip-buy / take-profit **ladder** for spot crypto, as a single command.

```console
$ rungbot plan
rungbot plan — 2026-09-20T17:01:40+00:00
bands 10/5 · core 20% · window 24h · knife floor -50% · trail off

BUY — nothing crossed a new dip rung

SELL — 1 coin(s) crossed a new profit rung
  BTC      +33.2% P&L  ->  rung 5 (+30%)  sell 30% of position   @ 81,278

COIN          PRICE      24H       ENTRY      P&L      TARGET   USED  FLAGS
ETH           2,636    -0.5%       2,400    +9.8%       2,640     0%
BTC          81,278    -0.6%      61,000   +33.2%      67,100     0%
SOL          110.11    -1.7%           -        -           -     0%

Notify-only. rungbot holds no keys and places no orders.
```

**rungbot tells you what it would do. It never does it.** It accepts no API key, contains
no request-signing code, and only ever issues public GETs to venue ticker endpoints. That
is not a promise in a README: [a test](crates/rungbot-cli/tests/no_keys.rs) greps the
shipped source for `hmac`, `api_secret` and friends and fails the build if any of them
ever appear.

## Install

```bash
cargo install rungbot-cli
```

Or grab a binary from [Releases](https://github.com/renezander030/rungbot/releases).
One static file, no runtime, no dependencies to install.

## Use

```bash
rungbot init              # writes ~/.config/rungbot/watchlist.yaml
$EDITOR ~/.config/rungbot/watchlist.yaml
rungbot plan              # what would I do right now?
rungbot plan --save       # ...and advance the ladder so those rungs don't repeat
rungbot plan --json       # for a cron, a dashboard, or a notifier
```

`plan` changes nothing unless you pass `--save`. Run it as often as you like.

```yaml
bands:
  first_pct: 10          # the first rung fires at a 10% move
  step_pct: 5            # every further rung, 5% beyond the last

ladder:
  min_core_pct: 20       # never sell below this much of a position
  window_hours: 24       # after acting, a coin is locked to that direction this long
  buy_floor_pct: 50      # stop dip-buying past a 50% daily drop
  breaker_pct: 40        # freeze dip-buys on a coin 40% underwater...
  breaker_days: 7        # ...for 7 straight days
  trail: off             # off = fire each rung on the way up; on = trail the peak

coins:
  BTC:
    venue: binance       # binance | gate | coingecko
    pair: BTCUSDT        # the venue's own symbol
    entry: 61000         # your cost basis; omit it to watch for dips only
  SOL:
    venue: gate
    pair: SOL_USDT
    bands:               # a volatile coin can have wider rungs of its own
      first_pct: 20
      step_pct: 10
```

Every setting also takes an env override (`RUNGBOT_FIRST_PCT`, `RUNGBOT_MIN_CORE_PCT`,
...). A value that does not parse is a hard error rather than a silent fallback, so a
typo in a cron file cannot quietly change your strategy.

## The strategy, in five rules

A coin's 24h move (for buys) and its profit against your cost basis (for sells) are each
divided into **rungs**. The first rung sits at `first_pct`; every further rung is
`step_pct` beyond the last. Crossing a rung triggers a suggestion, and the size of the
suggestion is the size of the move: a fresh rung 1 is 10%, a deepening rung is 5%, and a
crash that crosses rungs 1+2+3 at once is 10+5+5 = 20%.

1. **A rung fires once.** The ladder tracks a high-water mark. A retrace does not re-fire
   a rung you already acted on; only a deeper move advances it. This is the difference
   between a ladder and a chattering alert.
2. **A 24h directional lock.** Once a coin buys, it may only keep buying until the window
   closes, and the sell side is frozen. And vice versa. No round-tripping yourself inside
   a day.
3. **A budget that breathes.** Each coin's cumulative buy budget is
   `max(0, 100% + its own P&L)` of its base share. A winner earns a bigger dip-buy
   budget; a coin down 60% can only spend 40% of its slice. One coin in freefall can
   never eat the dry powder meant for the others.
4. **A protected core.** Sells stop at `min_core_pct`. The ladder will not sell you out of
   a position, ever.
5. **A circuit breaker.** A coin sitting below `-breaker_pct` for `breaker_days` straight
   stops being dip-bought, and says so once. It is never sold at a loss: the breaker only
   stops you throwing more money at it.

Plus a knife floor: past a `buy_floor_pct` daily drop, rungbot stops suggesting buys
entirely. Some dips are not dips.

## Layout

| Crate | What it is |
|---|---|
| [`rungbot-core`](crates/rungbot-core) | The entire strategy, as pure logic. Reads no clock, opens no file, makes no network call. |
| [`rungbot-cli`](crates/rungbot-cli) | The binary: config, public tickers, state, output. |

`rungbot-core` compiles unchanged to **`wasm32-unknown-unknown`**, which is how the same
ladder can run in a Cloudflare Worker without the strategy existing in two places. CI
builds that target on every push, so a dependency that breaks wasm is caught the day it
lands.

## Testing

```bash
cargo test --workspace
```

Tests run fully offline: `RUNGBOT_OFFLINE=1` makes the ticker layer refuse every network
call, so a test that forgets to inject prices fails loudly instead of hitting an exchange.
CI runs the suite on Linux, macOS and Windows, plus clippy, rustfmt, the wasm32 build, and
an end-to-end smoke test of the real binary.

**`tests/golden/ladder_scenario.json` is a cross-language contract.** It was generated by
the reference implementation this crate replaced, and
[the golden test](crates/rungbot-core/tests/golden.rs) replays the same ten-step market
scenario through the Rust core and asserts the same decisions come out. The unit tests say
each rule is correct; the golden file says the whole thing behaves exactly as the
implementation it replaced — so a refactor that quietly changes sizing, ordering or a
boundary is caught even when every unit test still passes.

## What this is not

It is not a backtester, not a portfolio tracker, and not an executor. It does not know
your balances and does not ask for them, which is why sizes are percentages rather than
amounts.

**It is not financial advice and it carries no warranty.** Ladders lose money in a
sustained downtrend: you buy every rung on the way to zero. Rule 5 exists because that
happens. Read the rules above, decide whether you agree with them, and size accordingly.

## Licence

MIT. See [LICENSE](LICENSE).
