"""Rung arithmetic. Every boundary is a number someone's money depends on."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from rungbot.ladder import (Bands, buy_rung_for, ladder_increment,  # noqa: E402
                            rung_threshold, sell_rung_for)

B = Bands(first_pct=10, step_pct=5)
fails = []


def eq(got, want, what):
    if got != want:
        fails.append(f"{what}: got {got!r}, want {want!r}")


# --- buy side: dips are negative, rungs are 1-based -------------------------------
eq(buy_rung_for(None, B), None, "missing 24h data holds the ladder")
eq(buy_rung_for(0.0, B), 0, "flat is neutral")
eq(buy_rung_for(+25.0, B), 0, "a pump is not a dip rung")
eq(buy_rung_for(-9.99, B), 0, "just inside the first band is still neutral")
eq(buy_rung_for(-10.0, B), 1, "exactly at first_pct fires rung 1")
eq(buy_rung_for(-14.9, B), 1, "still rung 1 below the next step")
eq(buy_rung_for(-15.0, B), 2, "exactly at the step fires rung 2")
eq(buy_rung_for(-30.0, B), 5, "deep dip counts every step")

# --- sell side: profit is positive, never fires at a loss -------------------------
eq(sell_rung_for(None, B), None, "no basis means no sell judgement")
eq(sell_rung_for(-50.0, B), 0, "never a sell rung at a loss")
eq(sell_rung_for(0.0, B), 0, "break-even is not a sell")
eq(sell_rung_for(9.99, B), 0, "just under target is not a sell")
eq(sell_rung_for(10.0, B), 1, "exactly at target fires rung 1")
eq(sell_rung_for(20.0, B), 3, "+20% is rung 3")

# --- thresholds mirror the rungs --------------------------------------------------
eq(rung_threshold(1, B), 10, "rung 1 threshold")
eq(rung_threshold(2, B), 15, "rung 2 threshold")
eq(rung_threshold(5, B), 30, "rung 5 threshold")
for r in range(1, 8):
    eq(buy_rung_for(-rung_threshold(r, B), B), r,
       f"threshold of rung {r} fires exactly rung {r}")

# --- increments: the sum of a multi-rung jump equals the size of the move ---------
eq(ladder_increment([1], B), 10, "a fresh rung 1 trades first_pct")
eq(ladder_increment([2], B), 5, "a deepening rung trades step_pct")
eq(ladder_increment([1, 2, 3], B), 20, "a 3-rung jump trades 10+5+5")
eq(ladder_increment([], B), 0, "no new rungs, no trade")

# --- per-coin bands are honoured --------------------------------------------------
W = Bands(first_pct=15, step_pct=8)
eq(buy_rung_for(-14.9, W), 0, "wide bands: -14.9% is still neutral")
eq(buy_rung_for(-15.0, W), 1, "wide bands: rung 1 at -15%")
eq(buy_rung_for(-23.0, W), 2, "wide bands: rung 2 at -23%")
eq(ladder_increment([1, 2], W), 23, "wide bands increment 15+8")

# --- bands validate themselves ----------------------------------------------------
for bad in ({"first_pct": 0}, {"step_pct": 0}, {"first_pct": -5}):
    try:
        Bands(**bad)
        fails.append(f"Bands({bad}) should have raised")
    except ValueError:
        pass

print(f"test_ladder: {len(fails)} failure(s)")
for f in fails:
    print("  FAIL", f)
sys.exit(1 if fails else 0)
