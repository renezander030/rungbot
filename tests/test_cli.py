"""The CLI end to end, offline. `plan` must work with no network and no keys.

RUNGBOT_OFFLINE=1 makes every ticker call raise, so if any code path here reached the
network the test would fail loudly instead of quietly hitting an exchange.
"""

import io
import json
import os
import sys
import tempfile
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

os.environ["RUNGBOT_OFFLINE"] = "1"

from rungbot import tickers  # noqa: E402
from rungbot.cli import main  # noqa: E402

fails = []


def eq(got, want, what):
    if got != want:
        fails.append(f"{what}: got {got!r}, want {want!r}")


def ok(cond, what):
    if not cond:
        fails.append(what)


def run(argv):
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        code = main(argv)
    return code, out.getvalue(), err.getvalue()


tmp = Path(tempfile.mkdtemp(prefix="rungbot-test-"))
cfg_path = tmp / "watchlist.yaml"
state_path = tmp / "state.json"
prices_path = tmp / "prices.json"

# --- init writes a config that loads --------------------------------------------
code, out, err = run(["init", "--config", str(cfg_path)])
eq(code, 0, "init succeeds")
ok(cfg_path.exists(), "init wrote the file")
ok("Edit it" in out, "init tells you what to do next")

code, _, err = run(["init", "--config", str(cfg_path)])
eq(code, 1, "init refuses to clobber")
ok("--force" in err, "init says how to override")
eq(run(["init", "--config", str(cfg_path), "--force"])[0], 0, "init --force overwrites")

# --- plan with injected prices needs no network ----------------------------------
prices_path.write_text(json.dumps({
    "BTC": {"price": 54900.0, "chg_24h": -12.0},   # -12% -> dip rung 1
    "ETH": {"price": 2880.0, "chg_24h": 3.0},      # +20% vs the example's 2400 entry
    "SOL": {"price": 140.0, "chg_24h": -2.0},      # no entry in the example config
}), encoding="utf-8")

code, out, err = run(["plan", "--config", str(cfg_path), "--state", str(state_path),
                      "--prices", str(prices_path), "--now", "1700000000"])
eq(code, 0, f"plan succeeds offline (stderr: {err[:200]})")
ok("BUY" in out and "SELL" in out, "plan prints both sides")
ok("BTC" in out and "rung 1" in out, "plan reports the BTC dip rung")
ok("notify-only" in out.lower(), "plan states it is notify-only")
ok("state not saved" in out, "plan says the state was not advanced")
ok(not state_path.exists(), "plan without --save writes nothing")

# --- --json is machine-readable and stable ---------------------------------------
code, out, _ = run(["plan", "--config", str(cfg_path), "--state", str(state_path),
                    "--prices", str(prices_path), "--now", "1700000000", "--json"])
eq(code, 0, "plan --json succeeds")
data = json.loads(out)
eq(sorted(data.keys()), ["buys", "errors", "generated", "rows", "sells"], "json shape")
eq([b["sym"] for b in data["buys"]], ["BTC"], "json lists the BTC buy")
eq([s["sym"] for s in data["sells"]], ["ETH"], "json lists the ETH sell")
eq(data["buys"][0]["rung"], 1, "json carries the rung")

# --- --save advances the ladder, and the rung then fires only once ----------------
code, _, _ = run(["plan", "--config", str(cfg_path), "--state", str(state_path),
                  "--prices", str(prices_path), "--now", "1700000000", "--save"])
eq(code, 0, "plan --save succeeds")
ok(state_path.exists(), "plan --save wrote state")
saved = json.loads(state_path.read_text())
eq(saved["version"], 1, "state carries a version")
eq(saved["coins"]["BTC"]["buy"], 1, "state records the high-water rung")

code, out, _ = run(["plan", "--config", str(cfg_path), "--state", str(state_path),
                    "--prices", str(prices_path), "--now", "1700000600", "--json"])
eq(json.loads(out)["buys"], [], "the same rung does not fire again after --save")

code, out, _ = run(["plan", "--config", str(cfg_path), "--state", str(state_path),
                    "--prices", str(prices_path), "--now", "1700000600",
                    "--fresh", "--json"])
eq([b["sym"] for b in json.loads(out)["buys"]], ["BTC"], "--fresh ignores prior state")

# --- errors are exit codes, not tracebacks ---------------------------------------
code, _, err = run(["plan", "--config", str(tmp / "nope.yaml")])
eq(code, 2, "a missing config exits 2")
ok("rungbot init" in err, "a missing config points at `init`")

(tmp / "bad.yaml").write_text("coins:\n  BTC:\n    venue: kraken\n    pair: X\n")
code, _, err = run(["plan", "--config", str(tmp / "bad.yaml")])
eq(code, 2, "a bad config exits 2")
ok("venue must be one of" in err, "a bad config explains itself")

# --- the offline guard really is armed --------------------------------------------
code, _, err = run(["tickers", "--config", str(cfg_path)])
eq(code, 3, "a blocked price feed exits 3")
ok("RUNGBOT_OFFLINE" in err, "the offline guard is what blocked it")

# --- there is no signing or key handling anywhere in the package -------------------
pkg = Path(__file__).resolve().parents[1] / "src" / "rungbot"
banned = ("hmac", "API_SECRET", "api_secret", "private_key", "X-MBX-APIKEY", "signature")
for f in sorted(pkg.glob("*.py")):
    body = f.read_text(encoding="utf-8")
    for token in banned:
        if token in body:
            fails.append(f"{f.name} mentions {token!r}: this package must hold no keys")
ok(tickers.USER_AGENT.startswith("rungbot/"), "the ticker feed identifies itself")

print(f"test_cli: {len(fails)} failure(s)")
for f in fails:
    print("  FAIL", f)
sys.exit(1 if fails else 0)
