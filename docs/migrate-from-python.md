# Migrating from the Python bot

`rungbot-exec` replaces the Python ladder bot it was ported from: the 30-minute
alert-and-trade run, the deploy layer, the watchers, the research report, the monthly
backtest and the dashboard collector. The state files keep their formats, so the move
is a copy, a side-by-side check, and a switch you can undo.

The order is:

1. [Translate the config](#1-translate-the-config)
2. [Import the state](#2-import-the-state)
3. [Run the shadow](#3-run-the-shadow) next to the old bot
4. [Diff every run](#4-diff-every-run) for a week
5. [Switch](#5-switch)
6. [Roll back](#6-roll-back), if you have to

Nothing here places an order until step 5.

## 1. Translate the config

The old bot keeps its knobs in two places: defaults in the modules and the exports in
its cron wrapper. One command reads both and prints the equivalent run config:

```bash
rungbot-exec import-cex /path/to/old-bot --config > ~/.config/rungbot/run.yaml
chmod 600 ~/.config/rungbot/run.yaml
```

The output holds personal values (cost basis, allocations, addresses). Keep it out of
any repository. It carries the halt file and the manual sell-arm file over **by path**:
both runtimes obey the same two files, so touching the halt file stops whichever one
is running.

Set `state_dir:` to where the new runtime keeps its state, and point the `notify:`
block at your mail and Telegram. Venue keys go in `~/.config/rungbot/env` or the
per-venue key files (`rungbot-exec --help`, ENVIRONMENT), never in the config.

## 2. Import the state

`import-cex` reads the old bot's directory and writes each state file where the run
config reads it. Without `--write` it is a dry run: it prints what it read and the
mapping, and writes nothing.

```bash
rungbot-exec import-cex /path/to/old-bot --dry-run    # the mapping, nothing written
rungbot-exec import-cex /path/to/old-bot --write      # write it
```

(The run config is found as for `run`: `RUNGBOT_RUN_CONFIG`, else
`~/.config/rungbot/run.yaml`.)

| Old file | Here | How |
|---|---|---|
| `orders-journal.json` | the journal | every row read and checked to read back as written |
| `orders-archive.jsonl` | beside the journal | re-written, same line format |
| `ladder-state.live.json` | `ladder-state.json` | the live ladder state |
| `ladder-state.dry.json` | `ladder-state.dry.json` | copied |
| `ladder-state.json` (the off-mode state) | `ladder-state.off.json` | copied |
| `pnl-ledger.json`, `ttl-warned.json` | same names | re-written, same format |
| `decisions.jsonl` | same name | copied, every line checked |
| `signal-notices.json`, `btc-alert-state.json` | same names | re-written, same format |
| `deploy-state.live.json` | `deploy-state.json` | the deploy layer's baselines and markers |
| `audit-state.json` | same name | the last book audit |
| `audit-state.json.last`, `orders-archive.jsonl.last` | same names | daily markers; their modification time is kept |
| `regime-state.json`, `regime-history.json`, `froth-state.json` | same names | copied |
| `zone-state.json`, `divergence-state.json`, `market-verdict.json`, `backtest-expectation.json` | the state dir | copied; `rungbot watch` reads them there when `watch.state_dir` is the state dir |
| `opportunity-ledger.json`, `value-index.json`, `unlock-index.json`, `theses.yaml` | `<state_dir>/research/` | copied; point `research.dir` there |
| `dashboard/manual-fills.json`, `dashboard/wallet-targets.json` and the collector's caches | the dashboard work dir | copied |
| `replay/cache/*.json` | the fill-odds candle cache | copied |
| the halt and sell-arm files | not copied | the config names the same paths |
| `.run.lock` | not copied | each runtime takes its own lock |
| the dashboard's `public/` | not copied | rewritten by every snapshot |
| files no job reads, backups (`*.json.bak*`) | not copied | listed with the reason |

Every copied file is checked to parse first; one that does not stops the import. The
dry run prints the same table for your directory, with a status per file: `new`,
`unchanged` (it already holds exactly this) or `differs`.

Importing is idempotent: a second import of the same source writes nothing. A target
that holds something else is refused, by name, before anything is written; `--force`
replaces it. The journal is written last.

## 3. Run the shadow

The shadow is the whole live cycle of the new runtime on a copy of the old bot's
state, with every venue write simulated:

```bash
rungbot-exec run --config run.yaml --dry-run --state-dir /scratch/copy
```

With `--state-dir`, a dry run is not a preview: it reconciles the journal against the
venues, does the housekeeping (paired sells, retries, reprices, cover restores), places
the ladder's orders, runs the deploy layer and the book audit, and saves its state in
the scratch directory, as a live run would. Venue reads are real: balances, open orders
and order status come from the venues, with the keys the old bot uses. Every write
(market and limit orders, cancels) is answered by the shadow itself, recorded, and
printed as a `SHADOW venue call pair ...` line; none leaves the process. No mail or
Telegram message is sent: they print as `Would send ...`. The halt and sell-arm files
are read where they are, so the shadow sees the same rails as the old bot.

`--state-dir` must not be the config's own state directory; the run refuses it.

For each run of the old bot:

1. Shortly before it (a minute is enough), copy its state directory to a fresh scratch
   directory. Copy, do not link: the shadow writes its state there.
2. `rungbot-exec import-cex <copy> --write` into a run config whose `state_dir` is the
   scratch directory (a second, private config, identical to the live one otherwise).
3. `rungbot-exec run --dry-run --state-dir <scratch> --config <that config>`.
4. When the old bot's run has finished, diff its new decision lines against the
   shadow's (step 4).

Both sides start from the same state and read the same venues; the prices they fetch
are a minute or two apart. That difference is what the diff's tolerance is for.

## 4. Diff every run

```bash
rungbot-exec shadow-diff old-bot/decisions.jsonl scratch/decisions.jsonl --since <TS>
```

`--since` is the epoch second of the copy: the imported log carries the old bot's
history, which is not part of the comparison. Runs pair by time (`--window`, default
1200 s). Within a pair, lines match on source, kind, coin, side and their text with the
numbers taken out; then the numbers: a rung, a count or a number of days must be
equal, an amount or a price must be within `--tolerance` percent (default 2), a
percentage within that many points. The signal counts of the run line must be equal.

The old bot named itself in some texts (its script, its log). Rewrite those before the
comparison with `--rename 'old text=new text,...'`.

A difference that one of the known, intentional divergences accounts for is printed as
`EXPLAINED [id] ... -- reason`. `rungbot-exec shadow-diff --list` prints them all: the
old bot's bugs this port fixes. Any other difference is `UNEXPLAINED`, and the command
exits 1 (2 when a log cannot be read).

Run the shadow for 7 days. The tripwire is one unexplained difference: stop, find out
why, and fix the port or explain the difference before you go on. A clean week is the
go signal.

## 5. Switch

1. Touch the halt file. Both runtimes stop placing orders.
2. Wait for any run of the old bot in progress to end, then stop and disable each of
   its timers.
3. Import once more, from the old bot's live directory, with `--force`: its state is
   now final.
   ```bash
   rungbot-exec import-cex /path/to/old-bot --dry-run
   rungbot-exec import-cex /path/to/old-bot --write --force
   ```
4. Enable the new units (`contrib/systemd/`, one per old job), still halted. Let one
   run go by and read its log: the lock, the prices, the reconcile, `halt file
   present`.
5. Remove the halt file. Watch the first live run: its decision lines, the orders it
   placed, the journal. The dashboard (`rungbot-exec snapshot`) should show the same
   book as before the switch.

## 6. Roll back

The new runtime writes the same formats, so going back is the import in reverse:

1. Touch the halt file.
2. Stop and disable the new units.
3. Copy the new state back over the old bot's files: the journal and archive as they
   are, `ladder-state.json` to `ladder-state.live.json`, `deploy-state.json` to
   `deploy-state.live.json`, and the others under their own names. Keep a copy of the
   old bot's directory from before the switch, in case.
4. Re-enable the old bot's timers, remove the halt file, and watch its first run.

Orders the new runtime placed are in the journal it hands back, with the same client
ids, so the old bot's reconcile keeps tracking them.
