//! The ladder itself: prices plus prior state become the trades this run would make.
//!
//! [`analyze`] is a pure function of `(config, prices, state, now)`. Same inputs, same
//! output, forever — which is why the golden test can pin the strategy's behaviour
//! across a rewrite, and across languages.
//!
//! Five rules run here, and they are the whole strategy:
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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{Config, Trail};
use crate::ladder::{buy_rung_for, ladder_increment, rung_threshold, sell_rung_for};

/// A price observation for one coin. `chg_24h` of `None` holds that coin's buy ladder.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub price: f64,
    #[serde(default)]
    pub chg_24h: Option<f64>,
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
}

/// What one run of the ladder decided.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub buys: Vec<Trade>,
    pub sells: Vec<Trade>,
    pub rows: Vec<Row>,
    pub errors: Vec<String>,
    pub state: State,
}

/// Sort helper matching the reference implementation's `-999` sentinel for missing data.
fn or_sentinel(v: Option<f64>) -> f64 {
    v.unwrap_or(-999.0)
}

fn cmp_desc(a: f64, b: f64) -> core::cmp::Ordering {
    b.partial_cmp(&a).unwrap_or(core::cmp::Ordering::Equal)
}

fn cmp_asc(a: f64, b: f64) -> core::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(core::cmp::Ordering::Equal)
}

/// Run the ladder. `now` is epoch seconds; the core never reads a clock itself.
pub fn analyze(cfg: &Config, prices: &BTreeMap<String, Price>, state: &State, now: f64) -> Outcome {
    let s = cfg.settings;
    let mut buys: Vec<Trade> = Vec::new();
    let mut sells: Vec<Trade> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
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

        // Cost basis starts from the config's `entry` and is then carried in state, so a
        // later run keeps using it even if the config is edited.
        let cost = prev.cost_basis.filter(|v| *v != 0.0).or(coin.entry);
        let pnl = match (cost, price) {
            (Some(c), p) if c != 0.0 && p != 0.0 => Some(((p / c) - 1.0) * 100.0),
            _ => None,
        };

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
            trailing: s.trail == Trail::On,
        };

        let mut deployed = prev.deployed_pct;
        let mut sold = prev.sold_pct;
        let mut win_until = prev.win_until;
        let mut win_dir = prev.win_dir.clone();

        // Rule 2. While the window is open only its direction may act. Once it closes,
        // re-arm: clear the direction and reset both ladders so a fresh move can open.
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

        // Rule 5. Continuously <= -breaker_pct vs cost basis for breaker_days freezes
        // BUYS for this coin and says so once. It clears itself on recovery.
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

        // --- BUY side: 24h dips, dynamic cap, stopped past the knife floor, and frozen
        //     while a SELL window is open (no direction flip within the window) ---
        match buy_rung_for(chg, bands) {
            None => {} // 24h data missing -> hold the ladder
            Some(0) => {
                if !window_open {
                    buy_hw = 0; // back in the neutral band, re-arm
                }
            }
            Some(br) => {
                let knifed = chg.is_some_and(|c| c <= -s.buy_floor_pct);
                if knifed || win_dir == "sell" || breaker {
                    // knife floor, sell-locked, or breaker: no buy, ladder unchanged
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
                        });
                        win_dir = "buy".to_string();
                        win_until = now + win_secs;
                    }
                    buy_hw = br;
                }
            }
        }

        // --- SELL side: profit vs cost basis, protected core, frozen while a BUY
        //     window is open ---
        let sr = sell_rung_for(pnl, bands);
        let mut peak_pnl = if window_open { prev.peak_pnl } else { 0.0 };
        let trailing = s.trail == Trail::On;
        match sr {
            None => {} // no entry or no price -> cannot judge
            Some(0) => {
                if !window_open {
                    sell_hw = 0; // below the first target above entry
                    peak_pnl = 0.0;
                }
            }
            Some(sr) if win_dir == "buy" => {
                let _ = sr; // buy-locked this window
            }
            Some(sr) => {
                // Not trailing (the default): fire every newly crossed rung now.
                // Trailing: rung 1 still fires immediately to lock the first slice;
                // upper rungs are held while the move runs and fire together once P&L
                // gives back `trail_giveback_pct` from the episode peak.
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
                        });
                        // NB: we do NOT subtract a position-% from a bag-%. Selling frees
                        // the bag via the actual stable balance on the next run; the two
                        // ledgers stay in their own units.
                        win_dir = "sell".to_string();
                        win_until = now + win_secs;
                    }
                    sell_hw = fire_to;
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
            },
        );

        row.committed_pct = deployed;
        row.cap_now_pct = cap_pct;
        row.over_budget = deployed > cap_pct + 1e-9;
        row.win_dir = if window_open { win_dir } else { String::new() };
        row.breaker = breaker;
        rows.push(row);
    }

    rows.sort_by(|a, b| cmp_desc(or_sentinel(a.chg), or_sentinel(b.chg)));
    buys.sort_by(|a, b| cmp_asc(or_sentinel(a.row.chg), or_sentinel(b.row.chg)));
    sells.sort_by(|a, b| cmp_desc(or_sentinel(a.row.pnl), or_sentinel(b.row.pnl)));

    Outcome {
        buys,
        sells,
        rows,
        errors,
        state: new_state,
    }
}
