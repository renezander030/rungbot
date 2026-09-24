//! The bull-market sell policy: sell right, not early.
//!
//! The cost-relative ladder is a **chop harvester**. In a confirmed bull it sells far too
//! soon — every rung fires on the way up and the position is gone long before the top.
//! While the market label is a confirmed bull, a coin's sell side is governed by this
//! module instead, and the ladder's sell rungs go dormant for that coin.
//!
//! Three mechanisms, each doing a different job:
//!
//! * **Tranches.** Sell a fixed slice of the *original* position the first time price
//!   reaches each multiple of cost. Checked every run, upside only.
//! * **Trail.** Once price reaches `trail_arm_mult × cost` the trail arms. The first
//!   time the daily sample sits `giveback_pct` below the running peak it sells a slice
//!   and re-arms from the hit; after that a hit needs only `giveback_next_pct` below the
//!   last one, and sells the rest down to the core. Evaluated **once per UTC day** — the
//!   evidence for it is on daily closes, and intraday wicks fire slices during ordinary
//!   base-building.
//! * **Armed exit.** An external signal (froth, a manual arm) can force a one-shot exit
//!   to the core `armed_giveback_pct` below the peak, regardless of multiple.
//!
//! Two invariants hold throughout: a `core_pct` slice of the original is never sold by
//! this policy, and nothing is ever sold below `cost + first_pct` — the ladder's
//! never-sell-at-a-loss rule, kept.
//!
//! Pure: no clock, no I/O. `now` gates the trail to one evaluation per UTC day; pass
//! `None` to evaluate every call, which is what a replay over daily data wants.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::fmt::py_g;
use crate::time::utc_day;

/// `%g`: six significant digits, the way every number in a reason line is printed.
fn g(v: f64) -> String {
    py_g(v, 6)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SellPolicyConfig {
    /// How far below the running peak a first trail hit sits, in percent.
    pub giveback_pct: f64,
    /// Once a hit has landed, the fall is confirmed and the next one needs less room.
    pub giveback_next_pct: f64,
    /// An externally armed exit is tighter: the signal has already done the waiting.
    pub armed_giveback_pct: f64,
    /// Never sold by this policy, as a percent of the original position.
    pub core_pct: f64,
    /// Sold at each multiple in `tranches`.
    pub tranche_pct: f64,
    pub tranches: Vec<f64>,
    /// Sold on the first trail hit. The rest goes on later hits.
    pub trail_slice_pct: f64,
    /// Sold on subsequent hits. `0` means "everything down to the core".
    pub trail_slice_next_pct: f64,
    /// The multiple of cost at which the trail arms by itself.
    pub trail_arm_mult: f64,
    /// Coins that take no tranches — typically ones you intend to hold through.
    pub no_tranche: Vec<String>,
    /// Coins with no unarmed trail, which only exit on an external arm.
    ///
    /// Empty by default. A large-cap that whipsaws through its own trail is the case
    /// this exists for; which coins those are is yours to decide, not the tool's.
    pub no_base_trail: Vec<String>,
    /// Per-coin override of `giveback_pct` (an unarmed first hit).
    #[serde(default)]
    pub giveback: BTreeMap<String, f64>,
    /// Per-coin override of `trail_arm_mult`.
    #[serde(default)]
    pub trail_arm: BTreeMap<String, f64>,
}

impl Default for SellPolicyConfig {
    fn default() -> Self {
        SellPolicyConfig {
            giveback_pct: 40.0,
            giveback_next_pct: 20.0,
            armed_giveback_pct: 15.0,
            core_pct: 20.0,
            tranche_pct: 20.0,
            tranches: vec![4.0, 8.0, 16.0, 32.0],
            trail_slice_pct: 25.0,
            trail_slice_next_pct: 0.0,
            trail_arm_mult: 2.0,
            no_tranche: Vec::new(),
            no_base_trail: Vec::new(),
            giveback: BTreeMap::new(),
            trail_arm: BTreeMap::new(),
        }
    }
}

impl SellPolicyConfig {
    fn takes_tranches(&self, sym: &str) -> bool {
        !self.no_tranche.iter().any(|s| s == sym) && self.tranche_pct > 0.0
    }

    fn has_base_trail(&self, sym: &str) -> bool {
        !self.no_base_trail.iter().any(|s| s == sym)
    }

    fn giveback_for(&self, sym: &str, armed: bool) -> f64 {
        if armed {
            self.armed_giveback_pct
        } else {
            self.giveback.get(sym).copied().unwrap_or(self.giveback_pct)
        }
    }

    /// The multiple of cost at which `sym`'s trail arms by itself.
    pub fn trail_arm_for(&self, sym: &str) -> f64 {
        self.trail_arm
            .get(sym)
            .copied()
            .unwrap_or(self.trail_arm_mult)
    }
}

/// What the policy remembers about one coin between runs.
///
/// Fractions are of the **original** bull-mode position, tracked as `remaining` percent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BullState {
    pub peak: f64,
    pub remaining: f64,
    /// Multiples already taken, so each fires once.
    pub tranches: Vec<f64>,
    pub exited: bool,
    /// The give-back percentage in force at the last daily evaluation.
    pub gb: Option<f64>,
    pub held_below_floor: bool,
    pub trail_on: bool,
    pub hits: u32,
    /// The UTC day the trail was last evaluated, so it runs once a day.
    pub trail_day: Option<String>,
    /// When the policy first took the coin over (epoch seconds), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<f64>,
}

impl BullState {
    pub fn fresh(price: f64) -> Self {
        BullState {
            peak: price,
            remaining: 100.0,
            tranches: Vec::new(),
            exited: false,
            gb: None,
            held_below_floor: false,
            trail_on: false,
            hits: 0,
            trail_day: None,
            since: None,
        }
    }
}

/// One sell the policy decided on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySell {
    pub pct_of_original: f64,
    pub pct_of_held: f64,
    pub rung: i64,
    pub reason: String,
}

/// One coin, one run.
///
/// Returns the new state and any sells. The caller commits the state only when the sells
/// actually executed — a skipped or failed sell must roll the coin's state back, or the
/// ledger claims a slice that was never sold.
/// Every one of these is an independent input to the decision; bundling them into a
/// struct purely to satisfy an argument-count lint would hide that.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    sym: &str,
    price: Option<f64>,
    cost: Option<f64>,
    prev: Option<&BullState>,
    first_pct: f64,
    armed: bool,
    now: Option<f64>,
    cfg: &SellPolicyConfig,
) -> (BullState, Vec<PolicySell>) {
    let start_price = price.unwrap_or(0.0);
    let mut b = prev.cloned().unwrap_or_else(|| BullState {
        since: now,
        ..BullState::fresh(start_price)
    });

    let (Some(price), Some(cost)) = (price, cost.filter(|c| *c != 0.0)) else {
        return (b, Vec::new());
    };

    let remaining_before = b.remaining;
    let floor = cost * (1.0 + first_pct / 100.0);
    let mut sells: Vec<PolicySell> = Vec::new();
    let day = now.map(utc_day);
    let daily = day.is_none() || b.trail_day != day;

    // --- tranches on multiples: checked every run, upside only ---
    if cfg.takes_tranches(sym) {
        let mult = price / cost;
        for (i, m) in cfg.tranches.iter().enumerate() {
            if b.tranches.contains(m) || mult < *m {
                continue;
            }
            // Stop at the core, and never take a tranche below the floor.
            if b.remaining - cfg.tranche_pct < cfg.core_pct - 1e-9 || price < floor {
                break;
            }
            b.tranches.push(*m);
            b.remaining -= cfg.tranche_pct;
            sells.push(PolicySell {
                pct_of_original: cfg.tranche_pct,
                pct_of_held: 0.0, // filled in below
                rung: 50 + i as i64,
                reason: format!(
                    "{}x over cost: tranche {}/{}",
                    g(*m),
                    b.tranches.len(),
                    cfg.tranches.len()
                ),
            });
        }
    }

    // --- peak, arming and the trail: once per UTC day on the sampled price ---
    if daily {
        b.trail_day = day;
        b.peak = b.peak.max(price);
        if !b.trail_on && (armed || price >= cost * cfg.trail_arm_for(sym)) {
            b.trail_on = true; // an arm IS the arming
        }

        let mut gb = cfg.giveback_for(sym, armed);
        if !armed && b.hits > 0 && cfg.has_base_trail(sym) {
            gb = cfg.giveback_next_pct; // the fall is confirmed: sell harder
        }
        b.gb = Some(gb);

        if price >= floor {
            b.held_below_floor = false;
        }
        let trail_live = b.trail_on && !b.exited && (armed || cfg.has_base_trail(sym));
        if trail_live && price <= b.peak * (1.0 - gb / 100.0) {
            let room = b.remaining - cfg.core_pct;
            let first = b.hits == 0;
            let slice_pct = if first {
                cfg.trail_slice_pct
            } else {
                cfg.trail_slice_next_pct
            };
            let one_shot = armed || slice_pct <= 0.0;
            let pct = if one_shot { room } else { slice_pct.min(room) };

            if price < floor {
                b.held_below_floor = true; // the never-at-a-loss invariant wins
            } else if pct > 1e-9 {
                b.remaining -= pct;
                b.hits += 1;
                if b.remaining <= cfg.core_pct + 1e-9 {
                    b.exited = true;
                }
                let tail = if b.exited {
                    format!(" -> core {}% kept", g(cfg.core_pct))
                } else {
                    format!(
                        ", slice {}% (hit {}), re-armed at {}",
                        g(pct),
                        b.hits,
                        g(price)
                    )
                };
                sells.push(PolicySell {
                    pct_of_original: pct,
                    pct_of_held: 0.0,
                    rung: if one_shot {
                        99
                    } else {
                        90 + (b.hits.min(8) as i64)
                    },
                    reason: format!(
                        "{}trail: {}% below peak {}{tail}",
                        if armed { "ARMED " } else { "" },
                        g(gb),
                        g(b.peak)
                    ),
                });
                if !b.exited {
                    b.peak = price; // re-arm from the hit
                }
            }
        }
    }

    if remaining_before > 0.0 {
        for s in &mut sells {
            s.pct_of_held = 100.0 * s.pct_of_original / remaining_before;
        }
    }
    (b, sells)
}

/// One line describing where a coin stands under the policy.
///
/// The tranches taken print as a list, `['4x', '8x']`, or `none`.
pub fn describe(sym: &str, b: &BullState, cost: f64, cfg: &SellPolicyConfig) -> String {
    let done = if b.tranches.is_empty() {
        "none".to_string()
    } else {
        let items: Vec<String> = b.tranches.iter().map(|t| format!("'{}x'", g(*t))).collect();
        format!("[{}]", items.join(", "))
    };
    if !cfg.has_base_trail(sym) {
        return format!(
            "{sym}: bull policy, no base trail; armed one-shot exit {}% below peak when froth \
             signals or the manual arm file fire; peak {}, remaining {}%",
            g(cfg.armed_giveback_pct),
            g(b.peak),
            g(b.remaining)
        );
    }
    if !b.trail_on {
        let arm = cfg.trail_arm_for(sym);
        return format!(
            "{sym}: bull policy, trail not armed (arms at {}x cost = {}), \
             remaining {}%, tranches done {done}",
            g(arm),
            g(cost * arm),
            g(b.remaining)
        );
    }
    let gb = b.gb.filter(|v| *v != 0.0).unwrap_or(cfg.giveback_pct);
    let lvl = b.peak * (1.0 - gb / 100.0);
    format!(
        "{sym}: bull policy, peak {}, trail {}% -> exit below {}, \
         remaining {}% of original, tranches done {done}{}{}",
        g(b.peak),
        g(b.gb.unwrap_or(gb)),
        g(lvl),
        g(b.remaining),
        if b.exited { ", EXITED to core" } else { "" },
        if b.held_below_floor {
            ", trail hit but held: below cost+floor"
        } else {
            ""
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: f64 = 86_400.0;
    const T0: f64 = 1_700_000_000.0;

    fn cfg() -> SellPolicyConfig {
        SellPolicyConfig::default()
    }

    fn run(
        price: f64,
        prev: Option<&BullState>,
        armed: bool,
        now: f64,
        c: &SellPolicyConfig,
    ) -> (BullState, Vec<PolicySell>) {
        decide(
            "AAA",
            Some(price),
            Some(100.0),
            prev,
            10.0,
            armed,
            Some(now),
            c,
        )
    }

    #[test]
    fn tranches_fire_once_each_at_their_multiple() {
        let c = cfg();
        let (b1, s1) = run(399.0, None, false, T0, &c);
        assert!(s1.is_empty(), "just under 4x takes nothing");

        let (b2, s2) = run(400.0, Some(&b1), false, T0 + DAY, &c);
        assert_eq!(s2.len(), 1, "4x takes the first tranche");
        assert_eq!(s2[0].pct_of_original, 20.0);
        assert_eq!(b2.remaining, 80.0);

        let (b3, s3) = run(420.0, Some(&b2), false, T0 + 2.0 * DAY, &c);
        assert!(s3.is_empty(), "the same multiple never fires twice");
        assert_eq!(b3.remaining, 80.0);
    }

    #[test]
    fn a_jump_past_several_multiples_takes_each_of_them() {
        let c = cfg();
        let (b, s) = run(1700.0, None, false, T0, &c); // 17x: past 4, 8 and 16
        assert_eq!(s.len(), 3, "three tranches in one run");
        assert_eq!(b.remaining, 40.0);
        assert_eq!(b.tranches, vec![4.0, 8.0, 16.0]);
    }

    #[test]
    fn tranches_stop_at_the_core() {
        let c = cfg();
        let (b, _) = run(10_000.0, None, false, T0, &c); // 100x: every multiple
        assert!(
            b.remaining >= c.core_pct,
            "remaining {} >= core {}",
            b.remaining,
            c.core_pct
        );
        assert_eq!(
            b.remaining, 20.0,
            "four tranches of 20% stop exactly at the core"
        );
    }

    #[test]
    fn the_trail_arms_at_the_multiple_then_sells_a_slice_on_the_give_back() {
        let c = cfg();
        let (b1, _) = run(150.0, None, false, T0, &c);
        assert!(!b1.trail_on, "1.5x has not armed the trail");

        let (b2, _) = run(300.0, Some(&b1), false, T0 + DAY, &c);
        assert!(b2.trail_on, "3x arms it");
        assert_eq!(b2.peak, 300.0);

        // 40% below the 300 peak is 180. At 175 the trail hits.
        let (b3, s3) = run(175.0, Some(&b2), false, T0 + 2.0 * DAY, &c);
        assert_eq!(s3.len(), 1, "the first trail hit");
        assert_eq!(s3[0].pct_of_original, 25.0, "the first slice");
        assert_eq!(b3.hits, 1);
        assert_eq!(b3.peak, 175.0, "and it re-arms from the hit");
    }

    #[test]
    fn the_second_hit_needs_less_room_and_takes_the_rest() {
        let c = cfg();
        let (b1, _) = run(300.0, None, false, T0, &c);
        let (b2, _) = run(175.0, Some(&b1), false, T0 + DAY, &c);
        assert_eq!(b2.hits, 1);
        assert_eq!(b2.gb, Some(40.0));

        // Next day the give-back is the tighter 20%: 20% below 175 is 140.
        let (b3, s3) = run(139.0, Some(&b2), false, T0 + 2.0 * DAY, &c);
        assert_eq!(
            b3.gb,
            Some(20.0),
            "the fall is confirmed, so it sells harder"
        );
        assert_eq!(s3.len(), 1);
        assert!(
            b3.exited,
            "trail_slice_next_pct of 0 means everything down to the core"
        );
        assert!((b3.remaining - c.core_pct).abs() < 1e-9);
    }

    #[test]
    fn the_trail_is_evaluated_once_per_utc_day() {
        let c = cfg();
        let (b1, _) = run(300.0, None, false, T0, &c);
        // 40% below the 300 peak is 180, and 150 also clears the cost+10% floor of 110,
        // so the only thing that can stop this sell is the once-a-day gate.
        let (b2, s2) = run(150.0, Some(&b1), false, T0 + 60.0, &c);
        assert!(
            s2.is_empty(),
            "a second run the same UTC day does not evaluate the trail"
        );
        assert_eq!(b2.peak, 300.0, "and does not move the peak");

        let (_, s3) = run(150.0, Some(&b2), false, T0 + DAY, &c);
        assert_eq!(s3.len(), 1, "the next day it does");
    }

    #[test]
    fn nothing_is_ever_sold_below_cost_plus_the_floor() {
        let c = cfg();
        // Arm the trail high, then collapse below cost+10%.
        let (b1, _) = run(300.0, None, false, T0, &c);
        let (b2, s2) = run(105.0, Some(&b1), false, T0 + DAY, &c);
        assert!(s2.is_empty(), "the trail hit but the floor wins");
        assert!(b2.held_below_floor, "and it says so");
        assert_eq!(b2.remaining, 100.0, "nothing was sold");

        let (b3, _) = run(400.0, Some(&b2), false, T0 + 2.0 * DAY, &c);
        assert!(!b3.held_below_floor, "recovering above the floor clears it");
    }

    #[test]
    fn an_armed_exit_is_a_one_shot_to_the_core_at_any_multiple() {
        let c = cfg();
        // Only 1.5x: below the 2x base arm, so the arm is what armed it.
        let (b1, _) = run(150.0, None, true, T0, &c);
        assert!(b1.trail_on, "an arm IS the arming");
        // Armed give-back is 15%: 15% below the 150 peak is 127.5, and 127 still clears
        // the cost+10% floor of 110, so the exit is allowed to happen.
        let (b2, s2) = run(127.0, Some(&b1), true, T0 + DAY, &c);
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].rung, 99, "a one-shot exit");
        assert!(b2.exited);
        assert!(
            (b2.remaining - c.core_pct).abs() < 1e-9,
            "straight to the core"
        );
    }

    #[test]
    fn a_coin_with_no_base_trail_only_exits_when_armed() {
        let c = SellPolicyConfig {
            no_base_trail: vec!["AAA".into()],
            ..cfg()
        };
        let (b1, _) = run(300.0, None, false, T0, &c);
        let (b2, s2) = run(150.0, Some(&b1), false, T0 + DAY, &c);
        assert!(
            s2.is_empty(),
            "50% off the peak and it still holds without an arm"
        );
        assert_eq!(b2.remaining, 100.0);

        let (_, s3) = run(150.0, Some(&b2), true, T0 + 2.0 * DAY, &c);
        assert_eq!(s3.len(), 1, "the arm exits it");
    }

    #[test]
    fn a_coin_excluded_from_tranches_takes_none() {
        let c = SellPolicyConfig {
            no_tranche: vec!["AAA".into()],
            ..cfg()
        };
        let (b, s) = run(1700.0, None, false, T0, &c);
        assert!(
            s.iter().all(|x| x.rung >= 90),
            "no tranche rungs, only trail"
        );
        assert!(b.tranches.is_empty());
    }

    #[test]
    fn pct_of_held_is_measured_against_what_was_held_before_the_run() {
        let c = cfg();
        let (b1, _) = run(400.0, None, false, T0, &c); // takes 20 of 100 -> 80 left
        assert_eq!(b1.remaining, 80.0);
        let (_, s2) = run(800.0, Some(&b1), false, T0 + DAY, &c);
        assert_eq!(s2[0].pct_of_original, 20.0);
        assert!(
            (s2[0].pct_of_held - 25.0).abs() < 1e-9,
            "20% of the original is 25% of the 80% still held, got {}",
            s2[0].pct_of_held
        );
    }

    #[test]
    fn missing_price_or_cost_decides_nothing() {
        let c = cfg();
        let (_, s1) = decide("AAA", None, Some(100.0), None, 10.0, false, Some(T0), &c);
        assert!(s1.is_empty());
        let (_, s2) = decide("AAA", Some(400.0), None, None, 10.0, false, Some(T0), &c);
        assert!(s2.is_empty(), "no cost basis means no policy");
    }

    #[test]
    fn describe_says_where_the_coin_stands() {
        let c = cfg();
        let (b, _) = run(300.0, None, false, T0, &c);
        let line = describe("AAA", &b, 100.0, &c);
        assert!(line.contains("peak"), "{line}");
        assert!(line.contains("exit below"), "{line}");
    }
}
