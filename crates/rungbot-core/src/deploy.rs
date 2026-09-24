//! The deploy layer's planning maths: where fresh capital rests as limit-buy zones.
//!
//! A tranche of new capital on a venue is split over the venue's coins, and each coin's
//! share becomes up to a few GTC limit buys below spot ("let price come to us"). This
//! module is the part of that which needs no venue: the depth profile per regime label,
//! the snap onto structure (30-day average, 30-day low), the depth cap, holding the price
//! levels that still rest, re-ordering held levels, and merging rungs that collapse onto
//! one price or fall under the venue minimum. The caller rounds the surviving levels to
//! the venue's tick and lot, which is the only part that needs the venue.
//!
//! Pure: no clock, no network, no filesystem.

use std::collections::BTreeMap;

use crate::watch::pyfmt;

/// Rung depths (% below spot) and weights (% of the coin's amount) per regime label.
/// Bear rests deeper so a falling market fills lower; bull rests shallow so the tranche
/// actually fills. The weights are a tent: the middle rung is the most probable real dip.
pub fn default_profile(label: &str) -> (Vec<f64>, Vec<f64>) {
    let depths = match label {
        "bear" => [8.0, 15.0, 25.0],
        "bull" => [3.0, 7.0, 12.0],
        _ => [5.0, 10.0, 18.0],
    };
    (depths.to_vec(), vec![30.0, 40.0, 30.0])
}

/// A per-coin profile that replaces the regime's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ZoneOverride {
    pub depths: Vec<f64>,
    pub weights: Vec<f64>,
}

/// The profile for `sym` under `label`: its override when it has a usable one, else the
/// regime's. The second value is the warning to print when an override exists but its
/// lists are empty or of unequal length.
pub fn zone_profile(
    label: &str,
    sym: Option<&str>,
    overrides: &BTreeMap<String, ZoneOverride>,
) -> ((Vec<f64>, Vec<f64>), Option<String>) {
    if let Some(sym) = sym.filter(|s| !s.is_empty()) {
        if let Some(ov) = overrides.get(&sym.to_uppercase()) {
            if !ov.depths.is_empty()
                && !ov.weights.is_empty()
                && ov.depths.len() == ov.weights.len()
            {
                return ((ov.depths.clone(), ov.weights.clone()), None);
            }
            return (
                default_profile(label),
                Some(format!(
                    "WARN: DEPLOY_ZONES_JSON[{sym}] needs equal-length depths+weights, \
                     falling back to {label} profile"
                )),
            );
        }
    }
    (default_profile(label), None)
}

/// `(sma30, low30)` from a close series: the mean and the minimum of the last 30
/// closes, or `(None, None)` with fewer than 30.
pub fn structure(closes: &[f64]) -> (Option<f64>, Option<f64>) {
    if closes.len() < 30 {
        return (None, None);
    }
    let w = &closes[closes.len() - 30..];
    let low = w.iter().copied().fold(f64::INFINITY, f64::min);
    (Some(pyfmt::sum(w.iter().copied()) / 30.0), Some(low))
}

/// A price level still resting, which a re-ladder holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pinned {
    pub price: f64,
    pub note: Option<String>,
    /// When the level was first placed.
    pub since: Option<f64>,
}

/// One planned level before venue rounding.
#[derive(Debug, Clone, PartialEq)]
pub struct Level {
    pub price: f64,
    /// Weight, % of the coin's amount; stays with the position when levels re-order.
    pub w: f64,
    pub note: String,
    pub held: bool,
    pub since: Option<f64>,
    pub usd: f64,
}

/// The knobs [`plan_levels`] reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanKnobs {
    /// No rung rests deeper than this % below spot; `<= 0` disables the cap.
    pub max_depth_pct: f64,
    /// Hold the price of levels still resting.
    pub pin_prices: bool,
}

/// `max()` over `(price, name)` tuples: the higher price, then the greater name.
fn max_cand(c: &[(f64, &'static str)]) -> (f64, &'static str) {
    let mut best = c[0];
    for &x in &c[1..] {
        if x.0 > best.0 || (x.0 == best.0 && x.1 > best.1) {
            best = x;
        }
    }
    best
}

/// The levels for one coin with `amount` (already floored to the cent) to rest, before
/// rounding to the venue. Levels whose `usd` ends at 0 were merged into another.
///
/// * Band `j` sits `depths[j]`% under spot; its floor is the next band, or 90% of it for
///   the last one. A structure level inside the band (the 30-day average, or 1% over the
///   30-day low) moves the rung onto it, the shallower of the two winning: snapping only
///   ever goes deeper.
/// * The ladder is kept strictly decreasing (each rung at most 98.5% of the one above),
///   then capped at `max_depth_pct`.
/// * `pinned` (by 1-based rung) holds levels still resting, unless spot has fallen
///   through one, which then re-anchors and says so. Held levels that end out of price
///   order are sorted again, weights staying with the position, and only free rungs
///   move to repair the ladder.
/// * Rungs on an identical price merge into the first; a rung under `minq * 1.02` merges
///   one level deeper, and a last rung under it rolls back up into the nearest one above.
pub fn plan_levels(
    spot: f64,
    amount: f64,
    profile: &(Vec<f64>, Vec<f64>),
    structure: (Option<f64>, Option<f64>),
    minq: f64,
    knobs: PlanKnobs,
    pinned: Option<&BTreeMap<i64, Pinned>>,
) -> Vec<Level> {
    let (depths, weights) = profile;
    let (sma30, low30) = structure;
    let bands: Vec<f64> = depths.iter().map(|d| spot * (1.0 - d / 100.0)).collect();
    let mut rungs: Vec<Level> = Vec::with_capacity(bands.len());
    for (j, &band) in bands.iter().enumerate() {
        let mut p = band;
        let floor = if j + 1 < bands.len() {
            bands[j + 1]
        } else {
            p * 0.90
        };
        let mut note = format!("-{}%", pyfmt::g(depths[j], 6));
        let mut cands: Vec<(f64, &'static str)> = Vec::new();
        if let Some(s) = sma30.filter(|s| *s != 0.0) {
            if floor < s && s < p {
                cands.push((s, "30d SMA"));
            }
        }
        if let Some(l) = low30.filter(|l| *l != 0.0) {
            let lv = l * 1.01;
            if floor < lv && lv < p {
                cands.push((lv, "30d low+1%"));
            }
        }
        if !cands.is_empty() {
            let (cp, name) = max_cand(&cands);
            p = cp;
            note = format!("{name} (-{}%)", pyfmt::fixed((1.0 - p / spot) * 100.0, 1));
        }
        rungs.push(Level {
            price: p,
            w: weights[j],
            note,
            held: false,
            since: None,
            usd: 0.0,
        });
    }
    for j in 1..rungs.len() {
        rungs[j].price = rungs[j].price.min(rungs[j - 1].price * 0.985);
    }
    if knobs.max_depth_pct > 0.0 {
        let floor_px = spot * (1.0 - knobs.max_depth_pct / 100.0);
        for r in rungs.iter_mut() {
            if r.price < floor_px {
                r.price = floor_px;
                r.note = format!("capped -{}%", pyfmt::g(knobs.max_depth_pct, 6));
            }
        }
    }
    if let Some(pinned) = pinned.filter(|p| !p.is_empty() && knobs.pin_prices) {
        for (i, r) in rungs.iter_mut().enumerate() {
            let j = i as i64 + 1;
            let Some(pin) = pinned.get(&j) else {
                continue;
            };
            let px = pin.price;
            if px == 0.0 {
                continue;
            }
            if px >= spot * 0.999 {
                r.note = format!(
                    "{} (held ${} now at/above spot — re-anchored)",
                    r.note,
                    pyfmt::g(px, 6)
                );
                continue;
            }
            r.price = px;
            r.held = true;
            r.since = pin.since;
            let base = pin
                .note
                .as_deref()
                .filter(|n| !n.is_empty())
                .unwrap_or(&r.note)
                .split(" (held")
                .next()
                .unwrap_or_default()
                .to_string();
            r.note = format!("{base} (held)");
        }
        // Order the levels by price again (a stable sort, as the reference's); the
        // weights stay with the position so the ladder keeps its shape.
        let mut by_price: Vec<Level> = rungs.clone();
        by_price.sort_by(|a, b| {
            (-a.price)
                .partial_cmp(&-b.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if by_price != rungs {
            for (r, lv) in rungs.iter_mut().zip(by_price) {
                let w = r.w;
                *r = Level { w, ..lv };
            }
        }
        for j in 1..rungs.len() {
            if !rungs[j].held {
                rungs[j].price = rungs[j].price.min(rungs[j - 1].price * 0.985);
            }
        }
    }
    for r in rungs.iter_mut() {
        r.usd = amount * r.w / 100.0;
    }
    // Rungs the cap collapsed onto one price merge into the first of them.
    let mut seen: Vec<(f64, usize)> = Vec::new();
    for i in 0..rungs.len() {
        let key = pyfmt::round(rungs[i].price, 12);
        if let Some(&(_, first)) = seen.iter().find(|(k, _)| *k == key) {
            let u = rungs[i].usd;
            rungs[first].usd += u;
            rungs[i].usd = 0.0;
        } else {
            seen.push((key, i));
        }
    }
    let n = rungs.len();
    for j in 0..n.saturating_sub(1) {
        if 0.0 < rungs[j].usd && rungs[j].usd < minq * 1.02 {
            let u = rungs[j].usd;
            rungs[j + 1].usd += u;
            rungs[j].usd = 0.0;
        }
    }
    if n > 0 && 0.0 < rungs[n - 1].usd && rungs[n - 1].usd < minq * 1.02 {
        let rem = rungs[n - 1].usd;
        rungs[n - 1].usd = 0.0;
        for j in (0..n - 1).rev() {
            if rungs[j].usd > 0.0 {
                rungs[j].usd += rem;
                break;
            }
        }
    }
    rungs
}

/// `floor(x * 100) / 100`: venues hold cash in whole cents.
pub fn floor_cents(x: f64) -> f64 {
    (x * 100.0).floor() / 100.0
}

/// What a venue reserves for `qty` at `price`: the notional rounded UP to the cent.
pub fn cost(qty: f64, price: f64) -> f64 {
    (qty * price * 100.0 - 1e-9).ceil() / 100.0
}

/// The largest venue-legal quantity whose cent-ceiled cost fits `budget`: start from
/// the budget over the price, rounded by the venue, and back off one `step` at a time
/// (at most 8 times) while the cost overshoots.
pub fn fit_qty<E>(
    budget: f64,
    price: f64,
    step: f64,
    mut round: impl FnMut(f64) -> Result<f64, E>,
) -> Result<f64, E> {
    let mut qty = round(floor_cents(budget) / price)?;
    for _ in 0..8 {
        if qty <= 0.0 || cost(qty, price) <= budget + 1e-9 {
            break;
        }
        qty = if step > 0.0 { round(qty - step)? } else { 0.0 };
    }
    Ok(qty.max(0.0))
}

/// Days since the first bull label after the last bear one; `None` when the last label
/// is not bull. Chop inside the run does not reset it; no bear in the window makes it
/// as old as the window, so an age cutoff fails toward not buying.
pub fn bull_age_days<S: AsRef<str>>(labels: &[S]) -> Option<usize> {
    if labels.last().map(AsRef::as_ref) != Some("bull") {
        return None;
    }
    let start = labels
        .iter()
        .rposition(|x| x.as_ref() == "bear")
        .map_or(0, |i| i + 1);
    let first_bull = (start..labels.len()).find(|&i| labels[i].as_ref() == "bull")?;
    Some(labels.len() - first_bull)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOBS: PlanKnobs = PlanKnobs {
        max_depth_pct: 20.0,
        pin_prices: true,
    };

    #[test]
    fn bull_age_counts_from_the_first_bull_after_the_last_bear() {
        assert_eq!(bull_age_days(&["chop", "bull"]), Some(1));
        assert_eq!(
            bull_age_days(&["bull", "bear", "bull", "chop", "bull"]),
            Some(3)
        );
        assert_eq!(bull_age_days(&["bull", "chop"]), None);
        assert_eq!(bull_age_days::<&str>(&[]), None);
    }

    #[test]
    fn the_chop_profile_rests_three_rungs_below_spot() {
        let lv = plan_levels(
            100.0,
            100.0,
            &default_profile("chop"),
            (None, None),
            1.0,
            KNOBS,
            None,
        );
        let px: Vec<f64> = lv.iter().map(|l| l.price).collect();
        assert_eq!(px, vec![95.0, 90.0, 82.0]);
        assert_eq!(lv[1].usd, 40.0);
        assert_eq!(lv[2].note, "-18%");
    }

    #[test]
    fn a_held_level_above_a_fresh_rung_is_reordered_not_inverted() {
        let mut pins = BTreeMap::new();
        pins.insert(
            2,
            Pinned {
                price: 96.0,
                note: Some("-10% (held)".into()),
                since: Some(7.0),
            },
        );
        let lv = plan_levels(
            100.0,
            100.0,
            &default_profile("chop"),
            (None, None),
            1.0,
            KNOBS,
            Some(&pins),
        );
        assert!(lv[0].held && lv[0].price == 96.0 && lv[0].since == Some(7.0));
        assert_eq!(lv[0].w, 30.0);
        assert!(!lv[1].held && lv[1].price == 94.56);
    }

    #[test]
    fn fit_qty_backs_off_until_the_ceiled_cost_fits() {
        let q = fit_qty::<()>(10.0, 3.0, 0.01, |x| Ok((x / 0.01).floor() * 0.01)).unwrap();
        assert!(cost(q, 3.0) <= 10.0);
    }
}
