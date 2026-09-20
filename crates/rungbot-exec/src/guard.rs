//! The rails. Every order passes through here, and the defaults are deliberately small.
//!
//! These are backstops, not strategy. The ladder already decides sizes; this exists for
//! the run where something upstream is wrong — a bad price, a config typo, a loop that
//! should not have fired — and the only useful behaviour is to refuse.
//!
//! Four things must all be true before an order is placed:
//!
//! 1. The mode is [`Mode::Live`]. Anything else prints and places nothing.
//! 2. The halt file does not exist. Creating it stops trading with no config change and
//!    no restart, which is what you want when you want it.
//! 3. You have acknowledged live trading once, explicitly.
//! 4. The order is inside every cap.
//!
//! Pure: it is handed the facts and returns a verdict. Reading the halt file and the
//! clock is the caller's job.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Notify only. The default, and the only mode that needs no key.
    #[default]
    Off,
    /// Read balances and print the exact orders it would place. Places none.
    Dry,
    /// Place real orders.
    Live,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "no" => Ok(Mode::Off),
            "dry" | "dry-run" | "dryrun" => Ok(Mode::Dry),
            "live" => Ok(Mode::Live),
            other => Err(format!("mode must be off, dry or live, got {other:?}")),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Dry => "dry",
            Mode::Live => "live",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    /// Largest single order, in quote currency. `0` disables the cap.
    pub max_order_quote: f64,
    /// Most that may be placed in a rolling day. `0` disables.
    pub max_daily_notional: f64,
    /// Most orders in a rolling day. `0` disables.
    pub max_daily_orders: usize,
    /// Refuse if the venue price has moved this far from the price the ladder decided on.
    pub max_slippage_pct: f64,
    /// Refuse an order smaller than the venue would accept anyway.
    pub min_order_quote: f64,
}

impl Default for Caps {
    fn default() -> Self {
        // Small on purpose. Someone who wants bigger orders can say so; nobody should
        // discover the size of their first live order by accident.
        Caps {
            max_order_quote: 50.0,
            max_daily_notional: 200.0,
            max_daily_orders: 10,
            max_slippage_pct: 2.0,
            min_order_quote: 1.0,
        }
    }
}

/// What the ladder wants to do, before the rails see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Intent {
    pub sym: String,
    pub quote: f64,
    /// The price the decision was made at.
    pub decided_price: f64,
    /// The price the venue is showing now.
    pub venue_price: f64,
}

/// The facts the rails need that they cannot work out themselves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Context {
    pub mode: Mode,
    pub halted: bool,
    pub acknowledged: bool,
    pub today_notional: f64,
    pub today_orders: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    NotLive(Mode),
    Halted,
    NotAcknowledged,
    TooSmall { quote: f64, min: f64 },
    TooLarge { quote: f64, cap: f64 },
    DailyNotional { would_be: f64, cap: f64 },
    DailyOrders { placed: usize, cap: usize },
    Slippage { moved_pct: f64, cap: f64 },
    BadPrice,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refusal::NotLive(m) => write!(f, "mode is {}, so nothing is placed", m.as_str()),
            Refusal::Halted => write!(f, "the halt file exists; remove it to resume"),
            Refusal::NotAcknowledged => {
                write!(
                    f,
                    "live trading has not been acknowledged; pass --i-understand once"
                )
            }
            Refusal::TooSmall { quote, min } => {
                write!(f, "order of {quote:.2} is below the {min:.2} minimum")
            }
            Refusal::TooLarge { quote, cap } => {
                write!(f, "order of {quote:.2} exceeds the {cap:.2} per-order cap")
            }
            Refusal::DailyNotional { would_be, cap } => {
                write!(
                    f,
                    "would put the day at {would_be:.2}, over the {cap:.2} cap"
                )
            }
            Refusal::DailyOrders { placed, cap } => {
                write!(f, "{placed} orders already placed today, cap is {cap}")
            }
            Refusal::Slippage { moved_pct, cap } => write!(
                f,
                "the venue price moved {moved_pct:.2}% from the decision, cap is {cap:.2}%"
            ),
            Refusal::BadPrice => write!(f, "a price was zero or negative"),
        }
    }
}

impl std::error::Error for Refusal {}

/// The only way an order gets placed.
pub fn check(intent: &Intent, ctx: Context, caps: Caps) -> Result<(), Refusal> {
    if ctx.mode != Mode::Live {
        return Err(Refusal::NotLive(ctx.mode));
    }
    if ctx.halted {
        return Err(Refusal::Halted);
    }
    if !ctx.acknowledged {
        return Err(Refusal::NotAcknowledged);
    }
    if intent.decided_price <= 0.0 || intent.venue_price <= 0.0 || intent.quote <= 0.0 {
        return Err(Refusal::BadPrice);
    }
    if intent.quote < caps.min_order_quote {
        return Err(Refusal::TooSmall {
            quote: intent.quote,
            min: caps.min_order_quote,
        });
    }
    if caps.max_order_quote > 0.0 && intent.quote > caps.max_order_quote {
        return Err(Refusal::TooLarge {
            quote: intent.quote,
            cap: caps.max_order_quote,
        });
    }
    if caps.max_daily_orders > 0 && ctx.today_orders >= caps.max_daily_orders {
        return Err(Refusal::DailyOrders {
            placed: ctx.today_orders,
            cap: caps.max_daily_orders,
        });
    }
    let would_be = ctx.today_notional + intent.quote;
    if caps.max_daily_notional > 0.0 && would_be > caps.max_daily_notional {
        return Err(Refusal::DailyNotional {
            would_be,
            cap: caps.max_daily_notional,
        });
    }
    if caps.max_slippage_pct > 0.0 {
        let moved = ((intent.venue_price / intent.decided_price) - 1.0).abs() * 100.0;
        if moved > caps.max_slippage_pct {
            return Err(Refusal::Slippage {
                moved_pct: moved,
                cap: caps.max_slippage_pct,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(quote: f64) -> Intent {
        Intent {
            sym: "AAA".into(),
            quote,
            decided_price: 100.0,
            venue_price: 100.0,
        }
    }

    fn live() -> Context {
        Context {
            mode: Mode::Live,
            halted: false,
            acknowledged: true,
            today_notional: 0.0,
            today_orders: 0,
        }
    }

    #[test]
    fn nothing_is_placed_unless_the_mode_is_live() {
        for m in [Mode::Off, Mode::Dry] {
            let ctx = Context { mode: m, ..live() };
            assert_eq!(
                check(&intent(10.0), ctx, Caps::default()),
                Err(Refusal::NotLive(m))
            );
        }
        assert!(check(&intent(10.0), live(), Caps::default()).is_ok());
    }

    #[test]
    fn the_halt_file_stops_everything() {
        let ctx = Context {
            halted: true,
            ..live()
        };
        assert_eq!(
            check(&intent(10.0), ctx, Caps::default()),
            Err(Refusal::Halted)
        );
    }

    #[test]
    fn live_trading_must_be_acknowledged_once() {
        let ctx = Context {
            acknowledged: false,
            ..live()
        };
        assert_eq!(
            check(&intent(10.0), ctx, Caps::default()),
            Err(Refusal::NotAcknowledged)
        );
    }

    #[test]
    fn the_per_order_cap_is_small_by_default_and_binds() {
        let caps = Caps::default();
        assert_eq!(
            caps.max_order_quote, 50.0,
            "nobody should discover this by accident"
        );
        assert!(
            check(&intent(50.0), live(), caps).is_ok(),
            "exactly at the cap is allowed"
        );
        assert!(matches!(
            check(&intent(50.01), live(), caps),
            Err(Refusal::TooLarge { .. })
        ));
    }

    #[test]
    fn the_daily_notional_cap_counts_what_is_already_placed() {
        let caps = Caps::default();
        let ctx = Context {
            today_notional: 180.0,
            ..live()
        };
        assert!(
            check(&intent(20.0), ctx, caps).is_ok(),
            "exactly at 200 is allowed"
        );
        assert!(matches!(
            check(&intent(21.0), ctx, caps),
            Err(Refusal::DailyNotional { .. })
        ));
    }

    #[test]
    fn the_daily_order_count_binds_before_the_next_order() {
        let caps = Caps::default();
        let ctx = Context {
            today_orders: 10,
            ..live()
        };
        assert!(matches!(
            check(&intent(1.0), ctx, caps),
            Err(Refusal::DailyOrders {
                placed: 10,
                cap: 10
            })
        ));
        let ok = Context {
            today_orders: 9,
            ..live()
        };
        assert!(check(&intent(1.0), ok, caps).is_ok());
    }

    #[test]
    fn slippage_is_measured_in_both_directions() {
        let caps = Caps::default(); // 2%
        let mut up = intent(10.0);
        up.venue_price = 103.0;
        assert!(matches!(
            check(&up, live(), caps),
            Err(Refusal::Slippage { .. })
        ));

        let mut down = intent(10.0);
        down.venue_price = 97.0;
        assert!(
            matches!(check(&down, live(), caps), Err(Refusal::Slippage { .. })),
            "a price that ran away downward is just as wrong"
        );

        let mut fine = intent(10.0);
        fine.venue_price = 101.0;
        assert!(check(&fine, live(), caps).is_ok());
    }

    #[test]
    fn a_zero_or_negative_price_is_refused_rather_than_divided_by() {
        let caps = Caps::default();
        let mut zero = intent(10.0);
        zero.decided_price = 0.0;
        assert_eq!(check(&zero, live(), caps), Err(Refusal::BadPrice));

        let mut neg = intent(10.0);
        neg.venue_price = -1.0;
        assert_eq!(check(&neg, live(), caps), Err(Refusal::BadPrice));

        assert_eq!(check(&intent(0.0), live(), caps), Err(Refusal::BadPrice));
    }

    #[test]
    fn dust_is_refused_before_the_venue_has_to() {
        let caps = Caps::default();
        assert!(matches!(
            check(&intent(0.5), live(), caps),
            Err(Refusal::TooSmall { .. })
        ));
    }

    #[test]
    fn a_zero_cap_means_unlimited_not_forbidden() {
        let caps = Caps {
            max_order_quote: 0.0,
            max_daily_notional: 0.0,
            max_daily_orders: 0,
            max_slippage_pct: 0.0,
            min_order_quote: 1.0,
        };
        let ctx = Context {
            today_notional: 1e9,
            today_orders: 9_999,
            ..live()
        };
        let mut wild = intent(1e6);
        wild.venue_price = 500.0; // enormous slippage, but the cap is off
        assert!(
            check(&wild, ctx, caps).is_ok(),
            "explicitly disabled means disabled"
        );
    }

    #[test]
    fn the_refusals_say_what_to_do_about_them() {
        let msgs = [
            Refusal::Halted.to_string(),
            Refusal::NotAcknowledged.to_string(),
            Refusal::NotLive(Mode::Dry).to_string(),
        ];
        assert!(msgs[0].contains("remove it"), "{}", msgs[0]);
        assert!(msgs[1].contains("--i-understand"), "{}", msgs[1]);
        assert!(msgs[2].contains("dry"), "{}", msgs[2]);
    }

    #[test]
    fn mode_parsing_accepts_the_spellings_a_config_produces() {
        assert_eq!(Mode::parse("off").unwrap(), Mode::Off);
        assert_eq!(
            Mode::parse("false").unwrap(),
            Mode::Off,
            "YAML 1.1 turns off into false"
        );
        assert_eq!(Mode::parse("DRY").unwrap(), Mode::Dry);
        assert_eq!(Mode::parse("live").unwrap(), Mode::Live);
        assert!(
            Mode::parse("yes").is_err(),
            "ambiguous is refused, not guessed"
        );
        assert_eq!(Mode::default(), Mode::Off, "the default can never trade");
    }
}
