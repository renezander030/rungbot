//! The ladder *as configured*, replayed over a trailing window of real prices.
//!
//! Every step goes through the live [`rungbot_core::analyze`], so the 24h directional
//! lock, the dynamic per-coin cap, the entry-relative sell ladder, the knife floor and the
//! protected core are exactly the production rules. Around it sits the dollar bookkeeping
//! a live run applies: each venue's free stable is shared by the coins routed there, an
//! order is capped at `max_order_usd` and at what the venue holds, a trade below the
//! pair's minimum notional is skipped **and the coin's ladder state rolled back**, and a
//! buy re-averages the cost basis.
//!
//! The window's granularity is read from the data: the 24h lookback is however many
//! points span a day at the median spacing, so an hourly and a daily series both run the
//! true 24h rules.

use std::collections::BTreeMap;

use rungbot_core::{analyze, Config, Price, State};

use crate::py::{ff, fs, round_half_even, sum};
use crate::{entry_text, named_get, named_set, Book, History, Named};

pub const HOUR: i64 = 3600;

/// Minimum order notional when the book has none for a pair, by venue.
pub fn min_notional_fallback(venue: &str) -> f64 {
    match venue {
        "binance" => 5.0,
        "gate" => 3.0,
        "revx" => 0.1,
        _ => 1.0,
    }
}

#[derive(Debug, Clone)]
pub struct WindowOpts {
    /// Window length in days; only labels the report, the data sets the span.
    pub days: i64,
    /// Hard per-order cap in dollars. 0 = unlimited.
    pub max_order_usd: f64,
    /// Replace the book's free stable with this bag, split evenly across venues.
    pub bag: Option<f64>,
}

impl Default for WindowOpts {
    fn default() -> Self {
        WindowOpts {
            days: 28,
            max_order_usd: 50.0,
            bag: None,
        }
    }
}

/// The report, plus the start book the sweep must replay.
#[derive(Debug, Clone)]
pub struct WindowRun {
    pub text: String,
    /// `None` when the run stopped before it could fix a book.
    pub book: Option<Book>,
}

/// Points per 24h and the granularity label, from the median timestamp spacing.
pub fn lag_from(ts: &[i64]) -> (usize, i64) {
    let mut deltas: Vec<i64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    deltas.sort_unstable();
    let dt = if deltas.is_empty() {
        HOUR
    } else {
        deltas[deltas.len() / 2]
    };
    let lag = round_half_even(86400.0 / dt as f64).max(1) as usize;
    (lag, dt)
}

/// Run one window. `history` is the price cache, or why it could not be had.
pub fn run(
    cfg: &Config,
    history: Result<&History, String>,
    book: &Book,
    opts: &WindowOpts,
) -> WindowRun {
    let mut out = String::new();

    // Venue topology comes from the coins' routing, like the live run.
    let mut counts: Vec<(String, i64)> = Vec::new();
    for c in &cfg.coins {
        let v = c.venue.as_str();
        match counts.iter_mut().find(|(n, _)| n == v) {
            Some(slot) => slot.1 += 1,
            None => counts.push((v.to_string(), 1)),
        }
    }

    // Starting position: the book's holdings and each routed venue's free stable.
    let mut held: Named = Vec::new();
    let mut stable: Named = Vec::new();
    for c in &cfg.coins {
        named_set(
            &mut held,
            &c.symbol,
            named_get(&book.held, &c.symbol).unwrap_or(0.0),
        );
        let v = c.venue.as_str();
        named_set(&mut stable, v, named_get(&book.stable, v).unwrap_or(0.0));
    }
    if let Some(b) = opts.bag {
        stable = counts
            .iter()
            .map(|(e, _)| (e.clone(), b / counts.len() as f64))
            .collect();
    }
    let mut minnot: Named = Vec::new();
    for c in &cfg.coins {
        let v = c.venue.as_str();
        let floor = match named_get(&book.min_notional, &c.symbol) {
            Some(x) => x,
            None => {
                let f = min_notional_fallback(v);
                out.push_str(&format!(
                    "min-notional {} ({v} {}) from venue failed, using ${}: not in the book\n",
                    c.symbol,
                    c.pair,
                    ff(f, "g")
                ));
                f
            }
        };
        named_set(&mut minnot, &c.symbol, floor.max(0.01));
    }
    let book_out = Book {
        held: held.clone(),
        stable: stable.clone(),
        min_notional: minnot.clone(),
        counts: counts.clone(),
    };

    let hist = match history {
        Ok(h) => h,
        Err(e) => {
            out.push_str(&e);
            out.push('\n');
            return WindowRun {
                text: out,
                book: Some(book_out),
            };
        }
    };

    let n = hist.0.iter().map(|(_, v)| v.len()).min().unwrap_or(0);
    let series: BTreeMap<&str, Vec<f64>> = hist
        .0
        .iter()
        .map(|(s, v)| (s.as_str(), v[v.len() - n..].iter().map(|p| p.1).collect()))
        .collect();
    let first = &hist.0[0].1;
    let ts_ref: Vec<i64> = first[first.len() - n..].iter().map(|p| p.0).collect();
    let price_of = |sym: &str, i: usize| -> f64 {
        series
            .get(sym)
            .map(|v| v[i])
            .unwrap_or_else(|| panic!("no history for {sym}"))
    };

    let (lag, dt) = lag_from(&ts_ref);
    let gran = if dt <= HOUR * 2 { "hourly" } else { "daily" };

    let held0 = held.clone();
    let stable_start = stable.clone();
    let stable0 = sum(stable.iter().map(|x| x.1));
    let mut state: State = State::new();
    let (mut buys, mut sells) = (0i64, 0i64);
    let (mut buy_usd, mut sell_usd) = (0.0f64, 0.0f64);
    let venue_of = |sym: &str| cfg.coin(sym).map(|c| c.venue.as_str()).unwrap_or("");
    let count_of = |v: &str| {
        counts
            .iter()
            .find(|(n, _)| n == v)
            .map(|(_, c)| *c)
            .unwrap_or(1) as f64
    };

    for (i, &t) in ts_ref.iter().enumerate().take(n).skip(lag) {
        let now = t as f64;
        let mut prices = BTreeMap::new();
        for c in &cfg.coins {
            let px = price_of(&c.symbol, i);
            let px24 = price_of(&c.symbol, i - lag);
            prices.insert(
                c.symbol.clone(),
                Price {
                    price: px,
                    chg_24h: Some((px / px24 - 1.0) * 100.0),
                },
            );
        }
        let prev = state.clone();
        let outcome = analyze(cfg, &prices, &state, now);
        state = outcome.state;

        for r in &outcome.buys {
            let sym = r.row.sym.as_str();
            let exch = venue_of(sym);
            let price = prices[sym].price;
            let st = named_get(&stable, exch).unwrap_or(0.0);
            let base = st / count_of(exch);
            let mut notional = r.pct / 100.0 * base;
            if opts.max_order_usd > 0.0 {
                notional = notional.min(opts.max_order_usd);
            }
            notional = notional.min(st);
            if notional < named_get(&minnot, sym).unwrap_or(0.01) {
                rollback(&mut state, &prev, sym);
                continue;
            }
            let units = notional / price;
            let old_u = named_get(&held, sym).unwrap_or(0.0);
            let cs = state.get_mut(sym).expect("analyze wrote the coin");
            let old_c = cs
                .cost_basis
                .filter(|v| *v != 0.0)
                .or(cfg.coin(sym).and_then(|c| c.entry).filter(|v| *v != 0.0))
                .unwrap_or(price);
            named_set(&mut held, sym, old_u + units);
            cs.cost_basis = Some(if old_u + units != 0.0 {
                ((old_u * old_c) + notional) / (old_u + units)
            } else {
                old_c
            });
            named_set(&mut stable, exch, st - notional);
            buys += 1;
            buy_usd += notional;
        }

        for r in &outcome.sells {
            let sym = r.row.sym.as_str();
            let exch = venue_of(sym);
            let price = prices[sym].price;
            let h = named_get(&held, sym).unwrap_or(0.0);
            let qty = r.pct / 100.0 * h;
            let notional = qty * price;
            if qty <= 0.0 || notional < named_get(&minnot, sym).unwrap_or(0.01) {
                rollback(&mut state, &prev, sym);
                continue;
            }
            named_set(&mut held, sym, h - qty);
            let st = named_get(&stable, exch).unwrap_or(0.0);
            named_set(&mut stable, exch, st + notional);
            sells += 1;
            sell_usd += notional;
        }
    }

    // --- valuation ---
    let end_price = |s: &str| price_of(s, n - 1);
    let start_price = |s: &str| price_of(s, 0);
    let strat_val =
        sum(held.iter().map(|(s, h)| h * end_price(s))) + sum(stable.iter().map(|x| x.1));
    let bh_val = sum(held0.iter().map(|(s, h)| h * end_price(s))) + stable0;
    let start_val = sum(held0.iter().map(|(s, h)| h * start_price(s))) + stable0;

    let days = opts.days;
    out.push_str(&format!(
        "=== {days}-DAY BACKTEST (as configured) — {n} {gran} points, {days}d ===\n"
    ));
    let mut sorted = stable_start.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let bag_by_venue = sorted
        .iter()
        .map(|(e, v)| format!("{e} ${}", ff(*v, ",.2f")))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!(
        "start bag: ${} free stable ({bag_by_venue}) | coins valued at cost-basis entries\n",
        ff(stable0, ",.2f")
    ));
    out.push_str("\nPER COIN (price move over window):\n");
    for (s, h0) in &held0 {
        let mv = (end_price(s) / start_price(s) - 1.0) * 100.0;
        out.push_str(&format!(
            "  {} {}%   held {}->{}   cost_basis ${}\n",
            fs(s, "5"),
            ff(mv, "+6.1f"),
            ff(*h0, ".4g"),
            ff(named_get(&held, s).unwrap_or(0.0), ".4g"),
            entry_text(cfg.coin(s).and_then(|c| c.entry))
        ));
    }
    out.push_str(&format!(
        "\nTRADES: {buys} buys (${}), {sells} sells (${})\n",
        ff(buy_usd, ".2f"),
        ff(sell_usd, ".2f")
    ));
    out.push_str(&format!(
        "end stable bag: ${}\n",
        ff(sum(stable.iter().map(|x| x.1)), ".2f")
    ));
    out.push_str(&format!(
        "\nVALUE @ window start:     ${}\n",
        ff(start_val, ",.2f")
    ));
    out.push_str(&format!(
        "VALUE @ end, buy & hold:  ${}   ({}%)\n",
        ff(bh_val, ",.2f"),
        ff((bh_val / start_val - 1.0) * 100.0, "+.1f")
    ));
    out.push_str(&format!(
        "VALUE @ end, STRATEGY:    ${}   ({}%)\n",
        ff(strat_val, ",.2f"),
        ff((strat_val / start_val - 1.0) * 100.0, "+.1f")
    ));
    out.push_str(&format!(
        "\nSTRATEGY vs BUY&HOLD:     {}%  (alpha)\n",
        ff((strat_val / bh_val - 1.0) * 100.0, "+.2f")
    ));
    WindowRun {
        text: out,
        book: Some(book_out),
    }
}

/// A skipped trade must not advance the ladder: put the coin back as it was.
fn rollback(state: &mut State, prev: &State, sym: &str) {
    // On a first sighting there is nothing to go back to; the fresh state stays.
    if let Some(p) = prev.get(sym) {
        state.insert(sym.to_string(), p.clone());
    }
}
