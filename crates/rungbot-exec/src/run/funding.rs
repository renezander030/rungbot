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

/// USD that should move from Revolut X to Gate; at or below 0 Gate holds its share.
pub fn gate_gap(
    rx_stable: f64,
    gate_stable: f64,
    alloc: &[(String, f64)],
    routing: &[(String, String)],
) -> f64 {
    (rx_stable + gate_stable) * gate_share(alloc, routing) - gate_stable
}

/// The dashboard's top-up card, in its field order.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FundingCard {
    pub gate_stable: f64,
    pub gate_target: f64,
    pub gate_share_pct: f64,
    pub gate_gap: f64,
    pub revx_stable: f64,
    /// Revolut X deposits the Gate cushion still absorbs before the card turns due.
    pub deposit_headroom: f64,
    /// What the deploy layer holds free on Revolut X so the transfer is possible.
    pub reserved_on_revx: f64,
    pub revx_sendable: f64,
    /// Due, but nothing sendable yet: staging, re-laddering or in transit.
    pub in_flight: bool,
    pub due: bool,
    /// Free USDC, clamped to the gap: what can be sent now.
    pub send_now: f64,
    pub revx_usdc_free: f64,
}

/// The top-up card from both venues' stable and Revolut X's free USDC. Only USDC can
/// leave Revolut X, so the card names free USDC and nothing else; until the reserve is
/// staged, or while a transfer travels, it says in flight, never an amount.
pub fn card(
    rx_stable: f64,
    gate_stable: f64,
    usdc_free: f64,
    alloc: &[(String, f64)],
    routing: &[(String, String)],
) -> FundingCard {
    let share = gate_share(alloc, routing);
    let gap = gate_gap(rx_stable, gate_stable, alloc, routing);
    let usdc_free = rungbot_core::watch::pyfmt::round(usdc_free.max(0.0), 2);
    let due = gap >= DUE_MIN_USD && usdc_free >= DUE_MIN_USD;
    let headroom = if share != 0.0 && gap < DUE_MIN_USD {
        (-gap + DUE_MIN_USD) / share
    } else {
        0.0
    };
    FundingCard {
        gate_stable,
        gate_target: gap + gate_stable,
        gate_share_pct: share * 100.0,
        gate_gap: gap,
        revx_stable: rx_stable,
        deposit_headroom: headroom,
        reserved_on_revx: onramp_reserve(rx_stable, gate_stable, alloc, routing),
        revx_sendable: usdc_free,
        in_flight: gap >= DUE_MIN_USD && !due,
        due,
        send_now: if due { gap.min(usdc_free) } else { 0.0 },
        revx_usdc_free: usdc_free,
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
    let gap = gate_gap(rx_stable, gate_stable, alloc, routing);
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
