"""The five strategy rules, each pinned by the case that would break it.

These are the tests that matter. Everything else in rungbot is plumbing around them.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from rungbot.analyze import analyze  # noqa: E402
from rungbot.config import from_mapping  # noqa: E402

fails = []
T0 = 1_700_000_000.0
DAY = 86400.0


def eq(got, want, what):
    if got != want:
        fails.append(f"{what}: got {got!r}, want {want!r}")


def ok(cond, what):
    if not cond:
        fails.append(what)


def cfg(entry=100.0, **ladder):
    return from_mapping({
        "bands": {"first_pct": 10, "step_pct": 5},
        "ladder": {"min_trade_pct": 1, "min_core_pct": 20, "window_hours": 24,
                   "buy_floor_pct": 50, "target_pct": 10, "breaker_pct": 40,
                   "breaker_days": 7, "trail": "off", **ladder},
        "coins": {"AAA": {"venue": "binance", "pair": "AAAUSDT", "entry": entry}},
    })


def px(price, chg):
    return {"AAA": {"price": price, "chg_24h": chg}}


# --- Rule 1: a rung fires once; a retrace does not re-fire it ----------------------
c = cfg()
buys, _, _, _, st = analyze(c, px(88.0, -12.0), {}, T0)
eq(len(buys), 1, "R1 first dip fires")
eq(buys[0]["rung"], 1, "R1 rung 1")
eq(buys[0]["buy_pct"], 10.0, "R1 buys first_pct")

buys2, _, _, _, st2 = analyze(c, px(88.5, -11.5), st, T0 + 3600)
eq(len(buys2), 0, "R1 the same rung does not fire twice")

buys3, _, _, _, st3 = analyze(c, px(84.0, -16.0), st2, T0 + 7200)
eq(len(buys3), 1, "R1 a deeper rung advances the ladder")
eq(buys3[0]["buy_pct"], 5.0, "R1 the deepening rung trades step_pct")
eq(st3["AAA"]["deployed_pct"], 15.0, "R1 deployed accumulates")

# --- Rule 2: the directional window locks out the opposite side -------------------
# A coin that just bought may not sell inside the window, even deep in profit.
_, _, _, _, bought = analyze(cfg(), px(88.0, -12.0), {}, T0)
eq(bought["AAA"]["win_dir"], "buy", "R2 a buy opens a buy window")
_, sells_locked, _, _, _ = analyze(cfg(), px(130.0, +2.0), bought, T0 + 60)
eq(len(sells_locked), 0, "R2 no sell while a buy window is open")

# Once the window has expired, the same profit does fire a sell.
_, sells_free, _, _, _ = analyze(cfg(), px(130.0, +2.0), bought, T0 + 25 * 3600)
eq(len(sells_free), 1, "R2 sell fires once the window closed")

# A sell-locked coin may not buy.
_, _, _, _, sold_state = analyze(cfg(), px(130.0, +2.0), {}, T0)
buys_locked, _, _, _, _ = analyze(cfg(), px(80.0, -20.0), sold_state, T0 + 60)
eq(len(buys_locked), 0, "R2 no buy while a sell window is open")

# --- Rule 3: the dynamic cap clamps cumulative buys --------------------------------
# A coin 60% underwater has a cap of 40% of base; it cannot deploy more than that.
c = cfg()
deep = {"AAA": {"buy": 0, "sell": 0, "deployed_pct": 35.0, "sold_pct": 0.0,
                "win_until": 0.0, "win_dir": "", "cost_basis": 100.0,
                "below_since": 0.0, "breaker": False, "peak_pnl": 0.0}}
b, _, _, _, s_after = analyze(c, px(40.0, -12.0), deep, T0)
eq(len(b), 1, "R3 a capped buy still fires")
eq(round(b[0]["buy_pct"], 6), 5.0, "R3 clamped to the remaining 5% of a 40% cap")
ok(b[0]["capped"], "R3 the capped flag is set")
eq(round(s_after["AAA"]["deployed_pct"], 6), 40.0, "R3 deployed stops at the cap")

# At the cap, nothing further fires at all.
at_cap = dict(deep); at_cap["AAA"] = {**deep["AAA"], "deployed_pct": 40.0}
b2, _, _, _, _ = analyze(c, px(40.0, -12.0), at_cap, T0)
eq(len(b2), 0, "R3 nothing fires once the cap is reached")

# --- Rule 4: the protected core is never sold ---------------------------------------
c = cfg()
mostly_sold = {"AAA": {"buy": 0, "sell": 0, "deployed_pct": 0.0, "sold_pct": 75.0,
                       "win_until": 0.0, "win_dir": "", "cost_basis": 100.0,
                       "below_since": 0.0, "breaker": False, "peak_pnl": 0.0}}
_, s1, _, _, st_core = analyze(c, px(130.0, +2.0), mostly_sold, T0)
eq(len(s1), 1, "R4 a sell still fires near the core")
eq(s1[0]["sell_pct"], 5.0, "R4 clamped to the last 5% above the core")
eq(st_core["AAA"]["sold_pct"], 80.0, "R4 sold stops at 100 - min_core_pct")

_, s2, _, _, _ = analyze(c, px(200.0, +2.0), st_core, T0 + 25 * 3600)
eq(len(s2), 0, "R4 nothing sells into the core")

# --- Rule 5: the circuit breaker freezes buys, never sells --------------------------
c = cfg()
# Day 0: 50% underwater -> breaker arms but has not tripped yet.
_, _, _, errs0, st_b0 = analyze(c, px(50.0, -2.0), {}, T0)
ok(st_b0["AAA"]["below_since"] == T0, "R5 breaker arms on the first deep day")
ok(not st_b0["AAA"]["breaker"], "R5 breaker has not tripped on day 0")
eq(len(errs0), 0, "R5 no alert before it trips")

# Day 8, still underwater and now dipping hard: buys are frozen and it says so once.
b_frozen, _, _, errs8, st_b8 = analyze(c, px(50.0, -20.0), st_b0, T0 + 8 * DAY)
ok(st_b8["AAA"]["breaker"], "R5 breaker tripped after breaker_days")
eq(len(b_frozen), 0, "R5 no dip-buy while the breaker holds")
ok(any("CIRCUIT BREAKER" in e for e in errs8), "R5 alerts when it trips")

# It alerts once, not every run.
_, _, _, errs9, st_b9 = analyze(c, px(50.0, -20.0), st_b8, T0 + 9 * DAY)
eq(len(errs9), 0, "R5 does not re-alert every run")

# Recovery clears it.
_, _, _, _, st_ok = analyze(c, px(95.0, -2.0), st_b9, T0 + 10 * DAY)
ok(not st_ok["AAA"]["breaker"], "R5 breaker clears on recovery")
eq(st_ok["AAA"]["below_since"], 0.0, "R5 the clock resets on recovery")

# --- the knife floor: past it, stop catching --------------------------------------
b_knife, _, _, _, _ = analyze(cfg(), px(40.0, -60.0), {}, T0)
eq(len(b_knife), 0, "past the knife floor, no buy")

# --- a coin with no cost basis is watched for dips only ----------------------------
nc = from_mapping({"bands": {"first_pct": 10, "step_pct": 5},
                   "coins": {"AAA": {"venue": "binance", "pair": "AAAUSDT"}}})
nb, ns, nrows, _, _ = analyze(nc, px(500.0, -12.0), {}, T0)
eq(len(nb), 1, "no-basis coin still dip-buys")
eq(len(ns), 0, "no-basis coin never sells")
eq(nrows[0]["pnl"], None, "no-basis coin reports no P&L")

# --- a missing price is an error, and leaves that ladder untouched ------------------
b, s, rows, errs, st_miss = analyze(cfg(), {}, {}, T0)
eq(len(rows), 0, "a coin with no price is not reported as a row")
eq(len(errs), 1, "a coin with no price is an error")
eq(st_miss, {}, "a missing price leaves state untouched")

# --- min_trade_pct suppresses meaningless dust --------------------------------------
c = cfg(min_trade_pct=8)
b_dust, _, _, _, _ = analyze(c, px(84.0, -16.0),
                             {"AAA": {"buy": 1, "sell": 0, "deployed_pct": 10.0,
                                      "sold_pct": 0.0, "win_until": T0 + DAY,
                                      "win_dir": "buy", "cost_basis": 100.0,
                                      "below_since": 0.0, "breaker": False,
                                      "peak_pnl": 0.0}}, T0)
eq(len(b_dust), 0, "a 5% step under an 8% minimum is not emitted")

# --- trailing take-profit holds upper rungs until the peak gives back ---------------
c = cfg(trail="on", trail_giveback_pct=5)
_, s_t1, _, _, st_t1 = analyze(c, px(112.0, +2.0), {}, T0)
eq(len(s_t1), 1, "trail: rung 1 still locks in immediately")
eq(st_t1["AAA"]["sell"], 1, "trail: high-water at rung 1")

# Running up to +30% (rung 5) while trailing: nothing fires, the peak is tracked.
_, s_t2, _, _, st_t2 = analyze(c, px(130.0, +2.0), st_t1, T0 + 3600)
eq(len(s_t2), 0, "trail: upper rungs are held while the move runs")
eq(round(st_t2["AAA"]["peak_pnl"], 6), 30.0, "trail: the peak is tracked")

# Give back 5% from the peak -> harvest up to the peak rung.
_, s_t3, _, _, _ = analyze(c, px(124.0, +2.0), st_t2, T0 + 7200)
eq(len(s_t3), 1, "trail: the give-back harvests the run")
eq(s_t3[0]["rung"], 5, "trail: harvests up to the peak rung")

print(f"test_analyze: {len(fails)} failure(s)")
for f in fails:
    print("  FAIL", f)
sys.exit(1 if fails else 0)
