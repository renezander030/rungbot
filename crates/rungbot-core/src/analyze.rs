//! The ladder itself: prices plus prior state become the trades this run would make.
//!
//! [`analyze`] is a pure function of `(config, prices, state, steer, now)`. Same inputs,
//! same output, forever — which is why the golden test can pin the strategy's behaviour
//! across a rewrite, and across languages.
//!
//! Five rules run here, and they are the whole ladder:
//!
//! 1. **High-water rungs.** A rung fires once. A retrace never re-fires it; only a
//!    deeper move advances the ladder.
//! 2. **Directional window.** Once a coin buys, it may only keep buying until the
//!    window closes, and vice versa. No flip-flopping inside a day.
//! 3. **Dynamic cap.** Each coin's cumulative buy budget breathes with its own P&L, so
//!    a coin in freefall can only ever burn its own shrinking slice.
//! 4. **Protected core.** Sells stop at `min_core_pct`. The ladder never sells out.
//! 5. **Circuit breaker.** A coin far underwater for long enough stops being
//!    dip-bought. It is never sold at a loss — the breaker only stops the buying.
//!
//! Above all of that sits [`Steer`]: in a confirmed bull the sell side of a coin can be
//! handed to [`crate::sellpolicy`] instead, because the ladder is a chop harvester and
//! sells a running coin far too early.
//!
//! Every suppressed trade records **why** in [`Outcome::skips`], which is what lets the
//! report answer "why did nothing happen?" instead of just printing nothing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{Config, Trail};
use crate::ladder::{buy_rung_for, ladder_increment, rung_threshold, sell_rung_for};
use crate::sellpolicy::{self, BullState, SellPolicyConfig};

/// A price observation for one coin. `chg_24h` of `None` holds that coin's buy ladder.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub price: f64,
    #[serde(default)]
    pub chg_24h: Option<f64>,
}

/// The steering layer's input to one run.
///
/// Default is "no steering": the plain ladder, exactly as it behaved before steering
/// existed. That is what keeps the golden scenario meaningful.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Steer {
    /// Present when the bull sell policy is in force for `policy_coins`.
    #[serde(default)]
    pub policy: Option<SellPolicyConfig>,
    /// Coins whose sell side the policy governs. The ladder's sell rungs go dormant.
    #[serde(default)]
    pub policy_coins: Vec<String>,
    /// Coins with an external one-shot arm (a froth read, a manual arm).
    #[serde(default)]
    pub armed: Vec<String>,
    /// Coins running hard enough that the ladder should trail rather than harvest.
    #[serde(default)]
    pub trail_coins: Vec<String>,
}

impl Steer {
    fn governs(&self, sym: &str) -> bool {
        self.policy.is_some() && self.policy_coins.iter().any(|s| s == sym)
    }

    fn is_armed(&self, sym: &str) -> bool {
        self.armed.iter().any(|s| s == sym)
    }

    fn trails(&self, sym: &str) -> bool {
        self.trail_coins.iter().any(|s| s == sym)
    }
}

/// Per-coin ladder state. Carried between runs; this is what makes a rung fire once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoinState {
    pub buy: i64,
    pub sell: i64,
    pub deployed_pct: f64,
    pub sold_pct: f64,
    pub win_until: f64,
    pub win_dir: String,
    pub cost_basis: Option<f64>,
    pub below_since: f64,
    pub breaker: bool,
    pub peak_pnl: f64,
    /// The bull policy's memory for this coin, when the policy governs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bull: Option<BullState>,
}

impl Default for CoinState {
    fn default() -> Self {
        CoinState {
            buy: 0,
            sell: 0,
            deployed_pct: 0.0,
            sold_pct: 0.0,
            win_until: 0.0,
            win_dir: String::new(),
            cost_basis: None,
            below_since: 0.0,
            breaker: false,
            peak_pnl: 0.0,
            bull: None,
        }
    }
}

/// The whole book's ladder state. A `BTreeMap` so serialisation is key-ordered.
pub type State = BTreeMap<String, CoinState>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

/// Why a coin did nothing this run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SkipReason {
    NoNewRung,
    NoData,
    NoCostBasis,
    KnifeFloor { chg: f64, floor: f64 },
    WindowLocked { dir: String },
    Breaker { pnl: f64, days: f64 },
    CapReached { deployed: f64, cap: f64 },
    BelowMinTrade { want: f64, min: f64 },
    CoreProtected { sold: f64, core: f64 },
    PolicyGoverns,
}

impl SkipReason {
    /// How informative this reason is. A breaker tells you more than "no new rung".
    fn rank(&self) -> u8 {
        match self {
            SkipReason::NoNewRung => 0,
            SkipReason::NoData | SkipReason::NoCostBasis => 1,
            SkipReason::WindowLocked { .. } | SkipReason::PolicyGoverns => 2,
            SkipReason::CoreProtected { .. }
            | SkipReason::BelowMinTrade { .. }
            | SkipReason::CapReached { .. } => 3,
            SkipReason::KnifeFloor { .. } => 4,
            SkipReason::Breaker { .. } => 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skip {
    pub sym: String,
    #[serde(flatten)]
    pub reason: SkipReason,
}

/// Keep the most informative reason seen for a coin.
fn note(slot: &mut Option<SkipReason>, r: SkipReason) {
    if slot.as_ref().is_none_or(|cur| r.rank() >= cur.rank()) {
        *slot = Some(r);
    }
}

/// One coin's line in the full report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub sym: String,
    pub name: String,
    pub venue: String,
    pub pair: String,
    pub price: f64,
    pub chg: Option<f64>,
    pub entry: Option<f64>,
    pub pnl: Option<f64>,
    pub target: Option<f64>,
    pub committed_pct: f64,
    pub cap_now_pct: f64,
    pub over_budget: bool,
    pub win_dir: String,
    pub breaker: bool,
    pub trailing: bool,
    /// Set when the bull policy governs this coin: where it stands, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
}

/// A coin that crossed a new rung this run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    #[serde(flatten)]
    pub row: Row,
    pub side: Side,
    pub rung: i64,
    pub new_rungs: Vec<i64>,
    pub threshold: f64,
    /// % of the coin's base share (buy) or of the held position (sell).
    pub pct: f64,
    /// The running ledger after this trade: deployed (buy) or sold (sell).
    pub ledger_pct: f64,
    /// Buys only: the dynamic cap this trade was measured against.
    pub cap_pct: Option<f64>,
    pub capped: bool,
    /// Set when the bull policy produced this sell rather than the ladder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_reason: Option<String>,
}

/// What one run of the ladder decided.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub buys: Vec<Trade>,
    pub sells: Vec<Trade>,
    pub rows: Vec<Row>,
    pub errors: Vec<String>,
    pub skips: Vec<Skip>,
    pub state: State,
}

fn or_sentinel(v: Option<f64>) -> f64 {
    v.unwrap_or(-999.0)
}

fn cmp_desc(a: f64, b: f64) -> core::cmp::Ordering {
    b.partial_cmp(&a).unwrap_or(core::cmp::Ordering::Equal)
}

fn cmp_asc(a: f64, b: f64) -> core::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(core::cmp::Ordering::Equal)
}

/// Run the plain ladder with no steering. Equivalent to [`analyze_with`] and
/// [`Steer::default`].
pub fn analyze(cfg: &Config, prices: &BTreeMap<String, Price>, state: &State, now: f64) -> Outcome {
    analyze_with(cfg, prices, state, &Steer::default(), now)
}

/// Run the ladder under a steering layer. `now` is epoch seconds; the core never reads
/// a clock itself.
pub fn analyze_with(
    cfg: &Config,
    prices: &BTreeMap<String, Price>,
    state: &State,
    steer: &Steer,
    now: f64,
) -> Outcome {
    let s = cfg.settings;
    let mut buys: Vec<Trade> = Vec::new();
    let mut sells: Vec<Trade> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut skips: Vec<Skip> = Vec::new();
    let mut new_state: State = state.clone();

    for coin in &cfg.coins {
        let sym = coin.symbol.as_str();
        let Some(observed) = prices.get(sym) else {
            errors.push(format!(
                "{sym} ({}:{}): missing from venue ticker",
                coin.venue.as_str(),
                coin.pair
            ));
            continue;
        };

        let bands = coin.bands_or(s.bands);
        let price = observed.price;
        let chg = observed.chg_24h;
        let prev = new_state.get(sym).cloned().unwrap_or_default();
        let mut skip: Option<SkipReason> = None;
        let acted_before = buys.len() + sells.len();

        // Cost basis starts from the config's `entry` and is then carried in state, so a
        // later run keeps using it even if the config is edited.
        let cost = prev.cost_basis.filter(|v| *v != 0.0).or(coin.entry);
        let pnl = match (cost, price) {
            (Some(c), p) if c != 0.0 && p != 0.0 => Some(((p / c) - 1.0) * 100.0),
            _ => None,
        };

        let trailing = s.trail == Trail::On || steer.trails(sym);
        let mut row = Row {
            sym: sym.to_string(),
            name: coin.display_name().to_string(),
            venue: coin.venue.as_str().to_string(),
            pair: coin.pair.clone(),
            price,
            chg,
            entry: cost,
            pnl,
            target: cost.map(|c| c * (1.0 + s.target_pct / 100.0)),
            committed_pct: 0.0,
            cap_now_pct: 0.0,
            over_budget: false,
            win_dir: String::new(),
            breaker: false,
            trailing,
            policy: None,
        };

        let mut deployed = prev.deployed_pct;
        let mut sold = prev.sold_pct;
        let mut win_until = prev.win_until;
        let mut win_dir = prev.win_dir.clone();
        let mut bull = prev.bull.clone();
        let mut peak_pnl = if now < prev.win_until {
            prev.peak_pnl
        } else {
            0.0
        };

        // Rule 2. While the window is open only its direction may act.
        let window_open = now < win_until;
        let (mut buy_hw, mut sell_hw) = if window_open {
            (prev.buy, prev.sell)
        } else {
            win_dir = String::new();
            (0, 0)
        };
        let win_secs = s.window_hours * 3600.0;

        // Rule 3. Per-coin buy ceiling as a % of its base share, breathing with P&L.
        let cap_pct = (100.0 + pnl.unwrap_or(0.0)).max(0.0);

        // Rule 5. Continuously <= -breaker_pct for breaker_days freezes BUYS.
        let mut below_since = prev.below_since;
        if s.breaker_pct > 0.0 && pnl.is_some_and(|p| p <= -s.breaker_pct) {
            if below_since == 0.0 {
                below_since = now;
            }
        } else if pnl.is_some() {
            below_since = 0.0;
        }
        let breaker = below_since > 0.0 && (now - below_since) >= s.breaker_days * 86400.0;
        if breaker && !prev.breaker {
            errors.push(format!(
                "{sym}: CIRCUIT BREAKER -- {:+.1}% vs entry for >= {:.0}d; \
                 dip-buys frozen until it recovers above -{:.0}%",
                pnl.unwrap_or(0.0),
                s.breaker_days,
                s.breaker_pct
            ));
        }

        // --- The bull sell policy decides first. A coin the policy sells this run is
        //     never dip-bought in the same run. ---
        let mut policy_sells = Vec::new();
        let governed = steer.governs(sym);
        if governed {
            let pcfg = steer.policy.as_ref().expect("governs() checked it");
            let (next, decided) = sellpolicy::decide(
                sym,
                Some(price),
                cost,
                bull.as_ref(),
                bands.first_pct,
                steer.is_armed(sym),
                Some(now),
                pcfg,
            );
            policy_sells = decided
                .into_iter()
                .filter(|d| d.pct_of_held >= s.min_trade_pct)
                .collect();
            if let Some(c) = cost {
                row.policy = Some(sellpolicy::describe(sym, &next, c, pcfg));
            }
            bull = Some(next);
        }

        // --- BUY side ---
        match buy_rung_for(chg, bands) {
            None => note(&mut skip, SkipReason::NoData),
            Some(0) => {
                if !window_open {
                    buy_hw = 0; // back in the neutral band, re-arm
                }
                note(&mut skip, SkipReason::NoNewRung);
            }
            Some(br) => {
                let knifed = chg.is_some_and(|c| c <= -s.buy_floor_pct);
                if knifed {
                    note(
                        &mut skip,
                        SkipReason::KnifeFloor {
                            chg: chg.unwrap_or(0.0),
                            floor: s.buy_floor_pct,
                        },
                    );
                } else if win_dir == "sell" {
                    note(&mut skip, SkipReason::WindowLocked { dir: "sell".into() });
                } else if breaker {
                    note(
                        &mut skip,
                        SkipReason::Breaker {
                            pnl: pnl.unwrap_or(0.0),
                            days: s.breaker_days,
                        },
                    );
                } else if !policy_sells.is_empty() {
                    note(&mut skip, SkipReason::PolicyGoverns);
                } else if br > buy_hw {
                    // Rule 1: advance only.
                    let new_rungs: Vec<i64> = (buy_hw + 1..=br).collect();
                    let want = ladder_increment(&new_rungs, bands);
                    let allowed = want.min(cap_pct - deployed).max(0.0);
                    if allowed >= s.min_trade_pct {
                        deployed += allowed;
                        buys.push(Trade {
                            row: row.clone(),
                            side: Side::Buy,
                            rung: br,
                            threshold: rung_threshold(br, bands),
                            new_rungs,
                            pct: allowed,
                            ledger_pct: deployed,
                            cap_pct: Some(cap_pct),
                            capped: allowed < want || deployed >= cap_pct,
                            policy_reason: None,
                        });
                        win_dir = "buy".to_string();
                        win_until = now + win_secs;
                    } else if cap_pct - deployed < want {
                        note(
                            &mut skip,
                            SkipReason::CapReached {
                                deployed,
                                cap: cap_pct,
                            },
                        );
                    } else {
                        note(
                            &mut skip,
                            SkipReason::BelowMinTrade {
                                want,
                                min: s.min_trade_pct,
                            },
                        );
                    }
                    buy_hw = br;
                } else {
                    note(&mut skip, SkipReason::NoNewRung);
                }
            }
        }

        // --- SELL side ---
        if governed {
            // The policy owns this coin's sell side; the ladder's rungs are dormant.
            for ps in &policy_sells {
                sells.push(Trade {
                    row: row.clone(),
                    side: Side::Sell,
                    rung: ps.rung,
                    threshold: rung_threshold(1, bands),
                    new_rungs: Vec::new(),
                    pct: ps.pct_of_held,
                    ledger_pct: sold,
                    cap_pct: None,
                    capped: false,
                    policy_reason: Some(ps.reason.clone()),
                });
                win_dir = "sell".to_string();
                win_until = now + win_secs;
            }
            if policy_sells.is_empty() {
                note(&mut skip, SkipReason::PolicyGoverns);
            }
        } else {
            let sr = sell_rung_for(pnl, bands);
            match sr {
                None => note(
                    &mut skip,
                    if cost.is_none() {
                        SkipReason::NoCostBasis
                    } else {
                        SkipReason::NoData
                    },
                ),
                Some(0) => {
                    if !window_open {
                        sell_hw = 0;
                        peak_pnl = 0.0;
                    }
                    note(&mut skip, SkipReason::NoNewRung);
                }
                Some(_) if win_dir == "buy" => {
                    note(&mut skip, SkipReason::WindowLocked { dir: "buy".into() });
                }
                Some(sr) => {
                    let mut fire_to = 0;
                    if !trailing {
                        if sr > sell_hw {
                            fire_to = sr;
                        }
                    } else {
                        peak_pnl = peak_pnl.max(pnl.unwrap_or(0.0));
                        if sell_hw == 0 {
                            fire_to = 1;
                        } else {
                            let peak_rung = sell_rung_for(Some(peak_pnl), bands).unwrap_or(0);
                            if peak_rung > sell_hw
                                && pnl.is_some_and(|p| p <= peak_pnl - s.trail_giveback_pct)
                            {
                                fire_to = peak_rung; // give-back: harvest the run
                            }
                        }
                    }
                    if fire_to > sell_hw {
                        let new_rungs: Vec<i64> = (sell_hw + 1..=fire_to).collect();
                        let want = ladder_increment(&new_rungs, bands);
                        let sellable = ((100.0 - s.min_core_pct) - sold).max(0.0); // Rule 4
                        let allowed = want.min(sellable);
                        if allowed >= s.min_trade_pct {
                            sold += allowed;
                            sells.push(Trade {
                                row: row.clone(),
                                side: Side::Sell,
                                rung: fire_to,
                                threshold: rung_threshold(fire_to, bands),
                                new_rungs,
                                pct: allowed,
                                ledger_pct: sold,
                                cap_pct: None,
                                capped: allowed < want,
                                policy_reason: None,
                            });
                            win_dir = "sell".to_string();
                            win_until = now + win_secs;
                        } else if sellable < want {
                            note(
                                &mut skip,
                                SkipReason::CoreProtected {
                                    sold,
                                    core: s.min_core_pct,
                                },
                            );
                        } else {
                            note(
                                &mut skip,
                                SkipReason::BelowMinTrade {
                                    want,
                                    min: s.min_trade_pct,
                                },
                            );
                        }
                        sell_hw = fire_to;
                    } else {
                        note(&mut skip, SkipReason::NoNewRung);
                    }
                }
            }
        }

        new_state.insert(
            sym.to_string(),
            CoinState {
                buy: buy_hw,
                sell: sell_hw,
                deployed_pct: deployed,
                sold_pct: sold,
                win_until,
                win_dir: win_dir.clone(),
                cost_basis: cost,
                below_since,
                breaker,
                peak_pnl,
                bull: bull.clone(),
            },
        );

        row.committed_pct = deployed;
        row.cap_now_pct = cap_pct;
        row.over_budget = deployed > cap_pct + 1e-9;
        row.win_dir = if window_open { win_dir } else { String::new() };
        row.breaker = breaker;
        rows.push(row);

        if buys.len() + sells.len() == acted_before {
            skips.push(Skip {
                sym: sym.to_string(),
                reason: skip.unwrap_or(SkipReason::NoNewRung),
            });
        }
    }

    rows.sort_by(|a, b| cmp_desc(or_sentinel(a.chg), or_sentinel(b.chg)));
    buys.sort_by(|a, b| cmp_asc(or_sentinel(a.row.chg), or_sentinel(b.row.chg)));
    sells.sort_by(|a, b| cmp_desc(or_sentinel(a.row.pnl), or_sentinel(b.row.pnl)));

    Outcome {
        buys,
        sells,
        rows,
        errors,
        skips,
        state: new_state,
    }
}
