//! The arithmetic behind a live run's housekeeping, with no venue and no file in sight.
//!
//! * [`limit_target`]: the paired take-profit price, lifted so the target is net of the
//!   sell-side fee.
//! * [`weighted_cost_basis`]: the fill-based cost basis after a buy.
//! * [`effective_ttl_days`]: how long a resting take-profit may wait before it is worth a
//!   look, which depends on the market regime.
//! * [`ttl_due`]: whether a stale order was already flagged recently.
//! * [`realized`] / [`realized_pct`]: the P&L of a filled sell against the cost basis.
//! * [`drift_exceeds`]: whether a holding moved more than the journal explains.
//!
//! The venue side lives in `rungbot-exec`.

/// Taker fee per side, in percent, for each venue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fees {
    pub gate: f64,
    pub revx: f64,
    pub binance: f64,
}

impl Default for Fees {
    fn default() -> Self {
        Fees {
            gate: 0.2,
            revx: 0.1,
            binance: 0.1,
        }
    }
}

impl Fees {
    /// The fee for `exch`. Anything that is not Binance or Revolut X is charged Gate's.
    pub fn pct(&self, exch: &str) -> f64 {
        match exch {
            "binance" => self.binance,
            "revx" => self.revx,
            _ => self.gate,
        }
    }
}

/// The limit-sell price that nets `target_pct` over `fill_price` after a sell fee of
/// `fee_pct`: `fill * (1 + target/100) / (1 - fee/100)`.
pub fn limit_target(fill_price: f64, target_pct: f64, fee_pct: f64) -> f64 {
    fill_price * (1.0 + target_pct / 100.0) / (1.0 - fee_pct / 100.0)
}

/// The cost basis after buying `fill_base` for `fill_quote` while `held` was already
/// held: a weighted average of the old basis and the fill.
///
/// The old basis is the recorded one, else the configured seed entry, else the fill
/// price; a zero counts as missing at each step. With nothing held and nothing filled,
/// the old basis is kept.
pub fn weighted_cost_basis(
    recorded: Option<f64>,
    seed: Option<f64>,
    held: f64,
    fill_quote: f64,
    fill_base: f64,
    fill_price: f64,
) -> f64 {
    let nonzero = |v: Option<f64>| v.filter(|x| *x != 0.0);
    let old = nonzero(recorded).or(nonzero(seed)).unwrap_or(fill_price);
    if held + fill_base > 0.0 {
        ((held * old) + fill_quote) / (held + fill_base)
    } else {
        old
    }
}

/// Days a resting take-profit may sit before it earns a review warning. `base_days` is
/// the bull value; a bear or chop market gives the order the longer `bear_days` or
/// `chop_days`, since waiting is expected there. `market` is `None` when the regime
/// could not be read, which falls back to `base_days`. `base_days <= 0` turns the
/// warning off (returns 0).
pub fn effective_ttl_days(
    base_days: f64,
    bear_days: f64,
    chop_days: f64,
    market: Option<&str>,
) -> f64 {
    if base_days <= 0.0 {
        return 0.0;
    }
    match market {
        Some("bear") => bear_days,
        Some("chop") => chop_days,
        _ => base_days,
    }
}

/// Should an order last flagged at `last` be flagged again at `now`? Never flagged: yes.
/// Otherwise only once `renag_days` have passed; `renag_days <= 0` means once, ever.
pub fn ttl_due(last: Option<f64>, now: f64, renag_days: f64) -> bool {
    match last {
        None => true,
        Some(l) => renag_days > 0.0 && now - l >= renag_days * 86_400.0,
    }
}

/// Realized USD of selling `qty` for `quote` against `cost_basis`. `None` when the cost
/// basis is unknown or either amount is zero.
pub fn realized(qty: f64, quote: f64, cost_basis: Option<f64>) -> Option<f64> {
    match cost_basis {
        Some(cb) if cb != 0.0 && qty != 0.0 && quote != 0.0 => Some(quote - qty * cb),
        _ => None,
    }
}

/// The sale's average price over the cost basis, in percent.
pub fn realized_pct(avg_price: Option<f64>, cost_basis: Option<f64>) -> Option<f64> {
    match (avg_price, cost_basis) {
        (Some(a), Some(cb)) if a != 0.0 && cb != 0.0 => Some((a / cb - 1.0) * 100.0),
        _ => None,
    }
}

/// Is `have` further from `expect` than rounding and fees explain (1.5%)?
pub fn drift_exceeds(have: f64, expect: f64) -> bool {
    (have - expect).abs() > (expect.abs() * 0.015).max(1e-9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paired_sell_nets_the_target_after_the_fee() {
        let p = limit_target(1.0, 10.0, 0.2);
        assert!((p * (1.0 - 0.002) - 1.1).abs() < 1e-12);
        assert_eq!(Fees::default().pct("kraken"), 0.2);
    }

    #[test]
    fn cost_basis_falls_back_from_record_to_seed_to_fill() {
        assert_eq!(
            weighted_cost_basis(Some(0.0), Some(0.4), 10.0, 5.0, 10.0, 0.5),
            (10.0 * 0.4 + 5.0) / 20.0
        );
        assert_eq!(
            weighted_cost_basis(None, None, 0.0, 5.0, 10.0, 0.5),
            0.5,
            "the fill itself"
        );
        assert_eq!(
            weighted_cost_basis(Some(0.7), None, 0.0, 0.0, 0.0, 0.5),
            0.7
        );
    }

    #[test]
    fn a_stale_order_is_flagged_once_per_window() {
        assert!(ttl_due(None, 100.0, 30.0));
        assert!(!ttl_due(Some(0.0), 29.0 * 86_400.0, 30.0));
        assert!(ttl_due(Some(0.0), 30.0 * 86_400.0, 30.0));
        assert!(!ttl_due(Some(0.0), 1e12, 0.0), "0 = once, ever");
        assert_eq!(effective_ttl_days(14.0, 45.0, 30.0, Some("bear")), 45.0);
        assert_eq!(effective_ttl_days(0.0, 45.0, 30.0, Some("bear")), 0.0);
        assert_eq!(effective_ttl_days(14.0, 45.0, 30.0, None), 14.0);
    }

    #[test]
    fn drift_tolerates_one_and_a_half_percent() {
        assert!(!drift_exceeds(101.4, 100.0));
        assert!(drift_exceeds(101.6, 100.0));
        assert!(drift_exceeds(1e-8, 0.0));
    }
}
