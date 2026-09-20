"""The ladder itself: turn prices plus prior state into the trades this run would make.

`analyze` is a pure function of (config, prices, state, now). Same inputs, same output,
forever — which is why the golden tests in `tests/` can pin the strategy's behaviour.

Five rules run here, and they are the whole strategy:

1. **High-water rungs.** A rung fires once. A retrace never re-fires it; only a deeper
   move advances the ladder.
2. **Directional 24h window.** Once a coin buys, it may only keep buying until the
   window closes, and vice versa. No flip-flopping inside a day.
3. **Dynamic cap.** Each coin's cumulative buy budget breathes with its own P&L, so a
   coin in freefall can only ever burn its own shrinking slice.
4. **Protected core.** Sells stop at `min_core_pct`. The ladder never sells out.
5. **Circuit breaker.** A coin far underwater for long enough stops being dip-bought.
   It is never sold at a loss — the breaker only stops throwing money at it.
"""

from __future__ import annotations

from .config import Config
from .ladder import buy_rung_for, ladder_increment, rung_threshold, sell_rung_for


def analyze(cfg: Config, prices: dict, state: dict, now: float):
    """Return (buys, sells, rows, errors, new_state).

    prices: {symbol: {"price": float, "chg_24h": float | None}} — a symbol missing from
            this mapping is reported in `errors` and its ladder is left untouched.
    state:  the mapping returned by a previous run (`{}` on a cold start).
    now:    epoch seconds.

    buys/sells are the coins that crossed a NEW rung this run. rows is every coin we
    priced, for the full report table.
    """
    buys, sells, rows, errors = [], [], [], []
    new_state = dict(state)
    s = cfg.settings

    for coin in cfg.coins:
        sym = coin.symbol
        entry_data = prices.get(sym)
        if not entry_data or entry_data.get("price") is None:
            errors.append(f"{sym} ({coin.venue}:{coin.pair}): missing from venue ticker")
            continue

        bands = coin.bands_or(s.bands)
        usd = entry_data["price"]
        chg = entry_data.get("chg_24h")
        prev = new_state.get(sym, {})

        # Cost basis starts from the config's `entry` and is then carried in state, so a
        # later run keeps using it even if the config is edited. Sells and P&L measure
        # against THIS number.
        cost = prev.get("cost_basis") or coin.entry
        pnl = ((usd / cost) - 1) * 100 if (cost and usd) else None

        row = {
            "sym": sym, "name": coin.name or sym, "venue": coin.venue, "pair": coin.pair,
            "price": usd, "chg": chg, "entry": cost, "pnl": pnl,
            "target": cost * (1 + s.target_pct / 100) if cost else None,
        }
        rows.append(row)

        deployed = prev.get("deployed_pct", 0.0)
        sold = prev.get("sold_pct", 0.0)
        win_until = prev.get("win_until", 0.0)
        win_dir = prev.get("win_dir", "")

        # Rule 2. While the window is open only its direction may act. Once it closes,
        # re-arm: clear the direction and reset both ladders so a fresh move can open.
        window_open = now < win_until
        if window_open:
            buy_hw, sell_hw = prev.get("buy", 0), prev.get("sell", 0)
        else:
            win_dir, buy_hw, sell_hw = "", 0, 0
        win_secs = s.window_hours * 3600.0

        # Rule 3. Per-coin buy ceiling as a % of its base share, breathing with P&L.
        cap_pct = max(0.0, 100.0 + (pnl if pnl is not None else 0.0))

        # Rule 5. Continuously <= -breaker_pct vs cost basis for breaker_days freezes
        # BUYS for this coin and says so once. It clears itself on recovery.
        below_since = prev.get("below_since", 0.0)
        if s.breaker_pct > 0 and pnl is not None and pnl <= -s.breaker_pct:
            below_since = below_since or now
        elif pnl is not None:
            below_since = 0.0
        breaker = below_since > 0 and (now - below_since) >= s.breaker_days * 86400
        if breaker and not prev.get("breaker"):
            errors.append(
                f"{sym}: CIRCUIT BREAKER -- {pnl:+.1f}% vs entry for "
                f">= {s.breaker_days:.0f}d; dip-buys frozen until it recovers "
                f"above -{s.breaker_pct:.0f}%"
            )

        # --- BUY side: 24h dips, dynamic cap, stopped past the knife floor, and frozen
        #     while a SELL window is open (no direction flip within the window) ---
        br = buy_rung_for(chg, bands)
        if br is None:                          # 24h data missing -> hold the ladder
            pass
        elif br == 0:                           # back in the neutral band
            buy_hw = 0 if not window_open else buy_hw
        elif chg <= -s.buy_floor_pct or win_dir == "sell" or breaker:
            pass                                # knife floor, sell-locked, or breaker
        elif br > buy_hw:                       # Rule 1: advance only
            new_rungs = list(range(buy_hw + 1, br + 1))
            want = ladder_increment(new_rungs, bands)
            allowed = max(0.0, min(want, cap_pct - deployed))    # clamp to remaining bag
            if allowed >= s.min_trade_pct:
                buys.append({**row, "rung": br, "new_rungs": new_rungs,
                             "threshold": rung_threshold(br, bands),
                             "buy_pct": allowed, "cap_pct": cap_pct,
                             "deployed_pct": deployed + allowed,
                             "capped": allowed < want or (deployed + allowed) >= cap_pct})
                deployed += allowed
                win_dir, win_until = "buy", now + win_secs       # open/extend the window
            buy_hw = br

        # --- SELL side: profit vs cost basis, protected core, frozen while a BUY
        #     window is open ---
        sr = sell_rung_for(pnl, bands)
        peak_pnl = prev.get("peak_pnl", 0.0) if window_open else 0.0
        trailing = s.trail == "on"
        if sr is None:                          # no entry or no price -> cannot judge
            pass
        elif sr == 0:                           # below the first target above entry
            sell_hw = 0 if not window_open else sell_hw
            peak_pnl = 0.0 if not window_open else peak_pnl
        elif win_dir == "buy":                  # buy-locked this window
            pass
        else:
            # Not trailing (the default): fire every newly crossed rung now. Trailing:
            # rung 1 still fires immediately to lock the first slice; upper rungs are
            # held while the move runs and fire together once P&L gives back
            # trail_giveback_pct from the episode peak.
            fire_to = 0
            if not trailing:
                if sr > sell_hw:
                    fire_to = sr
            else:
                peak_pnl = max(peak_pnl, pnl)
                if sell_hw == 0:
                    fire_to = 1
                else:
                    peak_rung = sell_rung_for(peak_pnl, bands) or 0
                    if peak_rung > sell_hw and pnl <= peak_pnl - s.trail_giveback_pct:
                        fire_to = peak_rung                      # give-back: harvest it
            if fire_to > sell_hw:
                new_rungs = list(range(sell_hw + 1, fire_to + 1))
                want = ladder_increment(new_rungs, bands)
                sellable = max(0.0, (100.0 - s.min_core_pct) - sold)   # Rule 4
                allowed = min(want, sellable)
                if allowed >= s.min_trade_pct:
                    sells.append({**row, "rung": fire_to, "new_rungs": new_rungs,
                                  "threshold": rung_threshold(fire_to, bands),
                                  "sell_pct": allowed, "sold_pct": sold + allowed,
                                  "capped": allowed < want})
                    sold += allowed
                    # NB: we do NOT subtract a position-% from a bag-%. Selling frees the
                    # bag via the actual stable balance on the next run; the two ledgers
                    # stay in their own units.
                    win_dir, win_until = "sell", now + win_secs
                sell_hw = fire_to

        new_state[sym] = {"buy": buy_hw, "sell": sell_hw,
                          "deployed_pct": deployed, "sold_pct": sold,
                          "win_until": win_until, "win_dir": win_dir, "cost_basis": cost,
                          "below_since": below_since, "breaker": breaker,
                          "peak_pnl": peak_pnl}

        row["committed_pct"] = deployed
        row["cap_now_pct"] = cap_pct
        row["over_budget"] = deployed > cap_pct + 1e-9
        row["win_dir"] = win_dir if window_open else ""
        row["breaker"] = breaker
        row["trailing"] = trailing

    rows.sort(key=lambda r: (r["chg"] if r["chg"] is not None else -999), reverse=True)
    buys.sort(key=lambda r: r["chg"])                    # deepest dip first
    sells.sort(key=lambda r: (r["pnl"] if r["pnl"] is not None else -999), reverse=True)
    return buys, sells, rows, errors, new_state
