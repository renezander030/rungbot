# rungbot

A dip-buy / take-profit **ladder** for spot crypto, as a single command.

```console
$ rungbot plan
rungbot plan — 2026-09-20T15:48:36+00:00
bands 10/5 · core 20% · window 24h · knife floor -50% · trail off

BUY — nothing crossed a new dip rung

SELL — 1 coin(s) crossed a new profit rung
  BTC      +32.6% P&L  ->  rung 5 (+30%)  sell 30% of position   @ 80,878

COIN          PRICE      24H       ENTRY      P&L      TARGET   USED  FLAGS
BTC          80,878    -1.0%      61,000   +32.6%      67,100     0%
ETH           2,606    -1.4%       2,400    +8.6%       2,640     0%
SOL          108.50    -2.9%           -        -           -     0%

Notify-only. rungbot holds no keys and places no orders.
```

**rungbot tells you what it would do. It never does it.** There is no signing code in
this package, it accepts no API keys, and it only ever issues public GET requests to
venue ticker endpoints. A test in the suite greps the source for `hmac`, `api_secret`
and friends and fails the build if any of them ever appear.

## Install

```bash
uvx rungbot plan          # no install at all
pipx install rungbot      # or keep it around
pip install rungbot
```

Python 3.10+. **No dependencies.** PyYAML is used if you happen to have it and a small
built-in reader handles the config format if you do not, so `rungbot` works on a bare
interpreter, in a slim container, and on a Pi.

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
`step_pct` beyond the last. Crossing a rung is what triggers a suggestion, and the size
of the suggestion is the size of the move: a fresh rung 1 is 10%, a deepening rung is 5%,
and a crash that crosses rungs 1+2+3 at once is 10+5+5 = 20%.

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

## Testing

```bash
./run-tests.sh
```

Every test runs as a plain script with `RUNGBOT_OFFLINE=1`, which makes the ticker layer
refuse all network calls, and with `HOME` pointed at an empty directory so no config or
state on your machine can leak into a result. CI runs the same script on Python 3.10,
3.12 and 3.13, once bare and once with PyYAML installed, then installs the wheel and runs
the real entry point.

`tests/test_golden.py` pins a ten-step market scenario byte-for-byte. The unit tests say
each rule is correct; the golden file says the whole thing still behaves exactly as it
did, so a refactor that quietly changes sizing or a boundary is caught even when every
unit test still passes. Regenerate it with `--update`, then read the diff. If you cannot
explain every changed line, do not commit it.

## What this is not

It is not a backtester, not a portfolio tracker, and not an executor. It does not know
your balances and does not ask for them, which is why sizes are percentages rather than
amounts.

**It is not financial advice and it carries no warranty.** Ladders lose money in a
sustained downtrend: you buy every rung on the way to zero. Rule 5 exists because that
happens. Read the rules above, decide whether you agree with them, and size accordingly.

## Licence

MIT. See [LICENSE](LICENSE).
