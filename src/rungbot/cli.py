"""`rungbot` command line. Text by default, `--json` for machines.

Commands:
  init      write a starter config you can edit
  plan      price the watchlist, run the ladder, print what it would do
  tickers   just the prices, to check a venue/pair before adding a coin
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import sys
from pathlib import Path

from . import __version__, config as cfgmod, report, state as statemod, tickers
from .analyze import analyze

EXAMPLE = """\
# rungbot — a dip-buy / take-profit ladder, notify-only.
#
# `pair` is the venue's own symbol: Binance BTCUSDT, Gate BTC_USDT, CoinGecko bitcoin.
# `entry` is your cost basis. Leave it out and the coin is watched for dips only —
# rungbot will not suggest selling something it has no basis for.

bands:
  first_pct: 10        # first rung fires at a 10% move
  step_pct: 5          # every further rung, 5% beyond the last

ladder:
  min_trade_pct: 1     # don't bother with a trade smaller than this
  min_core_pct: 20     # never sell below this much of the position
  window_hours: 24     # after acting, a coin is locked to that direction this long
  buy_floor_pct: 50    # stop dip-buying past a 50% daily drop (falling knife)
  target_pct: 10       # take-profit target shown in the report
  breaker_pct: 40      # freeze dip-buys on a coin 40% underwater...
  breaker_days: 7      # ...for 7 straight days
  trail: off           # off = fire each rung on the way up; on = trail the peak

coins:
  BTC:
    name: Bitcoin
    venue: binance
    pair: BTCUSDT
    entry: 61000
  ETH:
    name: Ethereum
    venue: binance
    pair: ETHUSDT
    entry: 2400
  SOL:
    name: Solana
    venue: gate
    pair: SOL_USDT
"""


def _default_config() -> Path:
    import os
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base).expanduser() / "rungbot" / "watchlist.yaml"


def cmd_init(args) -> int:
    path = Path(args.config) if args.config else _default_config()
    if path.exists() and not args.force:
        print(f"refusing to overwrite {path} (use --force)", file=sys.stderr)
        return 1
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(EXAMPLE, encoding="utf-8")
    print(f"wrote {path}\nEdit it, then run: rungbot plan --config {path}")
    return 0


def _load(args):
    path = Path(args.config) if args.config else _default_config()
    if not path.exists():
        raise cfgmod.ConfigError(
            f"no config at {path}. Run `rungbot init` to write a starter one."
        )
    return cfgmod.load(path)


def cmd_tickers(args) -> int:
    cfg = _load(args)
    prices = tickers.fetch(cfg.coins)
    if args.json:
        print(json.dumps(prices, indent=2))
        return 0
    for coin in cfg.coins:
        p = prices.get(coin.symbol)
        if not p:
            print(f"{coin.symbol:<7} {'-':>14}  (no price from {coin.venue}:{coin.pair})")
        else:
            print(f"{coin.symbol:<7} {report.fmt_price(p['price']):>14}"
                  f" {p['chg_24h']:+8.2f}%  {coin.venue}:{coin.pair}")
    return 0


def cmd_plan(args) -> int:
    cfg = _load(args)
    spath = Path(args.state) if args.state else statemod.default_path()

    if args.prices:
        raw = json.loads(Path(args.prices).read_text(encoding="utf-8"))
        prices = {k: {"price": float(v["price"]),
                      "chg_24h": None if v.get("chg_24h") is None else float(v["chg_24h"])}
                  for k, v in raw.items()}
    else:
        prices = tickers.fetch(cfg.coins)

    now = float(args.now) if args.now is not None else dt.datetime.now(dt.timezone.utc).timestamp()
    prior = {} if args.fresh else statemod.load(spath)
    buys, sells, rows, errors, new_state = analyze(cfg, prices, prior, now)

    now_iso = dt.datetime.fromtimestamp(now, dt.timezone.utc).isoformat(timespec="seconds")
    if args.json:
        print(json.dumps({"generated": now_iso, "buys": buys, "sells": sells,
                          "rows": rows, "errors": errors}, indent=2, sort_keys=True))
    else:
        print(report.render(buys, sells, rows, errors, cfg, now_iso))

    if args.save:
        statemod.save(spath, new_state)
    elif not args.json:
        print(f"\n(state not saved; pass --save to advance the ladder at {spath})")
    return 0


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="rungbot",
        description="A dip-buy / take-profit ladder for spot crypto. Notify-only: "
                    "rungbot holds no API keys and cannot place an order.",
    )
    p.add_argument("--version", action="version", version=f"rungbot {__version__}")
    sub = p.add_subparsers(dest="cmd", required=True)

    pi = sub.add_parser("init", help="write a starter config")
    pi.add_argument("--config", help="where to write it")
    pi.add_argument("--force", action="store_true", help="overwrite an existing file")
    pi.set_defaults(func=cmd_init)

    pp = sub.add_parser("plan", help="price the watchlist and print the ladder")
    pp.add_argument("--config", help="path to watchlist.yaml")
    pp.add_argument("--state", help="path to the ladder state file")
    pp.add_argument("--save", action="store_true",
                    help="persist the advanced ladder (default: dry, changes nothing)")
    pp.add_argument("--fresh", action="store_true", help="ignore prior state")
    pp.add_argument("--prices", help="read prices from a JSON file instead of the network")
    pp.add_argument("--now", type=float, help="epoch seconds, for reproducible runs")
    pp.add_argument("--json", action="store_true", help="machine-readable output")
    pp.set_defaults(func=cmd_plan)

    pt = sub.add_parser("tickers", help="just the prices")
    pt.add_argument("--config", help="path to watchlist.yaml")
    pt.add_argument("--json", action="store_true")
    pt.set_defaults(func=cmd_tickers)
    return p


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except cfgmod.ConfigError as e:
        print(f"config error: {e}", file=sys.stderr)
        return 2
    except tickers.TickerError as e:
        print(f"price feed error: {e}", file=sys.stderr)
        return 3
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
