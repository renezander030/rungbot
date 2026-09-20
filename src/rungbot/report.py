"""Human-readable output. JSON is the other half, and lives in the CLI.

Sizes are percentages on purpose. A buy is a % of that coin's base share of your dry
powder; a sell is a % of the position you hold. rungbot does not know your balances and
does not ask for them, so it cannot and will not print dollar amounts.
"""

from __future__ import annotations


def fmt_price(v) -> str:
    if v is None:
        return "-"
    if v >= 1000:
        return f"{v:,.0f}"
    if v >= 1:
        return f"{v:,.2f}"
    if v >= 0.01:
        return f"{v:.4f}"
    return f"{v:.6f}"


def _pct(v) -> str:
    return "-" if v is None else f"{v:+.1f}%"


def _flags(r) -> str:
    out = []
    if r.get("breaker"):
        out.append("BREAKER")
    if r.get("win_dir"):
        out.append(f"{r['win_dir']}-locked")
    if r.get("over_budget"):
        out.append("over-budget")
    if r.get("trailing"):
        out.append("trail")
    return " ".join(out)


def render(buys, sells, rows, errors, cfg, now_iso: str) -> str:
    s = cfg.settings
    L = []
    L.append(f"rungbot plan — {now_iso}")
    L.append(f"bands {s.bands.first_pct:g}/{s.bands.step_pct:g}"
             f" · core {s.min_core_pct:g}% · window {s.window_hours:g}h"
             f" · knife floor -{s.buy_floor_pct:g}% · trail {s.trail}")
    L.append("")

    if buys:
        L.append(f"BUY — {len(buys)} coin(s) crossed a new dip rung")
        for r in buys:
            cap = " (capped)" if r["capped"] else ""
            L.append(f"  {r['sym']:<6} {_pct(r['chg']):>8} 24h  ->  rung {r['rung']}"
                     f" (-{r['threshold']:g}%)  buy {r['buy_pct']:.0f}% of base{cap}"
                     f"   @ {fmt_price(r['price'])}")
    else:
        L.append("BUY — nothing crossed a new dip rung")
    L.append("")

    if sells:
        L.append(f"SELL — {len(sells)} coin(s) crossed a new profit rung")
        for r in sells:
            cap = " (capped by core)" if r["capped"] else ""
            L.append(f"  {r['sym']:<6} {_pct(r['pnl']):>8} P&L  ->  rung {r['rung']}"
                     f" (+{r['threshold']:g}%)  sell {r['sell_pct']:.0f}% of position{cap}"
                     f"   @ {fmt_price(r['price'])}")
    else:
        L.append("SELL — nothing crossed a new profit rung")
    L.append("")

    L.append(f"{'COIN':<7}{'PRICE':>12}{'24H':>9}{'ENTRY':>12}{'P&L':>9}"
             f"{'TARGET':>12}{'USED':>7}  FLAGS")
    for r in rows:
        L.append(
            f"{r['sym']:<7}{fmt_price(r['price']):>12}{_pct(r['chg']):>9}"
            f"{fmt_price(r['entry']):>12}{_pct(r['pnl']):>9}{fmt_price(r['target']):>12}"
            f"{r['committed_pct']:>6.0f}%  {_flags(r)}".rstrip()
        )

    if errors:
        L.append("")
        L.append("NOTES")
        for e in errors:
            L.append(f"  ! {e}")

    L.append("")
    L.append("Notify-only. rungbot holds no keys and places no orders.")
    return "\n".join(L)
