# rungbot

A dip-buy / take-profit **ladder** for spot crypto.

```console
$ rungbot plan
rungbot plan — 2026-09-21T09:14:02+00:00
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

`rungbot` accepts no API key and contains no request-signing code. A
[test](crates/rungbot/tests/no_keys.rs) greps the shipped source and fails the build if
any appears. Live trading is a separate binary you install on purpose — see
[Execution](#execution).

## Quickstart

```bash
cargo install rungbot

rungbot init                                # writes ~/.config/rungbot/watchlist.yaml
$EDITOR ~/.config/rungbot/watchlist.yaml    # your coins, venues and cost basis
rungbot plan                                # what would I do right now?
```

`plan` is read-only. When you have acted on what it says, record that so those rungs
don't fire again:

```bash
rungbot plan --save
```

On a schedule:

```cron
*/30 * * * * rungbot plan --save
```

Add `--notify` once a [`notify:` block](#decision-log-and-notifications) is configured;
without one it exits with an error rather than going quietly.

Rust 1.82+. No system dependencies, no API key, no account.

## Commands

| | |
|---|---|
| `rungbot init` | write a starter config |
| `rungbot plan` | what would I do right now? |
| `rungbot plan --save` | …and advance the ladder so those rungs don't repeat |
| `rungbot plan --steer` | read the market first, apply the sell policy |
| `rungbot plan --notify` | send the result to a webhook or Telegram |
| `rungbot regime` | what market is this, and what is running? |
| `rungbot kpi` | where is each coin in its own cycle? |
| `rungbot research` | what is deeply dislocated and still earns fees? |
| `rungbot tickers` | just the prices |

Every command except `init` takes `--json`. `--config PATH` works everywhere;
`--state PATH` applies to `plan`.

Live trading is a second binary, `cargo install rungbot-exec` — see [Execution](#execution).

## Config

`rungbot init` writes `~/.config/rungbot/watchlist.yaml`.

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
  trail: off             # off = fire each rung; on = trail the peak

coins:
  BTC:
    venue: binance       # binance | gate | revx | coingecko
    pair: BTCUSDT
    entry: 61000         # your cost basis; omit to watch for dips only
  SOL:
    venue: gate
    pair: SOL_USDT
    bands:               # optional per-coin override
      first_pct: 20
      step_pct: 10
```

Every setting has an env override (`RUNGBOT_FIRST_PCT`, `RUNGBOT_MIN_CORE_PCT`, …). A
value that does not parse is a hard error, not a silent fallback. Inline `{a: 1}` flow
YAML is not supported; the error says so.

### Venues

| Venue | `pair` | Notes |
|---|---|---|
| `binance` | `BTCUSDT` | one request per coin |
| `gate` | `BTC_USDT` | one request per coin |
| `revx` | `BTC/USD` | Revolut X (EEA/UK). Whole book in one request. |
| `coingecko` | `bitcoin` | fallback; rate-limited unauthenticated |

All public and unauthenticated.

## The ladder

A coin's 24h move (buys) and its profit against cost basis (sells) are divided into
**rungs**. The first sits at `first_pct`; each further rung is `step_pct` beyond the last.
Trade size equals move size: a fresh rung 1 is 10%, a deepening rung is 5%, and a crash
crossing rungs 1+2+3 at once is 20%.

1. **A rung fires once.** High-water mark; a retrace does not re-fire it.
2. **24h directional lock.** Once a coin buys it may only keep buying until the window
   closes, and vice versa.
3. **A budget that breathes.** Each coin's cumulative buy budget is
   `max(0, 100% + its own P&L)` of its base share.
4. **A protected core.** Sells stop at `min_core_pct`.
5. **A circuit breaker.** A coin below `-breaker_pct` for `breaker_days` stops being
   dip-bought, and says so once. It is never sold at a loss.

Plus a knife floor: past a `buy_floor_pct` daily drop, no buys.

## Steering

`rungbot regime` reads the market from public daily candles: `bull`, `chop` or `bear` from
BTC's trend plus breadth, and a four-signal strength read per coin. A bull needs BTC above
**both** its 100- and 200-day SMAs *and* half the watchlist above its own 30-day SMA.

With a `sellpolicy:` configured and the market a confirmed bull, the sell side of each coin
with a cost basis is handed to the policy and the ladder's sell rungs go dormant:

- **Tranches** — sell a slice at each multiple of cost.
- **A trail** — arms at `trail_arm_mult × cost`, sells when the daily sample gives back
  `giveback_pct` from the peak, re-arms from each hit. Evaluated once per UTC day.
- **An armed exit** — `--armed SOL` forces a one-shot exit to the core.

The core is never sold and nothing is sold below `cost + first_pct`.

```console
$ rungbot plan --steer
market bull · breadth 3/3 above 30d SMA · running: none
SELL — nothing crossed a new profit rung
WHY NOTHING HAPPENED
  SOL     bull policy governs this coin; the sell ladder is dormant
```

Unsteered, that same SOL position at +22% sells 20% of itself at rung 3.

## Cycle indicators

`rungbot kpi` reads ~800 daily candles per coin.

```console
COIN          PRICE   MAYER     PI    RSI    wRSI     DD%   vs200%    VOL%  PHASE
BTC          81,258    1.15   0.43     64      55      35       15      37  markup
```

| | |
|---|---|
| **Mayer** | price ÷ 200-day SMA. ~2.4 has marked cycle tops; 1.8 is stretched. |
| **PI** | Pi-cycle ratio, `SMA(111) ÷ (2 × SMA(350))`. |
| **RSI / wRSI** | Wilder RSI(14) daily, and on completed weekly closes. |
| **DD% / vs200% / VOL%** | drawdown from high, distance from the 200-day, annualised 30d vol. |
| **PHASE** | capitulation, accumulation, markup, euphoria, markdown, unknown. |

Values the history cannot support read `-`, and the phase reads `unknown`. Context only —
the ladder does not read these.

## Research

`rungbot research` screens the market for coins deeply off their high that still earn
fees. Value first, then dislocation. Data from CoinPaprika and DefiLlama, both keyless.

```console
screened 2000 coins · rank 40-400 · 40-92% off high · volume >= $0.5M · fees floor $50k/30d

COIN      RANK OFF_HIGH     VOL_24H    FEES_30D  CATEGORY        VERDICT
ETHFI       98      92%      $28.4M       $9.8M  Liquid Staking  SURVIVOR
ARB         60      91%     $298.5M       $3.9M  Foundation      SURVIVOR
POL         77      92%      $37.3M           -  Chain           SPECULATIVE
```

Both band ends are bounded. Ranking is on dislocation alone; a seven-day bounce is shown
but not rewarded. `--llm 'claude -p'` pipes each candidate's facts to any command on stdin
and reads the answer back — rungbot calls no provider and holds no model credential.
Without it the gate is arithmetic.

Never wired to the ladder.

## Decision log and notifications

Every run records a reason per coin — the floor it hit, the window that locked it, the
breaker, the budget spent, the core it will not sell into. `--save` appends
`decisions.jsonl` beside the state file.

`--notify` posts to a webhook or sends a Telegram message, deduplicated to one message per
new signal and one reminder a day. The bot token comes from `RUNGBOT_TELEGRAM_TOKEN`,
never the config file.

## Execution

A separate crate and binary. Installing `rungbot` does not install the ability to trade,
and a test asserts the `rungbot` crate never depends on `rungbot-exec`.

```bash
rungbot plan --json > plan.json
rungbot-exec plan --from plan.json --budget 1000 --pair-map SOL=SOL_USDT
rungbot-exec sync --from plan.json --budget 1000 --pair-map SOL=SOL_USDT \
    --live --i-understand
```

The ladder decides with no key loaded; the rails refuse; the journal writes a
deterministic id **before** the venue is called; only then is a request signed.

**GTC limit orders only.** A resting order fills while the machine is asleep. Market
orders are not implemented.

| Rail | Default | Override |
|---|---|---|
| mode | off | `--live` |
| acknowledgement | none | `--i-understand`, every run |
| halt file | `~/.config/rungbot/HALT` | if present, nothing is placed |
| per order | 50 | `--max-order` |
| per day | 200 across 10 orders | `--max-daily`, `--max-orders` |
| slippage | 2% | `--max-slippage` |

```console
$ rungbot-exec plan --from plan.json --budget 100000 --pair-map SOL=SOL_USDT
  SELL  SOL_USDT  392.857 @ 140.00  ≈ 55000.00  REFUSED: exceeds the 50.00 per-order cap
```

**Idempotency.** The client id is derived from intent — symbol, side, rung, 30-minute
window — and journaled before the venue call, so a crash between acceptance and save
cannot place it twice. Reprice suffixes derive from the root id, never chained: chaining
grows the id past the venue's 36-character cap, after which orders silently stop being
placed.

**Keys.** From `RUNGBOT_GATE_KEY` / `RUNGBOT_GATE_SECRET` or a mode-600 file, never the
watchlist. A group- or world-readable key file is refused.

**Gate disables a key with no IP allowlist after 90 days, silently.** `rungbot-exec keys
check` verifies the key works from your current address, and states what it cannot check:
Gate exposes no endpoint for a key's permission set, so confirm withdrawals are off in
their UI yourself.

Gate only, for now.

## Layout

| Crate | |
|---|---|
| [`rungbot-core`](crates/rungbot-core) | The strategy as pure logic: ladder, regime, sell policy, indicators, research, decision log, notification dedupe. No clock, no files, no network. |
| [`rungbot`](crates/rungbot) | The `rungbot` binary: config, public tickers, state, output. Holds no key. |
| [`rungbot-exec`](crates/rungbot-exec) | The `rungbot-exec` binary: order journal, rails, Gate client. Opt-in. |

`rungbot-core` compiles unchanged to `wasm32-unknown-unknown`, so the same ladder can run
in a Cloudflare Worker. CI builds that target on every push.

## Testing

```bash
cargo test --workspace
```

The suite runs fully offline: `RUNGBOT_OFFLINE=1` makes every network layer refuse, so a
test that forgets to inject data fails instead of hitting an exchange. CI runs Linux, macOS and
Windows, plus clippy, rustfmt, the wasm32 build and an end-to-end smoke test.

`crates/rungbot-core/tests/golden/ladder_scenario.json` is a cross-language contract, generated by the
reference implementation this crate replaced.
[The golden test](crates/rungbot-core/tests/golden.rs) replays the same ten-step scenario
and asserts the same decisions, so a refactor that changes sizing or a boundary fails even
when every unit test passes.

## What this is not

Not a backtester, not a portfolio tracker. `rungbot` is not an executor. It does not know
your balances, which is why sizes are percentages.

**Not financial advice, and no warranty.** Ladders lose money in a sustained downtrend:
you buy every rung on the way down. Rule 5 exists because that happens.

## Licence

MIT. See [LICENSE](LICENSE).
