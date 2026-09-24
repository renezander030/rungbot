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

## Three regimes, and the ladder is only right in two of them

A market is in one of three states, and the same ladder is not correct in all three.

| | | |
|---|---|---|
| **Bear** | BTC below its 200-day average and the book weak with it | Dip-buying is the job. The circuit breaker and the knife floor exist for this: a coin can fall a long way further than looks possible, and the ladder must stop feeding it. |
| **Chop** | anything in between | Where the ladder is at its best. It buys the dips and sells the rallies of a range that goes nowhere, which is most of the time. |
| **Bull** | BTC above **both** its 100- and 200-day averages, *and* half the watchlist above its own 30-day | Where the ladder is at its worst. It sells a running coin at rung 3 and hands you the rest of the move to watch from the sidelines. |

That last case is why `--steer` exists. In a confirmed bull the sell side passes to a
policy that takes tranches at multiples of cost and trails the peak, instead of harvesting
the move early. In chop and bear the ladder's own sell rungs run, because that is what
they are good at.

```console
$ rungbot regime
market: bull
BTC 81561  sma100 68502  sma200 70593
breadth above 30d SMA: 2/2

    BTC     2/4  above 30d SMA, fresh 30d high
    ETH     2/4  above 30d SMA, fresh 30d high
```

The per-coin line is a four-signal strength read; three of four makes a coin "running",
which is a separate question from what the market as a whole is doing.

A bull is deliberately hard to declare — one coin running is not a bull market, and
calling one early is how you turn off the behaviour that was working.

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
| `rungbot backtest` | the ladder replayed over history: window, sweep, monthly verdict, studies |
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

**`--steer`** applies the regime read above. In a confirmed bull the sell policy sells a
tranche the first time price reaches each multiple of cost, and trails the running peak
once it arms, re-arming from each hit. `--armed BTC` forces a one-shot exit when you have
a signal the tool does not. The core is never sold, and nothing is ever sold below
`cost + first_pct`.

**`rungbot kpi`** reports the Mayer multiple, Pi-cycle ratio, daily and weekly RSI,
drawdown, distance from the 200-day and a cycle phase. Values the history cannot support
read `-`; the phase reads `unknown`. Context only — the ladder does not read them.

**`rungbot research`** screens the market for coins deeply off their high that still earn
fees, value first and dislocation second (CoinPaprika + DefiLlama, both keyless).
`--llm 'claude -p'` pipes each candidate's facts to any command on stdin; without it the
gate is arithmetic. Never wired to the ladder.

**`rungbot research <stage>`** is the weekly version of that screen, as a pipeline that
composes through one ledger file:

| Stage | Does |
|---|---|
| `oppscan` | the tradable band, ranked by distance from the all-time high (CoinPaprika) |
| `survivor build-index \| select \| run` | a DefiLlama fees/TVL index, a value-first pick, then web evidence and one LLM verdict per coin |
| `catalyst search \| synthesize` | web evidence of a forward catalyst, then one LLM verdict per coin |
| `unlocks build-index \| enrich` | the next 180 days of token-unlock cliffs (DefiLlama's open CDN) |
| `report [--refresh] [--commit]` | reads the regime, runs the matching thesis, writes one dated note and a short email |
| `theses [--example]` | which screen runs in which regime; the wording lives in a YAML file you edit |

No model provider is built in. `research.llm.command` names any command that reads a
prompt on stdin and answers on stdout; rungbot **always** appends a spend cap to it
(`--max-budget-usd 0.10` by default, flag and amount configurable, never off) and adds
nothing else, so it runs with exactly the permissions you wrote into the command. With no
command configured, the LLM stages refuse to run. The search key (`EXA_API_KEY`), the
Resend key and the note token are read from the environment, never from the config.
`report` without `--commit` is a dry run. See `rungbot research help` and
[`contrib/systemd`](contrib/systemd) for a weekly timer.

**`--notify`** posts to a webhook or Telegram, deduplicated to one message per new signal
and one reminder a day. The bot token comes from `RUNGBOT_TELEGRAM_TOKEN`, never the
config. Every run also records why each coin did nothing; `--save` appends
`decisions.jsonl` beside the state file.

## Watchers

`rungbot watch <regime|btc|zone|froth|divergence|daily>` runs the read-only checks that
mail you when the market or the book needs a human. None of them places, cancels or
halts anything; each mail says what to run by hand. `--dry-run` prints instead of
sending. State files sit side by side and are written by rename, so a crash never
leaves half a file.

| Watcher | Mails when |
|---|---|
| `regime` | the bull/chop/bear label flips, and again when it confirms (14 days held). A confirmed label that later returns is announced again. |
| `btc` | BTC enters the heads-up band or breaks your alert line. Once per crossing; re-arms 2% back above. |
| `zone` | a resting buy has sat far below spot too long, a coin starts or stops running, or cash sits idle. |
| `froth` | the heat level changes: fear & greed, funding, open interest, BTC's Mayer multiple. |
| `divergence` | live results fall behind the last backtest, or the drawdown passes its worst window. Once per guard per baseline. |

`daily` runs `froth` then `zone`. Email goes through Resend (`RESEND_API_KEY`), Telegram
through `RUNGBOT_TELEGRAM_TOKEN`. The watchers read the executor's order journal and a
balances snapshot from the paths under `watch:`, so `rungbot` still holds no venue key.
Example systemd units are in [`contrib/systemd`](contrib/systemd).

**`rungbot backtest`** replays the configured ladder over past prices; it never trades.
`window --book FILE` runs the last 28 days (`--days N`) hour by hour from a start book of
holdings and free stable, with fees and slippage, and writes the book it started from.
`sweep` replays the same window under nine band and core variants. `monthly` runs
several windows (`backtest.windows`, default 28/90/180), flags REVIEW when a variant
beats the live bands by more than `backtest.drift_band_pct`, probes per-coin bands, and
writes an expectation file a later run is measured against; `--dry-run` prints it,
`--notify` sends it. Price history comes from CoinGecko: give each coin a
`coingecko: <id>`. For a monthly verdict, schedule it and keep the log:

```
0 7 1 * *  rungbot backtest monthly --book ~/book.json --notify >> ~/backtest.log 2>&1
```

`backtest replay <study>` runs the research studies behind the sell policy over cached
daily candles: BTC confirmations and the dip rungs after them, whether they transfer to
alts, the anatomy of cycle tops, and the sell policy replayed across them.
`replay example` prints a study file to start from; `replay fetch` caches its candles.

## Execution

```bash
cargo install rungbot-exec

rungbot plan --json > plan.json
rungbot-exec plan --from plan.json --budget 1000 --pair-map BTC=BTC_USDT
rungbot-exec sync --from plan.json --budget 1000 --pair-map BTC=BTC_USDT \
    --live --i-understand
rungbot-exec reconcile      # books fills, part-fills and venue cancels
```

The ladder decides with no key loaded; the rails refuse; the journal writes a
deterministic id **before** the venue is called; only then is a request signed.
`sync` places **GTC limit orders only** — a resting order fills while the machine is
asleep. `reconcile` reads each open order back and books what happened: a fill net of
a base-coin fee, the filled part of an order that left the book, and a resize made in
the venue's own app, which it re-attaches to instead of writing the rung off. One
writer at a time holds the journal (`sync`, `reconcile`, `cancel`, `archive`,
`import-cex --write`; a second waits up to `RUNGBOT_LOCK_WAIT` seconds, default 120),
and a journal that exists but does not parse stops the run instead of reading as empty.

The journal is a plain JSON object keyed by client id; finished cancels older than 30
days move to `orders-archive.jsonl` with `rungbot-exec archive`. `rungbot-exec
import-cex DIR` reads an existing `orders-journal.json` in the same format, prints what
it found and checks every row reads back as written; `--write` imports it, together
with the ladder state, P&L ledger, stale-order flags, decision log, signal-notice
dedupe and level-alert state it finds beside it. `import-cex DIR --config` prints the
run config that bot runs with as YAML (its values are yours: keep the output private).

`rungbot-exec run` is one complete scheduled run from a single YAML file
([`contrib/rungbot-run.example.yaml`](contrib/rungbot-run.example.yaml) lists every
knob): regime label and RUN gate, dip-buy and sell signals, market orders behind every
rail when `trade_mode: live` and `live_trading_enabled: yes`, the bull sell policy,
housekeeping, the decision log `decisions.jsonl`, one mail per new signal and the BTC
level alert. `--dry-run` computes and prints without placing, saving or mailing
anything; a run that finds another holding the lock prints `SKIPPED` and exits 0.
[`contrib/systemd`](contrib/systemd) runs it every 30 minutes.

`rungbot-exec snapshot` is the read-only dashboard collector, on the same file's
`dashboard:` block. It writes `data.json` (balances, P&L, the order log, the onramp
top-up card, the regime read, decisions, churn), `scenarios.json` (today's book
replayed along past cycles under hold, the old ladder and the bull sell policy, the
book at fractions of each coin's all-time high, and a seeded Monte Carlo of the next
cycle top) and `wallets.json` (self-custody balances and staking from public chain
APIs, with an alert when it is time to start unbonding). With `DASHBOARD_DEPLOY=1` it
then runs `dashboard.deploy_command`; the third failed deploy in a row exits 1.

| Rail | Default | |
|---|---|---|
| mode | off | `--live` |
| acknowledgement | none | `--i-understand`, every run |
| halt file | `~/.config/rungbot/HALT` | present ⇒ nothing is placed |
| per order / per day | 50 / 200 across 10 | `--max-order` `--max-daily` `--max-orders` |
| slippage | 2% | `--max-slippage` |

The client id derives from intent — symbol, side, rung, 30-minute window — so a crash
between the venue accepting an order and the state being saved cannot place it twice.

Venues: Gate (the default), Revolut X and Binance, chosen with `--venue`. Keys come from
the environment (`RUNGBOT_GATE_KEY` / `RUNGBOT_GATE_SECRET`, `RUNGBOT_BINANCE_KEY` /
`RUNGBOT_BINANCE_SECRET`, `RUNGBOT_REVX_KEY` / `RUNGBOT_REVX_PRIVATE_KEY_PEM`) or a
mode-600 `~/.config/rungbot/<venue>.env`, never the watchlist; a key file (or Revolut X
private key) others can read is refused. **Gate disables a key with no IP allowlist
after 90 days, silently.** `rungbot-exec keys check --venue V` verifies the key works
from your address and states what it cannot check — no venue here exposes its key's
permissions, so confirm withdrawals are off in their UI yourself. `RUNGBOT_OFFLINE=1`
refuses every network call, signed or public.

## Layout

| Crate | |
|---|---|
| [`rungbot-core`](crates/rungbot-core) | The strategy as pure logic. No clock, files or network; compiles to `wasm32`. |
| [`rungbot`](crates/rungbot) | The `rungbot` binary. Holds no key. |
| [`rungbot-notify`](crates/rungbot-notify) | Email (Resend), Telegram and webhook, plus signal-notice dedupe. Shared by both binaries; holds no venue key. |
| [`rungbot-backtest`](crates/rungbot-backtest) | Backtests and replays as pure computation over price history. |
| [`rungbot-research`](crates/rungbot-research) | The weekly research pipeline: dislocation screen, fundamentals, unlocks, and an optional LLM verdict with a spend cap. Holds no venue key. |
| [`rungbot-exec`](crates/rungbot-exec) | The `rungbot-exec` binary. Holds the key. Opt-in. |

## Testing

`cargo test --workspace` runs fully offline: `RUNGBOT_OFFLINE=1` makes every network layer
refuse, so a test that forgets to inject data fails instead of hitting an exchange. CI
covers Linux, macOS and Windows, clippy, rustfmt, the `wasm32` build, packaging and an
end-to-end smoke test.

[The golden test](crates/rungbot-core/tests/golden.rs) replays a frozen ten-step scenario
generated by the reference implementation this crate replaced, so a refactor that changes
sizing or a boundary fails even when every unit test passes. The backtests and replays
have [their own goldens](crates/rungbot-backtest/tests), recorded from the reference's
backtest and study scripts on public candles and compared byte for byte.

## Risk

Not financial advice, and no warranty. **Ladders lose money in a sustained downtrend: you
buy every rung on the way down.** Rule 5 exists because that happens.

rungbot is not a backtester and not a portfolio tracker. It does not know your balances,
which is why sizes are percentages. It *is* an executor, but only once you install
`rungbot-exec` and arm it — the ladder on its own places nothing.

MIT. See [LICENSE](LICENSE).
