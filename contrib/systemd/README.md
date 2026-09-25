# systemd user units

One oneshot service and one timer per watcher. They run as your user and read
`~/.config/rungbot/watchlist.yaml`; credentials go in `~/.config/rungbot/env`.

```bash
mkdir -p ~/.config/systemd/user
cp rungbot-watch-*.service rungbot-watch-*.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now rungbot-watch-regime.timer rungbot-watch-btc.timer \
    rungbot-watch-daily.timer rungbot-watch-divergence.timer
```

| Timer | Runs |
|---|---|
| `regime`, `btc` | every 30 minutes |
| `divergence` | daily, 05:00 UTC |
| `daily` (froth, then zone) | daily, 05:20 UTC |
| `rungbot-run` (`rungbot-exec run`) | every 30 minutes |
| `rungbot-snapshot` (`rungbot-exec snapshot`) | every 5 minutes, at :02, :07, ... |
| `rungbot-research-report` (`rungbot research report`) | Sundays, 07:00 |
| `rungbot-backtest-monthly` (`rungbot backtest monthly --notify`) | the 20th, 04:00 UTC |

The trading run already checks the regime label and the BTC level every cycle and
mails on its own; `rungbot-watch-regime` and `rungbot-watch-btc` are for a setup that
runs the keyless ladder alone. Enable one or the other, not both, or the alerts arrive
twice.

## One unit per job of a Python bot

Moving from the Python bot these units replace (see `docs/migrate-from-python.md`),
each of its timers has one counterpart here, on the same schedule:

| Old job | Unit here |
|---|---|
| the 30-minute alert-and-trade run | `rungbot-run` |
| the daily froth watch, then the zone watch | `rungbot-watch-daily` |
| the daily live-vs-backtest divergence check | `rungbot-watch-divergence` |
| the weekly opportunity-scan report | `rungbot-research-report` |
| the monthly calibration backtest and its mail | `rungbot-backtest-monthly` |
| the dashboard collector and deploy | `rungbot-snapshot` |

The trading run reads `~/.config/rungbot/run.yaml`; start from
`../rungbot-run.example.yaml`, which lists every knob with its default. Try it with
`TRADE_MODE=dry` first:

```bash
cp rungbot-run.service rungbot-run.timer ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now rungbot-run.timer
```

The dashboard collector reads the same `run.yaml` (its `dashboard:` block). It never
trades; with `DASHBOARD_DEPLOY=1` it runs `dashboard.deploy_command` after each refresh:

```bash
cp rungbot-snapshot.service rungbot-snapshot.timer ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now rungbot-snapshot.timer
```

`Persistent=true` catches up a run missed while the machine was off. Try a unit once
with `systemctl --user start rungbot-watch-btc.service` and read it back with
`journalctl --user -u rungbot-watch-btc`.
