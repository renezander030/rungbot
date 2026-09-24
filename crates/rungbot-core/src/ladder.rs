//! Ladder arithmetic: which rung a move has reached, and how much that rung trades.
//!
//! Pure. No I/O, no globals, no notion of a venue — which is what lets the golden test
//! pin the whole strategy against a frozen scenario.
//!
//! The model: a coin's 24h move (buys) or its profit against cost basis (sells) is
//! divided into rungs. The first rung sits at `first_pct`; every further rung is
//! `step_pct` beyond the last. Rung numbers are 1-based; `Some(0)` means "inside the
//! neutral band" and `None` means "no data, hold the ladder where it is".

use serde::{Deserialize, Serialize};

use crate::pymath::{floordiv, fsum_py};

/// The two numbers that define one coin's ladder spacing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bands {
    pub first_pct: f64,
    pub step_pct: f64,
}

impl Default for Bands {
    fn default() -> Self {
        Bands {
            first_pct: 10.0,
            step_pct: 5.0,
        }
    }
}

impl Bands {
    pub fn new(first_pct: f64, step_pct: f64) -> Result<Self, String> {
        // NaN must be rejected too, hence the explicit check rather than `<= 0.0`.
        if first_pct.is_nan() || first_pct <= 0.0 {
            return Err(format!("first_pct must be > 0, got {first_pct}"));
        }
        if step_pct.is_nan() || step_pct <= 0.0 {
            return Err(format!("step_pct must be > 0, got {step_pct}"));
        }
        Ok(Bands {
            first_pct,
            step_pct,
        })
    }
}

/// Dip rung from the 24h change. `None` = data missing (hold), `Some(0)` = neutral.
pub fn buy_rung_for(chg: Option<f64>, b: Bands) -> Option<i64> {
    let chg = chg?;
    if chg <= -b.first_pct {
        Some(floordiv(chg.abs() - b.first_pct, b.step_pct) as i64 + 1)
    } else {
        Some(0)
    }
}

/// Profit rung measured from cost basis.
///
/// A sell can only fire at `>= +first_pct` above entry, so the ladder never sells at a
/// loss. `None` = no entry or no price, `Some(0)` = below the first target.
pub fn sell_rung_for(pnl: Option<f64>, b: Bands) -> Option<i64> {
    let pnl = pnl?;
    if pnl >= b.first_pct {
        Some(floordiv(pnl - b.first_pct, b.step_pct) as i64 + 1)
    } else {
        Some(0)
    }
}

/// The % move at which a given rung number fires (magnitude, always positive).
pub fn rung_threshold(rung: i64, b: Bands) -> f64 {
    b.first_pct + (rung - 1) as f64 * b.step_pct
}

/// Percent to trade for the rungs newly crossed this run.
///
/// The first rung trades `first_pct`; each further rung trades `step_pct` more. So a
/// single fresh rung 1 -> 10%, a deepening rung -> 5%, and a jump that crosses rungs
/// 1+2+3 at once -> 10+5+5 = 20%, i.e. the cumulative size of the move.
pub fn ladder_increment(new_rungs: &[i64], b: Bands) -> f64 {
    fsum_py(
        new_rungs
            .iter()
            .map(|r| if *r == 1 { b.first_pct } else { b.step_pct }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Bands = Bands {
        first_pct: 10.0,
        step_pct: 5.0,
    };
    const W: Bands = Bands {
        first_pct: 15.0,
        step_pct: 8.0,
    };

    #[test]
    fn buy_rungs_at_every_boundary() {
        assert_eq!(
            buy_rung_for(None, B),
            None,
            "missing 24h data holds the ladder"
        );
        assert_eq!(buy_rung_for(Some(0.0), B), Some(0), "flat is neutral");
        assert_eq!(
            buy_rung_for(Some(25.0), B),
            Some(0),
            "a pump is not a dip rung"
        );
        assert_eq!(
            buy_rung_for(Some(-9.99), B),
            Some(0),
            "just inside the band is neutral"
        );
        assert_eq!(
            buy_rung_for(Some(-10.0), B),
            Some(1),
            "exactly at first_pct fires rung 1"
        );
        assert_eq!(
            buy_rung_for(Some(-14.9), B),
            Some(1),
            "still rung 1 below the next step"
        );
        assert_eq!(
            buy_rung_for(Some(-15.0), B),
            Some(2),
            "exactly at the step fires rung 2"
        );
        assert_eq!(
            buy_rung_for(Some(-30.0), B),
            Some(5),
            "a deep dip counts every step"
        );
    }

    #[test]
    fn sells_never_fire_at_a_loss() {
        assert_eq!(
            sell_rung_for(None, B),
            None,
            "no basis means no sell judgement"
        );
        assert_eq!(
            sell_rung_for(Some(-50.0), B),
            Some(0),
            "never a sell rung at a loss"
        );
        assert_eq!(
            sell_rung_for(Some(0.0), B),
            Some(0),
            "break-even is not a sell"
        );
        assert_eq!(
            sell_rung_for(Some(9.99), B),
            Some(0),
            "just under target is not a sell"
        );
        assert_eq!(
            sell_rung_for(Some(10.0), B),
            Some(1),
            "exactly at target fires rung 1"
        );
        assert_eq!(sell_rung_for(Some(20.0), B), Some(3), "+20% is rung 3");
    }

    #[test]
    fn a_rungs_threshold_fires_exactly_that_rung() {
        for r in 1..8 {
            assert_eq!(buy_rung_for(Some(-rung_threshold(r, B)), B), Some(r));
        }
        assert_eq!(rung_threshold(1, B), 10.0);
        assert_eq!(rung_threshold(5, B), 30.0);
    }

    #[test]
    fn increments_sum_to_the_size_of_the_move() {
        assert_eq!(
            ladder_increment(&[1], B),
            10.0,
            "a fresh rung 1 trades first_pct"
        );
        assert_eq!(
            ladder_increment(&[2], B),
            5.0,
            "a deepening rung trades step_pct"
        );
        assert_eq!(
            ladder_increment(&[1, 2, 3], B),
            20.0,
            "a 3-rung jump trades 10+5+5"
        );
        assert_eq!(ladder_increment(&[], B), 0.0, "no new rungs, no trade");
    }

    #[test]
    fn per_coin_bands_are_honoured() {
        assert_eq!(
            buy_rung_for(Some(-14.9), W),
            Some(0),
            "wide bands: -14.9% is neutral"
        );
        assert_eq!(
            buy_rung_for(Some(-15.0), W),
            Some(1),
            "wide bands: rung 1 at -15%"
        );
        assert_eq!(
            buy_rung_for(Some(-23.0), W),
            Some(2),
            "wide bands: rung 2 at -23%"
        );
        assert_eq!(
            ladder_increment(&[1, 2], W),
            23.0,
            "wide bands increment 15+8"
        );
    }

    #[test]
    fn bands_validate_themselves() {
        assert!(Bands::new(0.0, 5.0).is_err());
        assert!(Bands::new(10.0, 0.0).is_err());
        assert!(Bands::new(-5.0, 5.0).is_err());
        assert!(Bands::new(10.0, 5.0).is_ok());
    }
}
