"""Ladder arithmetic: which rung a move has reached, and how much that rung trades.

This module is pure. It has no I/O, no network, no globals and no notion of a venue,
which is what makes the whole strategy testable against frozen golden output.

The model: a coin's 24h move (buys) or its profit against cost basis (sells) is divided
into rungs. The first rung sits at ``first_pct``; every further rung is ``step_pct``
beyond the last. Rung numbers are 1-based; 0 means "inside the neutral band" and None
means "no data, hold the ladder where it is".
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Bands:
    """The two numbers that define one coin's ladder spacing."""

    first_pct: float = 10.0
    step_pct: float = 5.0

    def __post_init__(self) -> None:
        if self.first_pct <= 0:
            raise ValueError(f"first_pct must be > 0, got {self.first_pct}")
        if self.step_pct <= 0:
            raise ValueError(f"step_pct must be > 0, got {self.step_pct}")


def buy_rung_for(chg: float | None, bands: Bands) -> int | None:
    """Dip rung from the 24h change. None = data missing (hold), 0 = neutral (re-arm)."""
    if chg is None:
        return None
    if chg <= -bands.first_pct:
        return int((abs(chg) - bands.first_pct) // bands.step_pct) + 1
    return 0


def sell_rung_for(pnl: float | None, bands: Bands) -> int | None:
    """Profit rung measured from cost basis.

    A sell can ONLY fire at >= +first_pct above entry, so the ladder never sells at a
    loss. None = no entry or no price, 0 = below the first target.
    """
    if pnl is None:
        return None
    if pnl >= bands.first_pct:
        return int((pnl - bands.first_pct) // bands.step_pct) + 1
    return 0


def rung_threshold(rung: int, bands: Bands) -> float:
    """The % move at which a given rung number fires (magnitude, always positive)."""
    return bands.first_pct + (rung - 1) * bands.step_pct


def ladder_increment(new_rungs, bands: Bands) -> float:
    """Percent to trade for the rungs newly crossed this run.

    The first rung trades ``first_pct``; each further rung trades ``step_pct`` more.
    So a single fresh rung 1 -> 10%, a deepening rung -> 5%, and a jump that crosses
    rungs 1+2+3 at once -> 10+5+5 = 20%, i.e. the cumulative size of the move.
    """
    return sum(bands.first_pct if r == 1 else bands.step_pct for r in new_rungs)
