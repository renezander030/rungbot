"""Config loading, the built-in YAML reader, and the errors a human has to read.

A bad config must fail with a message that says what to fix. Silently falling back to a
default would change the strategy without telling anyone.
"""

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from rungbot import config as cfgmod  # noqa: E402
from rungbot.config import ConfigError, from_mapping  # noqa: E402

fails = []


def eq(got, want, what):
    if got != want:
        fails.append(f"{what}: got {got!r}, want {want!r}")


def raises(fn, needle, what):
    try:
        fn()
    except ConfigError as e:
        if needle.lower() not in str(e).lower():
            fails.append(f"{what}: message {str(e)!r} lacks {needle!r}")
        return
    except Exception as e:  # noqa: BLE001
        fails.append(f"{what}: raised {type(e).__name__} not ConfigError: {e}")
        return
    fails.append(f"{what}: did not raise")


# --- the built-in YAML reader handles the format the CLI writes --------------------
SAMPLE = """\
# a comment
bands:
  first_pct: 12
  step_pct: 6

ladder:
  min_core_pct: 25
  trail: off

coins:
  BTC:
    name: Bitcoin       # trailing comment
    venue: binance
    pair: BTCUSDT
    entry: 61000
  SOL:
    venue: gate
    pair: SOL_USDT
    bands:
      first_pct: 20
      step_pct: 10
"""
parsed = cfgmod._mini_yaml(SAMPLE)
eq(parsed["bands"]["first_pct"], 12, "mini-yaml reads a nested int")
# YAML 1.1: a bare `off` is the boolean False. Our reader matches PyYAML rather than
# inventing its own dialect, and _trail() turns it back into "off" downstream.
eq(parsed["ladder"]["trail"], False, "mini-yaml reads `off` as False, exactly like PyYAML")
eq(parsed["coins"]["BTC"]["name"], "Bitcoin", "mini-yaml strips a trailing comment")
eq(parsed["coins"]["SOL"]["bands"]["step_pct"], 10, "mini-yaml reads three levels deep")

c = from_mapping(parsed)
eq(len(c.coins), 2, "two coins loaded")
eq(c.settings.bands.first_pct, 12.0, "global bands applied")
eq(c.settings.min_core_pct, 25.0, "ladder override applied")
eq(c.settings.trail, "off", "a YAML-1.1 boolean `off` normalises back to the string")
eq(c.settings.window_hours, 24.0, "unspecified settings keep their default")
eq(c.coin("BTC").bands, None, "a coin without an override inherits")
eq(c.coin("BTC").bands_or(c.settings.bands).first_pct, 12.0, "inherit resolves to global")
eq(c.coin("SOL").bands_or(c.settings.bands).first_pct, 20.0, "a per-coin override wins")
eq(c.coin("SOL").entry, None, "a coin may have no cost basis")

# PyYAML, when present, must produce the same Config.
try:
    import yaml  # noqa: F401
    eq(from_mapping(cfgmod._load_yaml(SAMPLE)).coins, c.coins,
       "PyYAML and the built-in reader agree")
except ImportError:
    pass

# --- lists are refused with a pointer to the fix ------------------------------------
raises(lambda: cfgmod._mini_yaml("coins:\n  - BTC\n"), "pyyaml", "a list says install pyyaml")

# --- validation errors name the coin and the field ----------------------------------
base = {"coins": {"BTC": {"venue": "binance", "pair": "BTCUSDT"}}}
raises(lambda: from_mapping({"coins": {}}), "at least one coin", "empty coins")
raises(lambda: from_mapping({}), "at least one coin", "no coins key")
raises(lambda: from_mapping({"coins": {"BTC": {"venue": "kraken", "pair": "X"}}}),
       "venue must be one of", "unknown venue")
raises(lambda: from_mapping({"coins": {"BTC": {"venue": "binance"}}}),
       "`pair` is required", "missing pair")
raises(lambda: from_mapping({"coins": {"BTC": {"venue": "binance", "pair": "X",
                                               "entry": "soon"}}}),
       "must be a number", "non-numeric entry")
raises(lambda: from_mapping({"coins": {"BTC": {"venue": "binance", "pair": "X",
                                               "entry": -1}}}),
       "must be > 0", "negative entry")
raises(lambda: from_mapping({**base, "ladder": {"trail": "maybe"}}),
       "must be off or on", "bad trail")
raises(lambda: from_mapping({**base, "ladder": {"min_core_pct": 100}}),
       "must be >= 0 and < 100", "core of 100% would sell nothing ever")
raises(lambda: from_mapping({**base, "bands": {"first_pct": 0}}),
       "first_pct", "zero bands")

# --- env overrides apply, and a typo is a hard error --------------------------------
os.environ["RUNGBOT_FIRST_PCT"] = "7"
os.environ["RUNGBOT_MIN_CORE_PCT"] = "33"
c2 = from_mapping(base)
eq(c2.settings.bands.first_pct, 7.0, "env overrides the band")
eq(c2.settings.min_core_pct, 33.0, "env overrides a setting")
os.environ["RUNGBOT_FIRST_PCT"] = "ten"
raises(lambda: from_mapping(base), "RUNGBOT_FIRST_PCT", "an unparseable env var is fatal")
del os.environ["RUNGBOT_FIRST_PCT"], os.environ["RUNGBOT_MIN_CORE_PCT"]

# --- symbols are normalised ---------------------------------------------------------
eq(from_mapping({"coins": {"btc": {"venue": "binance", "pair": "BTCUSDT"}}}).coins[0].symbol,
   "BTC", "a lower-case symbol is upper-cased")

print(f"test_config: {len(fails)} failure(s)")
for f in fails:
    print("  FAIL", f)
sys.exit(1 if fails else 0)
