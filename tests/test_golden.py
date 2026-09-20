"""Frozen end-to-end scenarios: the ladder's behaviour pinned byte-for-byte.

The unit tests say each rule is right. This says the *whole* thing still behaves exactly
as it did — so a refactor that quietly changes sizing, ordering or a boundary is caught
even when every unit test still passes.

Regenerate deliberately, never casually:  python3 tests/test_golden.py --update
Then read the diff. If you cannot explain every changed line, do not commit it.
"""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from rungbot.analyze import analyze  # noqa: E402
from rungbot.config import from_mapping  # noqa: E402

GOLDEN = Path(__file__).resolve().parent / "golden"
UPDATE = "--update" in sys.argv
T0 = 1_700_000_000.0
HOUR = 3600.0

CONFIG = {
    "bands": {"first_pct": 10, "step_pct": 5},
    "ladder": {"min_trade_pct": 1, "min_core_pct": 20, "window_hours": 24,
               "buy_floor_pct": 50, "target_pct": 10, "breaker_pct": 40,
               "breaker_days": 7, "trail": "off"},
    "coins": {
        # A winner, a loser, a coin on wide bands, and one with no cost basis.
        "AAA": {"name": "Alpha", "venue": "binance", "pair": "AAAUSDT", "entry": 100.0},
        "BBB": {"name": "Beta", "venue": "gate", "pair": "BBB_USDT", "entry": 2.0},
        "CCC": {"name": "Gamma", "venue": "binance", "pair": "CCCUSDT", "entry": 0.5,
                "bands": {"first_pct": 15, "step_pct": 8}},
        "DDD": {"name": "Delta", "venue": "coingecko", "pair": "delta"},
    },
}

# (hours since T0, {symbol: (price, 24h change)}) — a month of market in ten steps.
SCENARIO = [
    (0,    {"AAA": (100.0, 0.5),  "BBB": (2.00, -1.0),  "CCC": (0.50, 2.0),  "DDD": (10.0, 0.0)}),
    (1,    {"AAA": (88.0, -12.0), "BBB": (1.90, -5.0),  "CCC": (0.46, -8.0), "DDD": (8.5, -15.0)}),
    (2,    {"AAA": (84.0, -16.0), "BBB": (1.80, -10.0), "CCC": (0.41, -18.0), "DDD": (8.0, -20.0)}),
    (26,   {"AAA": (95.0, 5.0),   "BBB": (1.85, 2.0),   "CCC": (0.44, 3.0),  "DDD": (9.0, 6.0)}),
    (50,   {"AAA": (115.0, 8.0),  "BBB": (1.50, -12.0), "CCC": (0.60, 20.0), "DDD": (12.0, 15.0)}),
    (74,   {"AAA": (135.0, 6.0),  "BBB": (1.10, -18.0), "CCC": (0.75, 12.0), "DDD": (11.0, -8.0)}),
    (98,   {"AAA": (160.0, 4.0),  "BBB": (0.95, -9.0),  "CCC": (0.90, 10.0), "DDD": (14.0, 12.0)}),
    (24 * 9,  {"AAA": (150.0, -2.0), "BBB": (0.92, -1.0), "CCC": (0.85, -3.0), "DDD": (13.0, -4.0)}),
    (24 * 12, {"AAA": (145.0, -1.0), "BBB": (0.80, -14.0), "CCC": (0.80, -2.0), "DDD": (9.0, -30.0)}),
    (24 * 30, {"AAA": (250.0, 9.0), "BBB": (0.75, -3.0),  "CCC": (1.40, 25.0), "DDD": (6.0, -55.0)}),
]


def _round(o):
    """Floats to 8 decimals so a platform's last-bit noise cannot fail the run."""
    if isinstance(o, float):
        return round(o, 8)
    if isinstance(o, dict):
        return {k: _round(v) for k, v in o.items()}
    if isinstance(o, list):
        return [_round(v) for v in o]
    return o


def run_scenario():
    cfg = from_mapping(CONFIG)
    state, steps = {}, []
    for hours, prices in SCENARIO:
        now = T0 + hours * HOUR
        px = {s: {"price": p, "chg_24h": c} for s, (p, c) in prices.items()}
        buys, sells, rows, errors, state = analyze(cfg, px, state, now)
        steps.append({
            "hours": hours,
            "buys": [{"sym": b["sym"], "rung": b["rung"], "buy_pct": b["buy_pct"],
                      "capped": b["capped"]} for b in buys],
            "sells": [{"sym": s["sym"], "rung": s["rung"], "sell_pct": s["sell_pct"],
                       "capped": s["capped"]} for s in sells],
            "errors": errors,
            "state": state,
        })
    return _round({"scenario": steps})


def main() -> int:
    GOLDEN.mkdir(parents=True, exist_ok=True)
    path = GOLDEN / "ladder_scenario.json"
    got = json.dumps(run_scenario(), indent=2, sort_keys=True)

    if UPDATE:
        path.write_text(got + "\n", encoding="utf-8")
        print(f"test_golden: wrote {path}")
        return 0

    if not path.exists():
        print(f"test_golden: FAIL no golden file at {path};"
              f" run `python3 {Path(__file__).name} --update` once and review it")
        return 1

    want = path.read_text(encoding="utf-8").strip()
    if got.strip() == want:
        print("test_golden: ladder scenario matches the frozen golden output")
        return 0

    import difflib
    diff = list(difflib.unified_diff(want.splitlines(), got.splitlines(),
                                     "golden", "current", lineterm="", n=2))
    print(f"test_golden: FAIL behaviour changed ({len(diff)} diff lines)")
    for line in diff[:60]:
        print("  " + line)
    if len(diff) > 60:
        print(f"  ... {len(diff) - 60} more lines")
    return 1


if __name__ == "__main__":
    sys.exit(main())
