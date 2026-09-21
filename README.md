# rungbot

A dip-buy / take-profit **ladder** for spot crypto, on Binance, Gate.io and Revolut X.

```console
$ rungbot plan
rungbot plan — 2026-09-21T06:47:31+00:00
bands 10/5 · core 20% · window 24h · knife floor -50% · trail off

BUY — nothing crossed a new dip rung

SELL — 1 coin(s) crossed a new profit rung
  BTC      +33.8% P&L  ->  rung 5 (+30%)  sell 30% of position   @ 81,640

COIN          PRICE      24H       ENTRY      P&L      TARGET   USED  FLAGS
ETH           2,663    +3.4%           -        -           -     0%
BTC          81,640    +1.5%      61,000   +33.8%      67,100     0%

WHY NOTHING HAPPENED
  ETH     no cost basis, so the sell side cannot be judged
```

**The `rungbot` binary holds no credentials and contains no request-signing code** — a
[test](crates/rungbot/tests/no_keys.rs) greps the shipped source and fails the build if
any appears. It reads public endpoints and tells you what it would do.

Placing orders needs an exchange key, and that lives in a **separate crate and binary**,
[`rungbot-exec`](#execution). Installing the ladder does not install the ability to trade.

## Quickstart

```bash
cargo install rungbot

rungbot init                                # writes ~/.config/rungbot/watchlist.yaml
$EDITOR ~/.config/rungbot/watchlist.yaml    # your coins, venues and cost basis
rungbot plan                                # what would I do right now?
rungbot plan --save                         # record that you acted, so rungs don't repeat
```

On a schedule: `*/30 * * * * rungbot plan --save`

Rust 1.82+. No system dependencies.

## Commands

| | |
|---|---|
| `rungbot plan` | what would I do right now? `--save` advances the ladder |
| `rungbot plan --steer` | read the market first, apply the bull sell policy |
| `rungbot plan --notify` | send it to a webhook or Telegram |
| `rungbot regime` | bull, chop or bear, and which coins are running |
| `rungbot kpi` | where each coin sits in its own cycle |
| `rungbot research` | what is deeply dislocated and still earns fees |
| `rungbot init` · `rungbot tickers` | starter config · just the prices |

All except `init` take `--json`. `--config PATH` works everywhere; `--state PATH` on `plan`.

## Config

```yaml
bands:
  first_pct: 10          # first rung fires at a 10% move
  step_pct: 5            # every further rung, 5% beyond the last

ladder:
  min_core_pct: 20       # never sell below this much of a position
  window_hours: 24       # after acting, a coin is locked to that direction
  buy_floor_pct: 50      # stop dip-buying past a 50% daily drop
  breaker_pct: 40        # freeze dip-buys on a coin 40% underwater…
  breaker_days: 7        # …for 7 straight days

coins:
  BTC:
    venue: binance       # binance BTCUSDT · gate BTC_USDT · revx BTC/USD · coingecko bitcoin
    pair: BTCUSDT
    entry: 61000         # cost basis; omit it and the coin is watched for dips only
  ETH:
    venue: gate
    pair: ETH_USDT
    bands:               # optional per-coin override
      first_pct: 20
      step_pct: 10
```

Every setting has an env override (`RUNGBOT_FIRST_PCT`, …). A value that does not parse
is a hard error. Inline `{a: 1}` flow YAML is not supported; the error says so.

All four venues are public and unauthenticated. Revolut X returns its whole book in one
request, so any number of Revolut coins costs one call.

## The ladder

A coin's 24h move (buys) and its profit against cost basis (sells) divide into **rungs**.
The first sits at `first_pct`, each further one `step_pct` beyond. Trade size equals move
size: a fresh rung 1 is 10%, a deepening rung 5%, a crash crossing rungs 1+2+3 is 20%.

1. **A rung fires once** — high-water mark; a retrace does not re-fire it.
2. **A 24h directional lock** — once a coin buys it may only keep buying, and vice versa.
3. **A budget that breathes** — each coin's cumulative buy budget is `max(0, 100% + its own P&L)`.
4. **A protected core** — sells stop at `min_core_pct`.
5. **A circuit breaker** — a coin below `-breaker_pct` for `breaker_days` stops being
   dip-bought, and says so once. It is never sold at a loss.

Plus a knife floor: past a `buy_floor_pct` daily drop, no buys.

## Beyond the ladder

**`--steer`** reads the market from public daily candles. A bull needs BTC above both its
100- and 200-day averages *and* half the watchlist above its own 30-day. In a confirmed
bull the sell side of each coin with a cost basis passes to a policy that sells tranches
at multiples of cost and trails the peak once armed, instead of the ladder harvesting the
move at rung 3. The core is never sold; nothing is sold below `cost + first_pct`.

**`rungbot kpi`** reports the Mayer multiple, Pi-cycle ratio, daily and weekly RSI,
drawdown, distance from the 200-day and a cycle phase. Values the history cannot support
read `-`; the phase reads `unknown`. Context only — the ladder does not read them.

**`rungbot research`** screens the market for coins deeply off their high that still earn
fees, value first and dislocation second (CoinPaprika + DefiLlama, both keyless).
`--llm 'claude -p'` pipes each candidate's facts to any command on stdin; without it the
gate is arithmetic. Never wired to the ladder.

**`--notify`** posts to a webhook or Telegram, deduplicated to one message per new signal
and one reminder a day. The bot token comes from `RUNGBOT_TELEGRAM_TOKEN`, never the
config. Every run also records why each coin did nothing; `--save` appends
`decisions.jsonl` beside the state file.

## Execution

```bash
cargo install rungbot-exec

rungbot plan --json > plan.json
rungbot-exec plan --from plan.json --budget 1000 --pair-map BTC=BTC_USDT
rungbot-exec sync --from plan.json --budget 1000 --pair-map BTC=BTC_USDT \
    --live --i-understand
```

The ladder decides with no key loaded; the rails refuse; the journal writes a
deterministic id **before** the venue is called; only then is a request signed.
**GTC limit orders only** — a resting order fills while the machine is asleep.

| Rail | Default | |
|---|---|---|
| mode | off | `--live` |
| acknowledgement | none | `--i-understand`, every run |
| halt file | `~/.config/rungbot/HALT` | present ⇒ nothing is placed |
| per order / per day | 50 / 200 across 10 | `--max-order` `--max-daily` `--max-orders` |
| slippage | 2% | `--max-slippage` |

The client id derives from intent — symbol, side, rung, 30-minute window — so a crash
between the venue accepting an order and the state being saved cannot place it twice.

Keys come from `RUNGBOT_GATE_KEY` / `RUNGBOT_GATE_SECRET` or a mode-600 file, never the
watchlist; a key file others can read is refused. **Gate disables a key with no IP
allowlist after 90 days, silently.** `rungbot-exec keys check` verifies the key works from
your address and states what it cannot check — Gate exposes no permission endpoint, so
confirm withdrawals are off in their UI yourself. Gate only, for now.

## Layout

| Crate | |
|---|---|
| [`rungbot-core`](crates/rungbot-core) | The strategy as pure logic. No clock, files or network; compiles to `wasm32`. |
| [`rungbot`](crates/rungbot) | The `rungbot` binary. Holds no key. |
| [`rungbot-exec`](crates/rungbot-exec) | The `rungbot-exec` binary. Holds the key. Opt-in. |

## Testing

`cargo test --workspace` runs fully offline: `RUNGBOT_OFFLINE=1` makes every network layer
refuse, so a test that forgets to inject data fails instead of hitting an exchange. CI
covers Linux, macOS and Windows, clippy, rustfmt, the `wasm32` build, packaging and an
end-to-end smoke test.

[The golden test](crates/rungbot-core/tests/golden.rs) replays a frozen ten-step scenario
generated by the reference implementation this crate replaced, so a refactor that changes
sizing or a boundary fails even when every unit test passes.

## Risk

Not financial advice, and no warranty. **Ladders lose money in a sustained downtrend: you
buy every rung on the way down.** Rule 5 exists because that happens. `rungbot` is not a
backtester, not a portfolio tracker and not an executor; it does not know your balances,
which is why sizes are percentages.

MIT. See [LICENSE](LICENSE).
