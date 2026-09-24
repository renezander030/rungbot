//! The watchers: read-only checks that mail a person when the book needs a decision.
//!
//! None of them places, cancels or reprices anything. Each one reads state (the regime,
//! the order journal, balances, public market data), decides whether something is worth
//! a message, and remembers what it already said so the same thing is never mailed twice.
//!
//! * [`regime_watch`] — the market label flipped (announced when it flips, and again
//!   when it has held long enough to count as confirmed).
//! * [`btc_level`] — BTC nears or breaks a price line you set. Once per crossing.
//! * [`zone`] — resting buy zones that stopped doing their job: a RUN-gate flip, a rung
//!   that sat unfilled while spot ran away, cash left idle on a venue.
//! * [`froth`] — daily crowding signals (sentiment, funding, open interest, Mayer
//!   multiple) and the BTC blow-off arming the sell policy reads.
//! * [`divergence`] — the live book falling behind what the backtest promised.
//!
//! Pure: every function takes what was read and `now`, and returns what to print, what to
//! send and what to write. The CLI does the reading, the sending and the writing, so all
//! of this is replayed in tests against recorded outputs of the original implementation.

pub mod btc_level;
pub mod divergence;
pub mod froth;
pub mod json;
pub mod pyfmt;
pub mod regime_state;
pub mod regime_watch;
pub mod textwrap;
pub mod zone;

pub use json::Json;

/// The commands and paths the mails tell you to run. Only wording: nothing here is
/// executed.
#[derive(Debug, Clone, PartialEq)]
pub struct Hints {
    /// The executor's deploy command, as the mails quote it.
    pub deploy_cmd: String,
    /// The halt file the executor honours.
    pub halt_file: String,
}

impl Default for Hints {
    fn default() -> Self {
        Hints {
            deploy_cmd: "rungbot-exec deploy".into(),
            halt_file: "~/.config/rungbot/halt".into(),
        }
    }
}

/// `YYYY-MM-DD HH:MM UTC`, the "Checked:" stamp every mail ends with.
pub fn utc_minute(now: f64) -> String {
    let (y, m, d, h, mi, _) = crate::time::civil(now);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// Python `str.upper()`.
pub fn upper(s: &str) -> String {
    s.to_uppercase()
}
