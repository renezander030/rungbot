//! Market-cycle indicators, per coin.
//!
//! Where [`crate::regime`] answers "what market is this, right now", this module answers
//! "where in its cycle is this coin" — the slow, structural reads you want before
//! deciding whether a 30% drawdown is an opportunity or the start of the end.
//!
//! Every indicator is computed from one daily close series and returns `None` rather
//! than a guess when there is not enough history. A number derived from 40 candles that
//! claims to be a 350-day average is worse than no number.
//!
//! Pure: no clock, no I/O. Fetching candles is the CLI's job.
//!
//! **None of this is a trading signal.** These are context. The ladder does not read them.

use serde::{Deserialize, Serialize};

/// Candles needed for the full set. The Pi-cycle ratio alone wants 350.
pub const FULL_HISTORY_DAYS: usize = 800;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct KpiThresholds {
    /// Pi-cycle ratio at which a blow-off is close. 1.0 is the classic cross.
    pub pi_hot: f64,
    /// Mayer multiple that counts as stretched.
    pub mayer_stretched: f64,
    /// Mayer multiple of the classic cycle top.
    pub mayer_hot: f64,
    /// Weekly RSI that counts as overheated.
    pub weekly_rsi_hot: f64,
    /// Daily RSI that counts as oversold.
    pub rsi_oversold: f64,
    /// Drawdown from the series high, in percent, that counts as deeply dislocated.
    pub deep_drawdown_pct: f64,
}

impl Default for KpiThresholds {
    fn default() -> Self {
        // Documented lines in the sand, not tuned parameters. The Pi-cycle cross and the
        // 2.4 Mayer multiple are the classic cycle-top markers; 1.8 is "stretched".
        KpiThresholds {
            pi_hot: 0.95,
            mayer_stretched: 1.80,
            mayer_hot: 2.40,
            weekly_rsi_hot: 85.0,
            rsi_oversold: 30.0,
            deep_drawdown_pct: 70.0,
        }
    }
}

/// Where a coin sits in its own cycle. Context, never an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Deeply off its high and below the long average.
    Capitulation,
    /// Off its high, below the long average, but no longer falling hard.
    Accumulation,
    /// Above the long average and climbing.
    Markup,
    /// Stretched far above the long average, or the cycle markers are firing.
    Euphoria,
    /// Below the long average after a markup: the air is coming out.
    Markdown,
    /// Not enough history to say, which is a real answer.
    Unknown,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Capitulation => "capitulation",
            Phase::Accumulation => "accumulation",
            Phase::Markup => "markup",
            Phase::Euphoria => "euphoria",
            Phase::Markdown => "markdown",
            Phase::Unknown => "unknown",
        }
    }
}

/// The full per-coin indicator set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoinKpi {
    pub sym: String,
    pub price: f64,
    pub candles: usize,

    // --- cycle markers ---
    /// `SMA(111) / (2 × SMA(350))`. Crossing 1.0 has marked cycle tops.
    pub pi_cycle: Option<f64>,
    /// `price / SMA(200)`.
    pub mayer: Option<f64>,
    /// Wilder RSI(14) on daily closes.
    pub rsi14: Option<f64>,
    /// Wilder RSI(14) on completed weekly closes.
    pub weekly_rsi14: Option<f64>,

    // --- position ---
    /// Percent below the highest close in the series. `0` = at its high.
    pub drawdown_pct: Option<f64>,
    /// Where price sits between the series low and high, `0..100`.
    pub cycle_position_pct: Option<f64>,
    pub sma30: Option<f64>,
    pub sma100: Option<f64>,
    pub sma200: Option<f64>,
    /// Percent above (+) or below (-) the 200-day average.
    pub vs_sma200_pct: Option<f64>,

    // --- momentum and risk ---
    pub ret30_pct: Option<f64>,
    pub ret90_pct: Option<f64>,
    pub ret365_pct: Option<f64>,
    /// Annualised standard deviation of daily log returns over the last 30 days, percent.
    pub volatility30_pct: Option<f64>,

    pub phase: Phase,
    /// Threshold crossings worth a human's attention.
    pub flags: Vec<String>,
}

pub fn sma(vals: &[f64], n: usize) -> Option<f64> {
    if n == 0 || vals.len() < n {
        return None;
    }
    Some(vals[vals.len() - n..].iter().sum::<f64>() / n as f64)
}

/// Wilder's RSI. `None` when there is not enough history to seed it.
pub fn rsi(vals: &[f64], n: usize) -> Option<f64> {
    if n == 0 || vals.len() < n + 1 {
        return None;
    }
    let (gains, losses): (Vec<f64>, Vec<f64>) = vals
        .windows(2)
        .map(|w| ((w[1] - w[0]).max(0.0), (w[0] - w[1]).max(0.0)))
        .unzip();
    let mut ag = gains[..n].iter().sum::<f64>() / n as f64;
    let mut al = losses[..n].iter().sum::<f64>() / n as f64;
    for i in n..gains.len() {
        ag = (ag * (n - 1) as f64 + gains[i]) / n as f64;
        al = (al * (n - 1) as f64 + losses[i]) / n as f64;
    }
    Some(if al == 0.0 {
        100.0
    } else {
        100.0 - 100.0 / (1.0 + ag / al)
    })
}

/// Day number of the Thursday of this epoch's ISO week.
///
/// Every day in an ISO week shares a Thursday, so it is a stable week key without
/// needing the ISO year/week pair. 1970-01-01 was itself a Thursday.
fn iso_week_key(epoch_secs: f64) -> i64 {
    let days = (epoch_secs as i64).div_euclid(86_400);
    let dow = (days % 7 + 7 + 3) % 7; // Monday = 0
    days - dow + 3
}

/// Last close of each **completed** week, oldest first.
///
/// `stamps` are the candle open times in epoch seconds, aligned with `closes`. The
/// current partial week is dropped: a week still in progress de-arms and re-arms on
/// every run, which is a race, not a signal.
pub fn weekly_closes(closes: &[f64], stamps: &[f64], now: f64) -> Vec<f64> {
    if closes.len() != stamps.len() || closes.is_empty() {
        return Vec::new();
    }
    let mut weeks: Vec<f64> = Vec::new();
    let mut keys: Vec<i64> = Vec::new();
    for (c, t) in closes.iter().zip(stamps) {
        let k = iso_week_key(*t);
        if keys.last() == Some(&k) {
            *weeks.last_mut().expect("keys and weeks stay in step") = *c;
        } else {
            keys.push(k);
            weeks.push(*c);
        }
    }
    if keys.last() == Some(&iso_week_key(now)) {
        weeks.pop();
    }
    weeks
}

fn pct_change(from: f64, to: f64) -> Option<f64> {
    (from != 0.0).then(|| (to / from - 1.0) * 100.0)
}

/// Annualised standard deviation of daily log returns, in percent.
fn volatility(closes: &[f64], n: usize) -> Option<f64> {
    if closes.len() < n + 1 {
        return None;
    }
    let rets: Vec<f64> = closes[closes.len() - n - 1..]
        .windows(2)
        .filter(|w| w[0] > 0.0 && w[1] > 0.0)
        .map(|w| (w[1] / w[0]).ln())
        .collect();
    if rets.len() < 2 {
        return None;
    }
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
    Some(var.sqrt() * (365.0f64).sqrt() * 100.0)
}

fn nth_back(closes: &[f64], n: usize) -> Option<f64> {
    (closes.len() > n).then(|| closes[closes.len() - 1 - n])
}

/// Compute every indicator this module knows for one coin.
///
/// `stamps` are candle open times in epoch seconds; pass an empty slice to skip the
/// weekly RSI rather than fake it.
pub fn compute(sym: &str, closes: &[f64], stamps: &[f64], now: f64, t: KpiThresholds) -> CoinKpi {
    let mut k = CoinKpi {
        sym: sym.to_string(),
        price: closes.last().copied().unwrap_or(0.0),
        candles: closes.len(),
        pi_cycle: None,
        mayer: None,
        rsi14: None,
        weekly_rsi14: None,
        drawdown_pct: None,
        cycle_position_pct: None,
        sma30: None,
        sma100: None,
        sma200: None,
        vs_sma200_pct: None,
        ret30_pct: None,
        ret90_pct: None,
        ret365_pct: None,
        volatility30_pct: None,
        phase: Phase::Unknown,
        flags: Vec::new(),
    };
    if closes.is_empty() {
        return k;
    }
    let px = k.price;

    k.sma30 = sma(closes, 30);
    k.sma100 = sma(closes, 100);
    k.sma200 = sma(closes, 200);
    k.mayer = k.sma200.filter(|s| *s > 0.0).map(|s| px / s);
    k.vs_sma200_pct = k.sma200.and_then(|s| pct_change(s, px));

    if let (Some(s111), Some(s350)) = (sma(closes, 111), sma(closes, 350)) {
        if s350 > 0.0 {
            k.pi_cycle = Some(s111 / (2.0 * s350));
        }
    }

    k.rsi14 = rsi(closes, 14);
    if !stamps.is_empty() {
        let weeks = weekly_closes(closes, stamps, now);
        k.weekly_rsi14 = rsi(&weeks, 14);
    }

    let high = closes.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let low = closes.iter().cloned().fold(f64::INFINITY, f64::min);
    if high > 0.0 {
        k.drawdown_pct = Some(((high - px) / high * 100.0).max(0.0));
    }
    if high > low {
        k.cycle_position_pct = Some(((px - low) / (high - low) * 100.0).clamp(0.0, 100.0));
    }

    k.ret30_pct = nth_back(closes, 30).and_then(|p| pct_change(p, px));
    k.ret90_pct = nth_back(closes, 90).and_then(|p| pct_change(p, px));
    k.ret365_pct = nth_back(closes, 365).and_then(|p| pct_change(p, px));
    k.volatility30_pct = volatility(closes, 30);

    // --- flags: only threshold crossings a human should look at ---
    if k.pi_cycle.is_some_and(|v| v >= t.pi_hot) {
        k.flags.push(format!(
            "pi-cycle {:.2} >= {:.2}",
            k.pi_cycle.unwrap(),
            t.pi_hot
        ));
    }
    if k.mayer.is_some_and(|v| v >= t.mayer_hot) {
        k.flags.push(format!(
            "mayer {:.2} >= {:.2}",
            k.mayer.unwrap(),
            t.mayer_hot
        ));
    } else if k.mayer.is_some_and(|v| v >= t.mayer_stretched) {
        k.flags
            .push(format!("mayer {:.2} stretched", k.mayer.unwrap()));
    }
    if k.weekly_rsi14.is_some_and(|v| v >= t.weekly_rsi_hot) {
        k.flags
            .push(format!("weekly RSI {:.0}", k.weekly_rsi14.unwrap()));
    }
    if k.rsi14.is_some_and(|v| v <= t.rsi_oversold) {
        k.flags
            .push(format!("daily RSI {:.0} oversold", k.rsi14.unwrap()));
    }
    if k.drawdown_pct.is_some_and(|v| v >= t.deep_drawdown_pct) {
        k.flags
            .push(format!("{:.0}% off its high", k.drawdown_pct.unwrap()));
    }

    k.phase = phase_of(&k, t);
    k
}

fn phase_of(k: &CoinKpi, t: KpiThresholds) -> Phase {
    let (Some(mayer), Some(dd)) = (k.mayer, k.drawdown_pct) else {
        return Phase::Unknown;
    };
    // Euphoria first: the cycle markers override everything else.
    if mayer >= t.mayer_hot || k.pi_cycle.is_some_and(|p| p >= t.pi_hot) {
        return Phase::Euphoria;
    }
    if mayer >= 1.0 {
        return Phase::Markup;
    }
    if dd >= t.deep_drawdown_pct {
        return Phase::Capitulation;
    }
    // Below the 200-day but holding above its own 30-day: basing rather than bleeding.
    match (k.sma30, k.price) {
        (Some(s30), px) if px >= s30 => Phase::Accumulation,
        _ => Phase::Markdown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: KpiThresholds = KpiThresholds {
        pi_hot: 0.95,
        mayer_stretched: 1.80,
        mayer_hot: 2.40,
        weekly_rsi_hot: 85.0,
        rsi_oversold: 30.0,
        deep_drawdown_pct: 70.0,
    };
    const DAY: f64 = 86_400.0;
    const T0: f64 = 1_700_000_000.0;

    fn ramp(n: usize, from: f64, to: f64) -> Vec<f64> {
        (0..n)
            .map(|i| from + (to - from) * i as f64 / (n - 1) as f64)
            .collect()
    }

    fn stamps_for(n: usize, end: f64) -> Vec<f64> {
        (0..n).map(|i| end - (n - 1 - i) as f64 * DAY).collect()
    }

    #[test]
    fn sma_needs_the_window_it_claims() {
        assert_eq!(sma(&[1.0, 2.0, 3.0], 3), Some(2.0));
        assert_eq!(
            sma(&[1.0, 2.0], 3),
            None,
            "never average fewer points than asked"
        );
        assert_eq!(sma(&[], 1), None);
    }

    #[test]
    fn rsi_is_wilders_and_pins_at_the_extremes() {
        let up: Vec<f64> = (0..60).map(|i| 100.0 + i as f64).collect();
        assert_eq!(rsi(&up, 14), Some(100.0), "an unbroken climb has no losses");
        let down: Vec<f64> = (0..60).map(|i| 200.0 - i as f64).collect();
        assert!(
            rsi(&down, 14).unwrap() < 1.0,
            "an unbroken fall pins near zero"
        );
        assert_eq!(rsi(&up[..10], 14), None, "not enough history to seed it");
    }

    #[test]
    fn rsi_of_a_flat_series_is_neutral_not_a_divide_by_zero() {
        // No gains and no losses: Wilder's formula divides 0/0 unless it is guarded.
        let flat = vec![100.0; 60];
        assert_eq!(rsi(&flat, 14), Some(100.0), "guarded: al == 0 pins at 100");
    }

    #[test]
    fn weekly_closes_drop_the_running_week() {
        // 21 days ending on a known date; the last partial week must be excluded.
        let closes = ramp(21, 100.0, 120.0);
        let stamps = stamps_for(21, T0);
        let weeks = weekly_closes(&closes, &stamps, T0);
        let all_weeks = {
            let mut k: Vec<i64> = stamps.iter().map(|t| iso_week_key(*t)).collect();
            k.dedup();
            k.len()
        };
        assert_eq!(
            weeks.len(),
            all_weeks - 1,
            "the in-progress week is dropped"
        );
    }

    #[test]
    fn a_week_key_is_stable_across_its_days_and_changes_on_monday() {
        // 1970-01-01 was a Thursday, so day 0 and day 3 (Sunday) share a key.
        let k = |d: i64| iso_week_key(d as f64 * DAY);
        assert_eq!(k(0), k(3), "Thursday and the Sunday after are one ISO week");
        assert_ne!(k(3), k(4), "Monday starts a new one");
        assert_eq!(k(4), k(10), "and that week runs through its Sunday");
    }

    #[test]
    fn mismatched_inputs_produce_no_weeks_rather_than_a_panic() {
        assert!(weekly_closes(&[1.0, 2.0], &[1.0], T0).is_empty());
        assert!(weekly_closes(&[], &[], T0).is_empty());
    }

    #[test]
    fn a_short_series_reports_unknown_rather_than_guessing() {
        let k = compute("AAA", &ramp(40, 100.0, 120.0), &[], T0, T);
        assert_eq!(k.candles, 40);
        assert_eq!(k.mayer, None, "40 candles cannot make a 200-day average");
        assert_eq!(k.pi_cycle, None);
        assert_eq!(k.ret365_pct, None);
        assert_eq!(k.phase, Phase::Unknown, "an unknown is a real answer");
        assert!(k.sma30.is_some(), "but what it can compute, it does");
    }

    #[test]
    fn a_long_climb_reads_as_markup_or_euphoria() {
        let closes = ramp(800, 10_000.0, 90_000.0);
        let k = compute("BTC", &closes, &stamps_for(800, T0), T0, T);
        assert!(k.mayer.unwrap() > 1.0, "price is above its 200-day average");
        assert!(
            matches!(k.phase, Phase::Markup | Phase::Euphoria),
            "{:?}",
            k.phase
        );
        assert_eq!(k.drawdown_pct.unwrap(), 0.0, "at its high");
        assert_eq!(k.cycle_position_pct.unwrap(), 100.0);
        assert!(
            k.pi_cycle.is_some(),
            "800 candles is enough for the Pi-cycle ratio"
        );
        assert!(k.weekly_rsi14.is_some(), "and for a weekly RSI");
    }

    #[test]
    fn a_deep_drawdown_reads_as_capitulation_and_says_so() {
        let mut closes = ramp(400, 10_000.0, 90_000.0);
        closes.extend(ramp(400, 90_000.0, 15_000.0)); // -83% from the high
        let k = compute("BTC", &closes, &[], T0, T);
        assert!(k.drawdown_pct.unwrap() > 70.0, "{:?}", k.drawdown_pct);
        assert_eq!(k.phase, Phase::Capitulation);
        assert!(
            k.flags.iter().any(|f| f.contains("off its high")),
            "the flag names the drawdown: {:?}",
            k.flags
        );
        assert!(
            k.mayer.unwrap() < 1.0,
            "and it is below its 200-day average"
        );
    }

    #[test]
    fn the_cycle_markers_flag_and_override_the_phase() {
        // A parabolic finish: price far above both averages.
        let mut closes = ramp(700, 10_000.0, 20_000.0);
        closes.extend(ramp(100, 20_000.0, 120_000.0));
        let k = compute("BTC", &closes, &[], T0, T);
        assert!(k.mayer.unwrap() >= T.mayer_hot, "mayer {:?}", k.mayer);
        assert_eq!(k.phase, Phase::Euphoria);
        assert!(
            k.flags.iter().any(|f| f.starts_with("mayer")),
            "{:?}",
            k.flags
        );
    }

    #[test]
    fn volatility_is_zero_for_a_flat_series_and_positive_otherwise() {
        assert_eq!(volatility(&vec![100.0; 60], 30), Some(0.0));
        let choppy: Vec<f64> = (0..60)
            .map(|i| 100.0 + if i % 2 == 0 { 5.0 } else { -5.0 })
            .collect();
        assert!(volatility(&choppy, 30).unwrap() > 0.0);
        assert_eq!(volatility(&[100.0, 101.0], 30), None, "not enough history");
    }

    #[test]
    fn returns_measure_from_the_right_bar() {
        let closes = ramp(400, 100.0, 500.0);
        let k = compute("AAA", &closes, &[], T0, T);
        let expect = |n: usize| {
            let prev = closes[closes.len() - 1 - n];
            (closes[closes.len() - 1] / prev - 1.0) * 100.0
        };
        assert!((k.ret30_pct.unwrap() - expect(30)).abs() < 1e-9);
        assert!((k.ret90_pct.unwrap() - expect(90)).abs() < 1e-9);
        assert!((k.ret365_pct.unwrap() - expect(365)).abs() < 1e-9);
    }

    #[test]
    fn an_empty_series_is_handled_rather_than_panicking() {
        let k = compute("AAA", &[], &[], T0, T);
        assert_eq!(k.candles, 0);
        assert_eq!(k.price, 0.0);
        assert_eq!(k.phase, Phase::Unknown);
        assert!(k.flags.is_empty());
    }
}
