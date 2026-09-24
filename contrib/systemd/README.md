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

The trading run reads `~/.config/rungbot/run.yaml`; start from
`../rungbot-run.example.yaml`, which lists every knob with its default. Try it with
`TRADE_MODE=dry` first:

```bash
cp rungbot-run.service rungbot-run.timer ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now rungbot-run.timer
```

`Persistent=true` catches up a run missed while the machine was off. Try a unit once
with `systemctl --user start rungbot-watch-btc.service` and read it back with
`journalctl --user -u rungbot-watch-btc`.
