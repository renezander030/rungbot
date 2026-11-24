#!/usr/bin/env python3
"""Synthetic test fixtures: every input below is drawn from one seeded generator, with neutral coins
(AAA, BBB, CCC, DDD, VVV and BTC on the bot; ZZZ and WWW only in wallets), example addresses and dates in 2030.

    python3 tools/synthetic_fixtures.py            # rewrite the fixtures
    python3 tools/synthetic_fixtures.py --check    # exit 1 when a committed fixture differs

The expected outputs of the dashboard books are then written by the current implementation:

    RUNGBOT_BLESS=1 cargo test -p rungbot-exec --test dashboard_golden -- --test-threads=1
    RUNGBOT_BLESS=1 cargo test -p rungbot-backtest --test golden_backtests
    python3 tools/synthetic_fixtures.py --sync-mail
    RUNGBOT_BLESS=1 cargo test -p rungbot the_monthly_mail

Standard library only; the same seed always gives the same bytes.
"""
import datetime as dt
import json
import math
import random
import sys
from pathlib import Path

SEED = 20300101
ROOT = Path(__file__).resolve().parents[1]
GOLD = ROOT / "crates/rungbot-exec/tests/golden"
DASH = GOLD / "dashboard"

T0 = dt.datetime(2030, 3, 1, 12, 0, tzinfo=dt.timezone.utc)
NOW = T0.timestamp() + 0.25
ALTS = ["AAA", "BBB", "CCC", "DDD", "VVV"]
COINS = ["BTC"] + ALTS


def rng(name):
    """one stream per fixture, so changing one never moves another"""
    return random.Random(f"{SEED}:{name}")


def r2(x, n=2):
    return round(x, n)


def addr(r, prefix, n):
    alphabet = "023456789acdefghjklmnpqrstuvwxyz"
    return prefix + "".join(r.choice(alphabet) for _ in range(n))


def hexaddr(r):
    """an example EVM address in the dummy form (no more than six distinct hex digits)"""
    digits = r.sample("0123456789abcdef", 6)
    return "0x" + "".join(r.choice(digits) for _ in range(40))


def dumps(v, indent=None):
    # the reference's json.dumps: ", " and ": " separators, keys in insertion order
    return json.dumps(v, indent=indent, ensure_ascii=False)


# ------------------------------------------------------------------ cex-dir-state


def cex_dir_state():
    r = rng("cex-dir-state")
    ts = T0.timestamp() - 86400 * 3
    cid = f"csAAAs{r.randint(100000, 999999)}r1"
    journal = {cid: {"client_id": cid, "sym": "AAA", "exch": "gate", "pair": "AAA_USDT", "side": "sell",
                     "kind": "limit_sell", "status": "open", "base": r2(r.uniform(20, 90), 1),
                     "price": r2(r.uniform(1, 3), 2), "rung": 1, "ts": ts + 0.5, "order_id": str(r.randint(10, 99))}}
    ladder = {
        "AAA": {"buy": 2, "sell": 1, "deployed_pct": r2(r.uniform(20, 80), 1), "sold_pct": r2(r.uniform(1, 9), 1),
                "win_until": ts + 86400 * 7 + 0.5, "win_dir": "buy", "cost_basis": r2(r.uniform(1, 2), 1),
                "below_since": 0, "breaker": False, "peak_pnl": r2(r.uniform(1, 9), 1),
                "bull": {"peak": r2(r.uniform(2, 4), 1), "remaining": r2(r.uniform(100, 900), 1), "tranches": [],
                         "exited": False}},
        "_daily": {"date": T0.strftime("%Y-%m-%d"), "notional": r2(r.uniform(10, 90), 1), "orders": 2},
        "_weekly": {"week": T0.strftime("%G-W%V"), "notional": r2(r.uniform(10, 90), 1)},
        "_bal": {"ts": ts + 0.5, "hold": {"AAA": r2(r.uniform(10, 90), 1)}, "booked": {cid: r2(r.uniform(10, 90), 1)}},
        "_sellpolicy": {"on": False, "armed": False, "ts": ts + 0.5},
    }

    def iso(t):
        return dt.datetime.fromtimestamp(t, dt.timezone.utc).isoformat(timespec="microseconds")

    qty, quote, avg = r2(r.uniform(10, 90), 1), r2(r.uniform(2, 9), 1), r2(r.uniform(1, 3), 1)
    pnl = [
        {"ts": ts + 3600.5, "iso": iso(ts + 3600.5), "sym": "AAA", "kind": "limit_sell", "qty": qty, "quote": quote,
         "avg_price": avg, "cost_basis": r2(avg * 0.8, 1), "realized_usd": r2(quote * 0.2, 1),
         "realized_pct": (avg / r2(avg * 0.8, 1) - 1) * 100},
        {"ts": ts + 7200.5, "iso": iso(ts + 7200.5), "sym": "AAA", "kind": "market_sell", "qty": r2(r.uniform(1, 9), 1),
         "quote": r2(r.uniform(1, 9), 1), "avg_price": r2(r.uniform(1, 3), 1), "cost_basis": None,
         "realized_usd": None, "realized_pct": None},
    ]
    ttl = {cid: ts + 0.5}
    d = GOLD / "cex-dir-state"
    return {d / "orders-journal.json": dumps(journal, 2), d / "ladder-state.live.json": dumps(ladder, 2),
            d / "pnl-ledger.json": dumps(pnl, 2), d / "ttl-warned.json": dumps(ttl, 2)}


# ------------------------------------------------------------------ backtest book


def backtest_book():
    """the start book the backtests replay: coin holdings, stables per venue, venue minimums"""
    r = rng("backtest-book")
    book = {"held": {"AAA": r2(r.uniform(0.02, 0.2), 3), "BBB": r2(r.uniform(200, 900), 1),
                     "CCC": r2(r.uniform(200, 900), 1)},
            "stable": {"binance": r2(r.uniform(100, 600), 1), "gate": r2(r.uniform(100, 600), 1)},
            "min_notional": {"AAA": 5.0, "BBB": 5.0, "CCC": 1.0},
            "counts": {"binance": 2, "gate": 1}}
    return {ROOT / "crates/rungbot-backtest/tests/fixtures/book.json": json.dumps(book)}


def sync_monthly_mail():
    """the monthly mail cases carry the monthly report's subject and body: copy them from the blessed monthly
    golden (the request each one builds is then blessed by its own test)"""
    monthly = json.loads((ROOT / "crates/rungbot-backtest/tests/golden/monthly.json").read_text())
    p = ROOT / "crates/rungbot/tests/golden/monthly_mail.json"
    mail = json.loads(p.read_text())
    for case, m in zip(mail["cases"], monthly):
        assert case["name"] == m["name"]
        head, _, rest = m["stdout"].partition("\nSubject: ")
        subject, _, body = rest.partition("\n\n")
        assert head == "--- Would send ---" and body.endswith("\n")
        case["subject"], case["body"] = subject, body[:-1]
    p.write_text(json.dumps(mail, indent=1, ensure_ascii=False) + "\n")


# ------------------------------------------------------------------ price series


def ohlc_series(name, start, days, p0, vol, decimals):
    """a seeded daily OHLC random walk: [date, open, high, low, close]"""
    r = rng(name)
    out, p = [], p0
    for i in range(days):
        o = p
        c = max(o * math.exp(r.gauss(0, vol)), 10 ** -decimals)
        h = max(o, c) * (1 + abs(r.gauss(0, vol / 2)))
        lo = min(o, c) * (1 - abs(r.gauss(0, vol / 2)))
        day = (start + dt.timedelta(days=i)).isoformat()
        out.append([day, round(o, decimals), round(h, decimals), round(lo, decimals), round(c, decimals)])
        p = c
    return out


def gate_series():
    return {
        DASH / "replay/gate_CCC_USDT.json": dumps(ohlc_series("gate_CCC", dt.date(2021, 1, 10), 336, 0.42, 0.05, 7)),
        DASH / "replay/gate_VVV_USDT.json": dumps(ohlc_series("gate_VVV", dt.date(2019, 11, 1), 360, 3.1, 0.045, 6)),
    }


# ------------------------------------------------------------------ dashboard


def common():
    r = rng("common")
    src = {"BTC": [["binance_BTCUSDT.json", "ohlc"]], "AAA": [["binance_AAAUSDT.json", "ohlc"],
                                                            ["kucoin_AAA-USDT.json", "ohlc"]],
           "BBB": [["gate_BBB_USDT.json", "ohlc"], ["llama_bbb.json", "close"]], "CCC": [["gate_CCC_USDT.json", "ohlc"]],
           "DDD": [["gate_DDD_USDT.json", "ohlc"]], "VVV": [["binance_VVVUSDT.json", "ohlc"],
                                                         ["gate_VVV_USDT.json", "ohlc"]]}
    # windows inside the committed price series (the public candles run 2019-06 to 2020-06)
    scenarios = [["w_crash", "synthetic crash", "2020-03-01", 60, "a sharp synthetic drawdown"],
                 ["w_top", "synthetic top", "2020-01-15", None, "a synthetic cycle top"],
                 ["w_low", "synthetic low", "2019-12-01", 90, "a synthetic cycle low"],
                 ["w_chop", "synthetic chop", "2020-02-01", 45, "a synthetic sideways stretch"],
                 ["w_rally", "synthetic rally", "2020-04-01", 30, "a synthetic rally"]]
    ath = {c: [r2(r.uniform(2, 40), 2) if c != "BTC" else r2(r.uniform(90000, 120000), 1),
               f"2029-{r.randint(1, 12):02d}-{r.randint(1, 28):02d}"] for c in COINS}
    cg = {c: ("bitcoin" if c == "BTC" else f"{c.lower()}-coin") for c in COINS}
    cosmos = [[c, f"{c.lower()}chain", addr(r, c.lower() + "1", 38), 6, f"{c.lower()}-coin"]
              for c in ["AAA", "ZZZ", "CCC", "WWW"]]
    return {
        "src": src, "scenarios": scenarios, "ath": ath, "cg_ids": cg, "alts": ALTS, "evm": hexaddr(r),
        "btc_addr": addr(r, "bc1q", 38), "cosmos": cosmos, "eth_rpc": "https://rpc-one.example.invalid",
        "l2_rpc": "https://rpc-two.example.invalid", "tok": [hexaddr(r) for _ in range(4)],
        "trail_only": ["BBB", "ZZZ"], "no_tranche": "BTC,BBB,DDD",
        "forecast_alloc": {c: r.choice([100, 150, 200, 250, 300]) for c in ALTS},
    }


def eth_key(rpc, to, data):
    body = dumps({"jsonrpc": "2.0", "id": 1, "method": "eth_call",
                  "params": [{"to": to, "data": data}, "latest"]})
    return f"{rpc} {body}"


def eth_hex(x):
    return hex(int(round(x * 1e18)))


def book(name, com):
    r = rng("book-" + name)
    full, degraded, stale = name == "full", name == "degraded", name == "stale"
    now = NOW + {"full": 0, "degraded": 3600, "stale": 7200}[name]
    prices_all = {c: [r2(r.uniform(60000, 90000), 1) if c == "BTC" else r2(r.uniform(0.3, 9), 2),
                      r2(r.uniform(-12, 12), 1)] for c in COINS}
    inp = {
        "now": now,
        "log_mtime_ns": None if degraded else int((now - 95) * 1e9),
        "halt": degraded,
        "mode": "dry" if degraded else "live",
        "live_enabled": "no" if degraded else "yes",
        "target_pct": 60.0, "first_pct": 25.0,
        "watchlist": [[c, com["cg_ids"][c]] for c in COINS],
        "routing": [["BTC", "revx", "BTC-USD", "USD"], ["AAA", "gate", "AAA_USDT", "USDT"],
                    ["BBB", "revx", "BBB-USD", "USD"], ["CCC", "gate", "CCC_USDT", "USDT"],
                    ["DDD", "revx", "DDD-USD", "USD"], ["VVV", "binance", "VVVUSDT", "USDT"]],
        "entries": {c: r2(prices_all[c][0] * (r.uniform(0.18, 0.22) if c in ("AAA", "CCC", "VVV") else r.uniform(0.8, 1.1)),
                          1 if c == "BTC" else 2) for c in COINS},
        "alloc": {c: r.choice([200, 300, 400, 500]) for c in ALTS},
        "legacy_venues": {"VVV": "binance", "CCC": "gate"},
        "legacy_before_ts": int(now - 86400 * 40),
        "gate": ({"clients": True, "balances": None, "balances_error": "synthetic gate outage", "pairs": {}}
                 if degraded else
                 {"clients": True,
                  "balances": {"USDT": {"free": r2(r.uniform(10, 90), 1), "locked": r2(r.uniform(1, 9), 1)},
                               "AAA": {"free": r2(r.uniform(1, 9), 1), "locked": r2(r.uniform(1, 9), 1)}},
                  "pairs": {"AAA_USDT": [r2(r.uniform(10, 90), 1), 1.0, 2]}}),
        "revx_pairs": {"BTC-USD": [0.0001, 1.0, 8], "BBB-USD": [1.0, 1.0, 2], "DDD-USD": [1000.0, 1.0, 0]},
        "prices": ({c: prices_all[c] for c in ("BTC", "BBB")} if degraded else prices_all),
        "regime": None if degraded else {
            "epoch": int(now - 600), "market": "bull",
            "btc": {"px": prices_all["BTC"][0], "sma100": r2(prices_all["BTC"][0] * 0.92, 1),
                    "sma200": r2(prices_all["BTC"][0] * 0.85, 1)},
            "breadth_above_sma30": "3/5",
            "coins": {**{c: {"running": i % 2 == 0, "score": i % 4,
                             "signals": {"above_sma30": i % 2 == 0, "ret30_strong": i % 3 == 0,
                                         "fresh_30d_high": i % 4 == 0, "higher_lows": i % 2 == 1}}
                         for i, c in enumerate(["BTC", "AAA", "BBB", "CCC", "VVV"])},
                      "DDD": {"running": False, "error": "synthetic fetch failure while reading the candle history"}},
        },
        "history": {"epoch": int(now - 600), "labels": ["bull", "bull"], "held_days": 12, "prev_label": "flat",
                    "confirmed": not stale, "confirm_days": 10, "days_covered": 120},
        "ladder_state": {} if degraded else {
            "AAA": "not a dict", "BBB": "not a dict", "CCC": "not a dict",
            "_daily": {"date": dt.datetime.fromtimestamp(now, dt.timezone.utc).strftime("%Y-%m-%d"),
                       "notional": r2(r.uniform(100, 900), 1), "orders": 3}},
    }
    if degraded:
        inp["regime_error"] = "synthetic regime outage"
    # the journal: deploy buys in every status the zones filter tells apart, plus ladder sells
    t = now - 86400
    rows = []

    def row(cid, **kw):
        rows.append((cid, {"client_id": cid, **kw}))

    p = lambda c, k=0.9: r2(prices_all[c][0] * k, 2)  # noqa: E731
    row("dep1AAAr1x0", sym="AAA", side="buy", kind="deploy_buy", status="cancelled", rung=1, price=p("AAA", 0.95),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t - 7200.5,
        note="rolled into new tranche (synthetic)", exch="gate", canceled_ts=t - 60.5)
    row("dep2AAAr1x0", sym="AAA", side="buy", kind="deploy_buy", status="open", rung=1, price=p("AAA", 0.92),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 0.5, note="placed below the zone (synthetic)",
        exch="gate", canceled_ts=t + 60.5)
    row("dpAAAr2x0", sym="AAA", side="buy", kind="deploy_buy", status="new", rung=2, price=p("AAA", 0.85),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=int(t))
    row("dpBBBr1x0", sym="BBB", side="buy", kind="deploy_buy", status="placed", rung=1, price=p("BBB"),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 1.5, venue="revx")
    row("dpBBBr2x1", sym="BBB", side="buy", kind="deploy_buy", status="PARTIALLY_FILLED", price=p("BBB", 0.8),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 2.5)
    row("dpCCCr1x0", sym="CCC", side="buy", kind="deploy_buy", status="open", rung=1, price=p("CCC"),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 3.5)
    row("lsAAAs1x0", sym="AAA", side="sell", kind="ladder_sell", status="filled", price=p("AAA", 1.2),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(100, 900), 1), filled_quote=r2(r.uniform(20, 90), 1),
        ts=t + 4.5, filled_ts=t + 400.5)
    row("lsBBBs1x0", sym="BBB", side="sell", kind="ladder_sell", status="filled", price=p("BBB", 1.2),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(100, 900), 1), filled_quote=r2(r.uniform(20, 90), 1),
        ts=t + 5.5, filled_ts=t + 500.5)
    row("dpDDDr1x0", sym="DDD", side="buy", kind="deploy_buy", status="filled", rung=1, price=p("DDD"),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 6.5)
    row("dpDDDr2x0", sym="DDD", side="buy", kind="deploy_buy", status="filled", rung=2, price=p("DDD", 0.8),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 7.5)
    row("dpCCCr2x0", sym="CCC", side="buy", kind="deploy_buy", status="canceled", rung=2, price=p("CCC", 0.8),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 8.5, exch="gate")
    row("dpVVVr1x0", sym="VVV", side="buy", kind="deploy_buy", status="open", rung=1, price=p("VVV"),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 9.5, exch="gate")
    row("dpVVVr2x0", sym="VVV", side="buy", kind="deploy_buy", status="placed", rung=2, price=p("VVV", 0.8),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 10.5)
    row("dpDDDr3x0", sym="DDD", side="buy", kind="deploy_buy", status="canceled", rung=3, price=p("DDD", 0.7),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 11.5, exch="gate")
    row("dpAAAr3x0", sym="AAA", side="buy", kind="deploy_buy", status="closed", price=p("AAA", 0.7),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(10, 90), 1), ts=t + 12.5)
    row("lsCCCs1x0", sym="CCC", side="sell", kind="ladder_sell", status="filled", price=p("CCC", 1.3),
        quote=r2(r.uniform(20, 90), 1), base=r2(r.uniform(100, 900), 1), ts=t + 13.5)
    inp["journal"] = dict(rows)
    lines = []
    for i, (sym, side, kind) in enumerate([("AAA", "buy", "deploy"), ("BBB", "buy", "deploy"),
                                           ("CCC", "sell", "ladder")]):
        lines.append(dumps({"ts": t + 100 * i + 0.25, "sym": sym, "side": side, "kind": kind,
                            "text": f"{side} {sym}", "src": "synthetic"}))
    lines.append("not json at all")
    lines.append(dumps({"ts": t + 400.25, "sym": "DDD", "side": "buy", "kind": "deploy", "text": "buy DDD",
                        "src": "synthetic"}))
    lines.append(dumps({"ts": t + 500.25, "kind": "note", "text": "a synthetic note without a coin"}))
    lines.append(dumps({"ts": t + 600.25, "sym": "VVV", "side": "buy", "kind": "deploy", "text": "buy VVV",
                        "src": "synthetic"}))
    lines.append(dumps({"ts": t + 700.25, "sym": "AAA", "side": "sell", "kind": "ladder", "text": "sell AAA",
                        "src": "synthetic"}))
    lines.append(dumps({"ts": now - 600.25, "sym": "BTC", "side": "buy", "kind": "plan", "text": "plan BTC",
                        "src": "synthetic"}))
    if degraded:
        lines.append(dumps({"ts": t + 800.25, "sym": "BBB", "side": "buy", "kind": "deploy", "src": "synthetic"}))
    inp["decisions"] = "\n".join(lines) + "\n"
    inp["audit"] = None if degraded else {"ts": t + 0.5, "ok": False, "findings": ["synthetic finding"],
                                          "checked": {"revx": 3, "gate": 2}}
    inp["deploy_status"] = None if degraded else {"last_ok_ts": t + 0.5, "last_fail_ts": (t + 900.5) if stale else None,
                                                  "consecutive_failures": 2 if stale else 0}
    if degraded:
        inp["manual_fills"] = [{"status": "open"}, 7, {"status": "filled"}]
    else:
        inp["manual_fills"] = [
            {"client_id": "manual-synthetic", "sym": "DDD", "side": "buy", "kind": "manual", "status": "filled",
             "venue": "revx", "base": r2(r.uniform(10, 90), 1), "quote": r2(r.uniform(10, 90), 1),
             "price": r2(r.uniform(1, 9), 1), "ts": t + 0.5, "note": "synthetic manual fill"},
            {"client_id": "manual-open", "sym": "BBB", "side": "buy", "kind": "manual", "status": "open", "ts": t + 1.5}]
    usd_av, usd_res = r2(r.uniform(100, 900), 1), r2(r.uniform(10, 90), 1)
    revx_bal = [{"currency": "BTC", "available": r2(r.uniform(0.1, 0.9), 4), "reserved": 0, "total": 0.0},
                {"currency": "USD", "available": usd_av, "reserved": usd_res, "total": r2(usd_av + usd_res, 2)},
                {"currency": "BBB", "available": r.randint(10, 99), "reserved": 0, "total": r.randint(10, 99)},
                {"currency": "DDD", "available": r.randint(100, 999), "reserved": r.randint(1, 9),
                 "total": r.randint(1000, 1999)},
                {"currency": "USDC", "available": 0, "reserved": 0, "total": 0},
                {"currency": "EUR", "available": r2(r.uniform(1, 9), 2), "reserved": 0, "total": r2(r.uniform(1, 9), 2)},
                {"currency": "AAA", "available": 0.0, "reserved": 0.0, "total": "0"},
                {"currency": "CCC", "available": "1.5", "reserved": "0", "total": "1.5"},
                {"currency": "VVV", "available": r2(r.uniform(1, 9), 1), "reserved": 0, "total": r2(r.uniform(1, 9), 1)},
                {"currency": "XXX", "available": 0, "reserved": 0, "total": 0}]
    if stale:
        inp["revx_cache"] = {"ts": now - 300.25, "data": {
            "balances": {"AAA": {"available": r2(r.uniform(10, 90), 1), "reserved": 0.0,
                                 "total": r2(r.uniform(10, 90), 1)},
                         "BBB": {"available": r2(r.uniform(10, 90), 1), "reserved": r2(r.uniform(1, 9), 1),
                                 "total": r2(r.uniform(10, 90), 1)},
                         "USD": {"available": r2(r.uniform(1000, 9000), 1), "reserved": r2(r.uniform(1, 9), 1),
                                 "total": r2(r.uniform(1000, 9000), 1)}},
            "orders": [{"sym": "BBB", "side": "buy", "price": p("BBB", 0.8), "qty": r2(r.uniform(10, 90), 1),
                        "usd": r2(r.uniform(10, 90), 1), "below_spot_pct": r2(r.uniform(10, 30), 3), "ts": t + 0.5},
                       {"sym": "CCC", "side": "buy", "price": p("CCC", 0.8), "qty": r2(r.uniform(10, 90), 1),
                        "usd": r2(r.uniform(10, 90), 1), "below_spot_pct": None, "ts": None}],
            "eur_usd": None, "usd_free": r2(r.uniform(1000, 9000), 1), "usd_reserved": r2(r.uniform(1, 9), 1),
            "hold_value": r2(r.uniform(100, 900), 2), "total_value": r2(r.uniform(1000, 9000), 2)}}
    else:
        inp["revx_cache"] = None
    http = {}
    rv = "https://revx.revolut.com/api/1.0"
    if degraded:
        http[f"{rv}/balances"] = {"network": "synthetic revx outage"}
    elif stale:
        http[f"{rv}/balances"] = {"status": 429}
    else:
        http[f"{rv}/balances"] = {"status": 200, "body": revx_bal}
    http[f"{rv}/orders/active"] = {"status": 200, "body": {"data": [] if degraded else [
        {"symbol": "BBB/USD", "side": "buy", "price": p("BBB", 0.8), "quantity": r.randint(10, 99),
         "created_date": int((t + 0.5) * 1000)},
        {"symbol": "CCC/USD", "side": "buy", "price": p("CCC", 0.8), "quantity": r.randint(10, 99), "created_date": None}]}}
    if full:
        http[f"{rv}/public/tickers"] = {"status": 200, "body": {"data": [
            {"symbol": "USDC/EUR", "bid": 0.91, "ask": 0.93},
            {"symbol": "BBB/USD", "bid": prices_all["BBB"][0], "ask": r2(prices_all["BBB"][0] * 1.01, 2)},
            {"symbol": "BTC/USD", "bid": int(prices_all["BTC"][0]), "ask": int(prices_all["BTC"][0]) + 10},
            {"symbol": "DDD/USD", "bid": prices_all["DDD"][0], "ask": r2(prices_all["DDD"][0] * 1.01, 2)},
            {"symbol": "THIN/USD", "bid": "", "ask": ""},
            {"symbol": "VVV/USD", "bid": prices_all["VVV"][0], "ask": r2(prices_all["VVV"][0] * 1.01, 2)},
            {"symbol": "ZERO/USD", "bid": 0, "ask": 0}]}}
    rest = "https://rest.cosmos.directory"
    for i, (sym, chain, a, dec, _) in enumerate(com["cosmos"]):
        if degraded and i == 3:
            continue  # the fourth chain does not answer: no fixture is a network error
        base = f"{rest}/{chain}"
        denom = "u" + sym.lower()
        http[f"{base}/cosmos/staking/v1beta1/params"] = {"status": 200, "body": {"params": {
            "bond_denom": denom, "unbonding_time": f"{r.choice([14, 21, 28]) * 86400}s"}}}
        http[f"{base}/cosmos/bank/v1beta1/balances/{a}"] = {"status": 200, "body": {"balances": [
            {"denom": "ibc/SYNTHETIC", "amount": str(r.randint(1, 9))},
            {"denom": denom, "amount": str(r.randint(1_000_000, 99_000_000))}]}}
        dels = [{"balance": {"denom": denom, "amount": str(r.randint(10_000_000, 900_000_000))}}]
        http[f"{base}/cosmos/staking/v1beta1/delegations/{a}"] = {"status": 200, "body": {"delegation_responses": dels}}
        ub = [{"entries": [{"balance": str(r.randint(1_000_000, 9_000_000)), "completion_time": f"2030-03-{d}T10:00:00Z"}
                           for d in ((20,) if i == 0 else (18, 25))]}] if i in (0, 1) else []
        http[f"{base}/cosmos/staking/v1beta1/delegators/{a}/unbonding_delegations"] = {
            "status": 200, "body": {"unbonding_responses": ub}}
        http[f"{base}/cosmos/distribution/v1beta1/delegators/{a}/rewards"] = {"status": 200, "body": {"total": [
            {"denom": denom, "amount": str(r.randint(1, 9))}, {"denom": "ibc/SYNTHETIC", "amount": f"{r.uniform(1, 9):.3f}"}]}}
    tok = com["tok"]
    holder = com["evm"][2:].lower()
    rate = r2(r.uniform(1.0, 1.2), 6)
    http[eth_key(com["eth_rpc"], tok[1], "0x07a2d13a" + "de0b6b3a7640000".rjust(64, "0"))] = {
        "status": 200, "body": {"jsonrpc": "2.0", "id": 1, "result": eth_hex(rate)}}
    for rpc, t_, v in ((com["eth_rpc"], tok[0], tok[1]), (com["l2_rpc"], tok[2], tok[3])):
        data = "0x70a08231" + holder.rjust(64, "0")
        http[eth_key(rpc, v, data)] = {"status": 200, "body": {"jsonrpc": "2.0", "id": 1,
                                                               "result": eth_hex(r2(r.uniform(10, 900), 2))}}
        http[eth_key(rpc, t_, data)] = {"status": 200, "body": {"jsonrpc": "2.0", "id": 1, "result": "0x0"}}
    if degraded:
        http[f"https://mempool.space/api/address/{com['btc_addr']}"] = {"status": 500, "body": "synthetic error"}
    else:
        funded = r.randint(10_000_000, 90_000_000)
        http[f"https://mempool.space/api/address/{com['btc_addr']}"] = {"status": 200, "body": {
            "address": com["btc_addr"], "chain_stats": {"funded_txo_sum": funded,
                                                        "spent_txo_sum": funded - r.randint(1_000_000, 9_000_000)}}}
        cgid = lambda c: com["cg_ids"].get(c) or next(x[4] for x in com["cosmos"] if x[0] == c)  # noqa: E731
        ids = [cgid(x[0]) for x in com["cosmos"]] + [cgid("VVV"), cgid("BTC")]
        # WWW (wallet only) is quoted here; AAA, CCC and VVV fall back to the bot's price, far above their entry;
        # ZZZ has no price anywhere
        http["https://api.coingecko.com/api/v3/simple/price?vs_currencies=usd&ids=" + ",".join(ids)] = {
            "status": 200, "body": {"www-coin": {"usd": r2(r.uniform(6, 9), 2)},
                                    "bitcoin": {"usd": prices_all["BTC"][0]}}}
        mids = ",".join(com["cg_ids"][c] for c in COINS)
        http["https://api.coingecko.com/api/v3/coins/markets?vs_currency=usd&ids=" + mids] = {"status": 200, "body": [
            {"symbol": "btc", "ath": r2(r.uniform(100000, 130000), 1), "ath_date": "2029-10-06T00:00:00.000Z"},
            {"symbol": "aaa", "ath": r2(r.uniform(10, 40), 2), "ath_date": "2029-04-01T00:00:00.000Z"},
            {"symbol": "bbb", "ath": r2(r.uniform(5, 20), 1), "ath_date": "2029-05-02"},
            {"symbol": "ccc", "ath": r2(r.uniform(5, 20), 1), "ath_date": "2029-05-03"},
            {"symbol": "zzz", "ath": 1.0, "ath_date": "2029-05-04"},
            {"symbol": "vvv", "ath": r2(r.uniform(5, 20), 1), "ath_date": "2029-05-05"},
            {"symbol": "ddd", "ath": r2(r.uniform(5, 20), 1), "ath_date": "2029-05-06"}]}
    inp["http"] = http
    inp["wallet_targets"] = {"AAA": r2(prices_all["AAA"][0] * 0.5, 2)}
    # full: AAA, CCC and VVV alert (ZZZ fired earlier, without a rung); stale: only VVV; degraded: none
    fired = [] if degraded else (["ZZZ"] if full else ["ZZZ", "AAA", "CCC"])
    inp["wallet_state"] = {c: {"fired": True, "ts": int(now - 86400 * 2)} for c in fired}
    inp["wallet_last_good"] = {} if not degraded else {"WWW": {
        "liquid": r2(r.uniform(1, 9), 1), "staked": r2(r.uniform(1, 9), 1), "rewards": r2(r.uniform(0.1, 0.9), 1),
        "unbonding": [], "unbond_days": 21, "chain": f"{com['cosmos'][3][1]} (last good)",
        "address": com["cosmos"][3][2], "read_ts": int(now - 86400)}}
    env = {"SCENARIO_DAYS": "120", "SCENARIO_DEX_USD": "2500", "SCENARIO_NEW_CASH_USD": "1000"}
    if stale:
        env = {"SCENARIO_FEE_PCT": "0.25", "SCENARIO_SLIP_PCT": "0.5", **env}
    inp["scenario_env"] = env
    order = ["now", "log_mtime_ns", "halt", "mode", "live_enabled", "target_pct", "first_pct", "watchlist", "routing",
             "entries", "alloc", "legacy_venues", "legacy_before_ts", "gate", "revx_pairs", "prices", "regime",
             "history", "ladder_state", "journal", "decisions", "audit", "deploy_status", "manual_fills", "revx_cache",
             "http", "wallet_targets", "wallet_state", "wallet_last_good", "scenario_env", "regime_error"]
    return {k: inp[k] for k in order if k in inp}


def dashboard():
    com = common()
    out = {DASH / "common.json": dumps(com, 1)}
    for b in ("full", "degraded", "stale"):
        out[DASH / b / "inputs.json"] = dumps(book(b, com), 1)
    return out


def all_files():
    out = {}
    for part in (cex_dir_state, gate_series, dashboard, backtest_book):
        out.update(part())
    return {p: (t if t.endswith("\n") else t + "\n") for p, t in out.items()}


def main(argv):
    files = all_files()
    if "--sync-mail" in argv:
        sync_monthly_mail()
        return 0
    if "--check" in argv:
        bad = [str(p.relative_to(ROOT)) for p, t in files.items() if not p.exists() or p.read_text() != t]
        for b in bad:
            print("differs:", b)
        return 1 if bad else 0
    for p, t in files.items():
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(t)
    print(f"{len(files)} files written")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
