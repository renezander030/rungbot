//! The onramp top-up: cash that has to stay free on the onramp venue for the pending
//! transfer to the second venue.
//!
//! Deposits land on Revolut X; Gate is funded by a manual transfer from it. The share
//! of the working book (Revolut X + Gate) that Gate should hold is its share of the
//! allocation weights. Until the gap is sent, the dip ladder must not spend it, so the
//! run ring-fences the part of it that is not already staged as free USDC.

use std::collections::BTreeMap;

use crate::venue::Balance;

/// A gap smaller than this is not worth a transfer, and is not reserved.
pub const DUE_MIN_USD: f64 = 50.0;

fn stable(bal: &BTreeMap<String, Balance>, assets: &[&str]) -> f64 {
    assets
        .iter()
        .filter_map(|a| bal.get(*a))
        .map(|b| b.free + b.locked)
        .sum()
}

/// Revolut X stable: USD and USDC, free and locked.
pub fn revx_stable_from(bal: &BTreeMap<String, Balance>) -> f64 {
    // Sum in the balance map's own order, as the reference did.
    bal.iter()
        .filter(|(a, _)| matches!(a.as_str(), "USD" | "USDC"))
        .map(|(_, b)| b.free + b.locked)
        .fold(0.0, |acc, x| acc + x)
}

/// Gate stable: USDT and USDC, free and locked.
pub fn gate_stable_from(bal: &BTreeMap<String, Balance>) -> f64 {
    stable(bal, &["USDT", "USDC"])
}

/// Gate's weight share of the working book. Coins routed elsewhere do not count.
pub fn gate_share(alloc: &[(String, f64)], routing: &[(String, String)]) -> f64 {
    let mut by_exch: BTreeMap<&str, f64> = BTreeMap::new();
    for (sym, exch) in routing {
        let w = alloc
            .iter()
            .find(|(s, _)| s == sym)
            .map_or(0.0, |(_, w)| *w);
        *by_exch.entry(exch.as_str()).or_insert(0.0) += w;
    }
    let gate = by_exch.get("gate").copied().unwrap_or(0.0);
    let working = by_exch.get("revx").copied().unwrap_or(0.0) + gate;
    if working != 0.0 {
        gate / working
    } else {
        0.0
    }
}

/// Stable that should stay free on Revolut X for the pending Gate top-up, 0 below
/// [`DUE_MIN_USD`].
pub fn onramp_reserve(
    rx_stable: f64,
    gate_stable: f64,
    alloc: &[(String, f64)],
    routing: &[(String, String)],
) -> f64 {
    let gap = (rx_stable + gate_stable) * gate_share(alloc, routing) - gate_stable;
    if gap >= DUE_MIN_USD {
        gap
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(free: f64, locked: f64) -> Balance {
        Balance { free, locked }
    }

    #[test]
    fn the_reserve_is_the_gap_to_the_gate_share() {
        let alloc = vec![("AAA".to_string(), 3.0), ("BBB".to_string(), 1.0)];
        let routing = vec![
            ("AAA".to_string(), "revx".to_string()),
            ("BBB".to_string(), "gate".to_string()),
        ];
        assert_eq!(gate_share(&alloc, &routing), 0.25);
        // 1000 total, Gate should hold 250 and holds 100: 150 to send.
        assert_eq!(onramp_reserve(900.0, 100.0, &alloc, &routing), 150.0);
        // A 40 gap is under the minimum.
        assert_eq!(onramp_reserve(750.0, 210.0, &alloc, &routing), 0.0);
        let mut rx = BTreeMap::new();
        rx.insert("USD".to_string(), b(10.0, 5.0));
        rx.insert("USDC".to_string(), b(1.0, 0.0));
        rx.insert("AAA".to_string(), b(9.0, 0.0));
        assert_eq!(revx_stable_from(&rx), 16.0);
    }
}
