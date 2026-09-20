//! Market regime and per-coin strength — the steering layer that sits *above* the ladder.
//!
//! The ladder on its own is regime-blind: it buys a 10% dip in a bull market exactly the
//! way it buys one in a bear. This module answers the two questions that change what the
//! ladder should do with that dip:
//!
//! * **What market is this?** `bull`, `chop` or `bear`, from BTC's trend plus breadth.
//! * **Is this coin running?** A coin in a strong uptrend wants its profits trailed, not
//!   harvested at the first rung.
//!
//! Pure: it takes price series and returns verdicts. Fetching candles is the CLI's job,
//! which is what keeps this compiling to `wasm32`.

use serde::{Deserialize, Serialize};

/// Daily candles needed for the readings here — enough to cover the 200-day SMA.
pub const KLINE_DAYS: usize = 220;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RegimeConfig {
    /// How many of the four strength signals must agree before a coin is "running".
    pub run_min_signals: usize,
    /// The 30-day return, in percent, that counts as strong.
    pub run_ret30_min: f64,
}

impl Default for RegimeConfig {
    fn default() -> Self {
        RegimeConfig {
            run_min_signals: 3,
            run_ret30_min: 25.0,
        }
    }
}

/// The four independent reads on a coin's strength. Three of four means "running".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RunSignals {
    pub above_sma30: bool,
    pub ret30_strong: bool,
    pub fresh_30d_high: bool,
    pub higher_lows: bool,
    /// Too little history to judge. When true the others are all false and so is the verdict.
    pub insufficient_history: bool,
}

impl RunSignals {
    pub fn count(&self) -> usize {
        [
            self.above_sma30,
            self.ret30_strong,
            self.fresh_30d_high,
            self.higher_lows,
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }

    /// The signals that fired, for a human-readable line.
    pub fn named(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.above_sma30 {
            out.push("above 30d SMA");
        }
        if self.ret30_strong {
            out.push("strong 30d return");
        }
        if self.fresh_30d_high {
            out.push("fresh 30d high");
        }
        if self.higher_lows {
            out.push("higher lows");
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Market {
    Bull,
    Chop,
    Bear,
    /// Not enough history, or the candle feed failed. Never guess a label.
    Unknown,
}

impl Market {
    pub fn as_str(&self) -> &'static str {
        match self {
            Market::Bull => "bull",
            Market::Chop => "chop",
            Market::Bear => "bear",
            Market::Unknown => "unknown",
        }
    }
}

pub fn sma(vals: &[f64]) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    Some(vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Is this coin running, and which signals said so?
///
/// `ppd` is points per day: 1 for daily candles, 24 for an hourly series. Fewer than 30
/// days of history is never "running" — an unknown is not a yes.
pub fn running_from_series(closes: &[f64], ppd: usize, cfg: RegimeConfig) -> (bool, RunSignals) {
    let ppd = ppd.max(1);
    let (w30, w14, w7, w3) = (30 * ppd, 14 * ppd, 7 * ppd, 3 * ppd);
    if closes.len() < w30 + 1 {
        return (
            false,
            RunSignals {
                insufficient_history: true,
                ..Default::default()
            },
        );
    }
    let n = closes.len();
    let px = closes[n - 1];
    let win30 = &closes[n - w30..];

    let max_of = |s: &[f64]| s.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_of = |s: &[f64]| s.iter().cloned().fold(f64::INFINITY, f64::min);

    let sig = RunSignals {
        above_sma30: sma(win30).is_some_and(|m| px > m),
        ret30_strong: (px / closes[n - w30] - 1.0) * 100.0 >= cfg.run_ret30_min,
        // 0.999 rather than equality: a high is "fresh" if it is within a tick of it.
        fresh_30d_high: max_of(&closes[n - w3..]) >= max_of(win30) * 0.999,
        higher_lows: closes.len() > w14
            && min_of(&closes[n - w7..]) > min_of(&closes[n - w14..n - w7]),
        insufficient_history: false,
    };
    (sig.count() >= cfg.run_min_signals, sig)
}

/// `bull` / `chop` / `bear` from BTC's trend plus breadth.
///
/// Deliberately conservative on the bull side: BTC must be above **both** long SMAs and
/// at least half the watchlist must be above its own 30-day SMA. One coin ripping is not
/// a bull market.
pub fn market_label(btc_closes: &[f64], breadth: usize, n_coins: usize) -> Market {
    if btc_closes.len() < 200 {
        return Market::Unknown;
    }
    let n = btc_closes.len();
    let px = btc_closes[n - 1];
    let (Some(sma100), Some(sma200)) = (sma(&btc_closes[n - 100..]), sma(&btc_closes[n - 200..]))
    else {
        return Market::Unknown;
    };
    if px > sma100 && px > sma200 && breadth * 2 >= n_coins {
        return Market::Bull;
    }
    if px < sma200 && breadth * 3 <= n_coins {
        return Market::Bear;
    }
    Market::Chop
}

/// One coin's strength reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoinRegime {
    pub sym: String,
    pub running: bool,
    pub signals: RunSignals,
    pub price: Option<f64>,
    pub sma30: Option<f64>,
    /// Why this coin could not be read, if it could not be.
    pub error: Option<String>,
}

/// The whole picture for one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Regime {
    pub market: Market,
    pub btc_price: Option<f64>,
    pub btc_sma100: Option<f64>,
    pub btc_sma200: Option<f64>,
    pub breadth_above_sma30: usize,
    pub coins: Vec<CoinRegime>,
    pub epoch: f64,
}

impl Regime {
    pub fn running(&self, sym: &str) -> bool {
        self.coins.iter().any(|c| c.sym == sym && c.running)
    }

    pub fn running_syms(&self) -> Vec<&str> {
        self.coins
            .iter()
            .filter(|c| c.running)
            .map(|c| c.sym.as_str())
            .collect()
    }
}

/// Assemble a [`Regime`] from already-fetched candle series.
///
/// `series` is `(symbol, closes)`; a coin whose feed failed is passed as an empty slice
/// and reported as not running rather than silently dropped.
pub fn assess(
    series: &[(String, Vec<f64>)],
    btc_closes: &[f64],
    cfg: RegimeConfig,
    epoch: f64,
) -> Regime {
    let mut coins = Vec::with_capacity(series.len());
    let mut breadth = 0usize;

    for (sym, closes) in series {
        if closes.is_empty() {
            coins.push(CoinRegime {
                sym: sym.clone(),
                running: false,
                signals: RunSignals {
                    insufficient_history: true,
                    ..Default::default()
                },
                price: None,
                sma30: None,
                error: Some("no candles".into()),
            });
            continue;
        }
        let (running, signals) = running_from_series(closes, 1, cfg);
        let n = closes.len();
        let sma30 = if n >= 30 {
            sma(&closes[n - 30..])
        } else {
            None
        };
        if sma30.is_some_and(|m| closes[n - 1] > m) {
            breadth += 1;
        }
        coins.push(CoinRegime {
            sym: sym.clone(),
            running,
            signals,
            price: Some(closes[n - 1]),
            sma30,
            error: None,
        });
    }

    let market = market_label(btc_closes, breadth, coins.len());
    let bn = btc_closes.len();
    Regime {
        market,
        btc_price: btc_closes.last().copied(),
        btc_sma100: (bn >= 100).then(|| sma(&btc_closes[bn - 100..])).flatten(),
        btc_sma200: (bn >= 200).then(|| sma(&btc_closes[bn - 200..])).flatten(),
        breadth_above_sma30: breadth,
        coins,
        epoch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: RegimeConfig = RegimeConfig {
        run_min_signals: 3,
        run_ret30_min: 25.0,
    };

    fn rising(n: usize, from: f64, to: f64) -> Vec<f64> {
        (0..n)
            .map(|i| from + (to - from) * i as f64 / (n - 1) as f64)
            .collect()
    }

    fn falling(n: usize, from: f64, to: f64) -> Vec<f64> {
        rising(n, to, from).into_iter().rev().collect()
    }

    #[test]
    fn too_little_history_is_never_running() {
        let (running, sig) = running_from_series(&rising(20, 100.0, 200.0), 1, C);
        assert!(!running, "an unknown is not a yes");
        assert!(sig.insufficient_history);
        assert_eq!(sig.count(), 0);
    }

    #[test]
    fn a_steady_climb_is_running() {
        let (running, sig) = running_from_series(&rising(60, 100.0, 200.0), 1, C);
        assert!(running, "signals fired: {:?}", sig.named());
        assert!(sig.above_sma30 && sig.ret30_strong && sig.fresh_30d_high);
    }

    #[test]
    fn a_steady_decline_is_not_running() {
        let (running, sig) = running_from_series(&falling(60, 200.0, 100.0), 1, C);
        assert!(!running, "signals fired: {:?}", sig.named());
        assert!(!sig.above_sma30);
        assert!(!sig.ret30_strong);
    }

    #[test]
    fn a_flat_market_is_not_running() {
        let (running, sig) = running_from_series(&vec![100.0; 60], 1, C);
        assert!(!running);
        assert!(!sig.ret30_strong, "0% is not a strong 30d return");
        assert!(!sig.above_sma30, "exactly at the mean is not above it");
    }

    #[test]
    fn the_signal_threshold_is_configurable() {
        // The signal reads the last 30 bars, not the whole series: a run that gains
        // 20% over 60 days has only gained about 8.9% across its final 30.
        let closes = rising(60, 100.0, 120.0);
        let (_, sig) = running_from_series(&closes, 1, C);
        assert!(
            !sig.ret30_strong,
            "8.9% over the last 30d is under the 25% bar"
        );
        let loose = RegimeConfig {
            run_ret30_min: 5.0,
            ..C
        };
        let (_, sig2) = running_from_series(&closes, 1, loose);
        assert!(sig2.ret30_strong, "and over a 5% bar");
    }

    #[test]
    fn hourly_series_are_read_with_ppd() {
        // 60 days of hourly data is plenty; the same series read as daily is not.
        let hourly = rising(60 * 24, 100.0, 200.0);
        let (running, _) = running_from_series(&hourly, 24, C);
        assert!(running, "60 days of hourly candles is 60 days");
    }

    #[test]
    fn market_label_needs_two_hundred_candles() {
        assert_eq!(market_label(&rising(150, 1.0, 2.0), 5, 6), Market::Unknown);
    }

    #[test]
    fn a_bull_needs_btc_trend_and_breadth_together() {
        let up = rising(220, 10_000.0, 90_000.0);
        assert_eq!(
            market_label(&up, 3, 6),
            Market::Bull,
            "BTC up and half the book up"
        );
        assert_eq!(
            market_label(&up, 1, 6),
            Market::Chop,
            "BTC up but the book is not: one coin ripping is not a bull market"
        );
    }

    #[test]
    fn a_bear_needs_btc_below_the_long_sma_and_a_weak_book() {
        let down = falling(220, 90_000.0, 10_000.0);
        assert_eq!(market_label(&down, 0, 6), Market::Bear);
        assert_eq!(
            market_label(&down, 3, 6),
            Market::Chop,
            "a broad book is not a bear"
        );
    }

    #[test]
    fn assess_counts_breadth_and_survives_a_dead_feed() {
        let series = vec![
            ("AAA".to_string(), rising(60, 100.0, 200.0)),
            ("BBB".to_string(), falling(60, 200.0, 100.0)),
            ("CCC".to_string(), Vec::new()), // feed failed
        ];
        let r = assess(
            &series,
            &rising(220, 10_000.0, 90_000.0),
            C,
            1_700_000_000.0,
        );
        assert_eq!(
            r.breadth_above_sma30, 1,
            "only AAA is above its own 30d SMA"
        );
        assert!(r.running("AAA"));
        assert!(!r.running("BBB"));
        assert!(!r.running("CCC"), "a dead feed fails safe to not-running");
        assert_eq!(r.coins[2].error.as_deref(), Some("no candles"));
        assert_eq!(r.running_syms(), vec!["AAA"]);
        assert!(r.btc_sma200.is_some());
    }
}
