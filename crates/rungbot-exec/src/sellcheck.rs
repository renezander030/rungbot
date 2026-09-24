//! Could the held amount of each coin be sold at its venue right now?
//!
//! The sell side runs least often and matters most on the day it runs. This check proves
//! it without placing anything: the held amount is rounded the way a real order would be,
//! then compared with the venue's minimum base amount and minimum notional at the current
//! price. A coin that fails here would be skipped as "below exchange min" when it counts.
//!
//! Never fails: a venue error is a finding (`check failed: …`), not a crash.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::housekeeping::Route;
use crate::pyfmt::{self, g};
use crate::reconcile::VenueSource;
use crate::venue::Venue;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SellCheck {
    pub ok: bool,
    pub reason: String,
    /// The held amount after the venue's rounding.
    pub qty: f64,
    pub min_base: Option<f64>,
    pub min_quote: Option<f64>,
    /// `qty * price`.
    pub notional: f64,
}

impl SellCheck {
    fn failed(reason: String) -> SellCheck {
        SellCheck {
            ok: false,
            reason,
            qty: 0.0,
            min_base: None,
            min_quote: None,
            notional: 0.0,
        }
    }
}

/// Check one coin: `held` of `pair`'s base at `price`.
pub fn check(v: &dyn Venue, pair: &str, held: Option<f64>, price: Option<f64>) -> SellCheck {
    let held = held.unwrap_or(0.0);
    let price = price.unwrap_or(0.0);
    if held <= 0.0 {
        return SellCheck::failed("nothing held".into());
    }
    if price <= 0.0 {
        return SellCheck::failed("no price".into());
    }
    let read = v
        .limits(pair)
        .and_then(|l| v.round_amount(pair, held).map(|q| (l, q)));
    let (limits, qty) = match read {
        Ok(x) => x,
        Err(e) => {
            return SellCheck::failed(format!("check failed: {}", pyfmt::head(&e.to_string(), 80)))
        }
    };
    let notional = qty * price;
    let (min_base, min_quote) = (limits.min_base, limits.min_quote);
    let base_asset = pair.split('/').next().unwrap_or("");
    let base_asset = base_asset.split('_').next().unwrap_or("");
    let (ok, reason) = if qty <= 0.0 {
        (false, format!("rounds to 0 (held {})", g(held, 6)))
    } else if min_base != 0.0 && qty < min_base {
        (
            false,
            format!("dust: {} < min {} {base_asset}", g(qty, 6), g(min_base, 6)),
        )
    } else if min_quote != 0.0 && notional < min_quote {
        (
            false,
            format!(
                "dust: ${} < min ${}",
                pyfmt::fixed(notional, 2),
                pyfmt::fixed(min_quote, 2)
            ),
        )
    } else {
        (
            true,
            format!("sellable {} (~${})", g(qty, 6), pyfmt::fixed(notional, 2)),
        )
    };
    SellCheck {
        ok,
        reason,
        qty,
        min_base: Some(min_base),
        min_quote: Some(min_quote),
        notional,
    }
}

/// Check every routed coin with a holding in `held`, at `prices`.
pub fn check_all(
    routing: &[Route],
    venues: &dyn VenueSource,
    held: &BTreeMap<String, f64>,
    prices: &BTreeMap<String, f64>,
) -> BTreeMap<String, SellCheck> {
    routing
        .iter()
        .filter(|r| held.contains_key(&r.sym))
        .map(|r| {
            let res = match venues.venue(&r.exch) {
                Ok(v) => check(
                    v,
                    &r.pair,
                    held.get(&r.sym).copied(),
                    prices.get(&r.sym).copied(),
                ),
                Err(e) => SellCheck::failed(format!("check failed: {}", pyfmt::head(&e, 80))),
            };
            (r.sym.clone(), res)
        })
        .collect()
}

/// One log line: `SELL-CHECK 5/7 sellable; AAA dust: $1.20 < min $3.00; ...`, failures in
/// symbol order.
pub fn summary(results: &BTreeMap<String, SellCheck>) -> String {
    let ok = results.values().filter(|r| r.ok).count();
    let bad: Vec<String> = results
        .iter()
        .filter(|(_, r)| !r.ok)
        .map(|(s, r)| format!("{s} {}", r.reason))
        .collect();
    let mut line = format!("SELL-CHECK {ok}/{} sellable", results.len());
    if !bad.is_empty() {
        line.push_str("; ");
        line.push_str(&bad.join("; "));
    }
    line
}
