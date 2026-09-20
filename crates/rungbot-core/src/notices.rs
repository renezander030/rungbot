//! Notification dedupe: one message per **new** signal, one reminder a day, never a loop.
//!
//! A ladder that runs every 30 minutes will re-derive the same standing signal 48 times
//! a day. Sent naively that is 48 notifications, which trains the reader to ignore all of
//! them — and an ignored alert is worse than no alert, because you believe you have one.
//!
//! The rule here: a signal that is **new** sends immediately. A signal that is still true
//! sends again only after `remind_after_s`. A signal that goes away is forgotten, so if
//! it returns it counts as new again.
//!
//! Pure. Persisting the state is the CLI's job.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A day, which is the cadence a standing signal should nag at.
pub const DEFAULT_REMIND_AFTER_S: f64 = 86_400.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub first_seen: f64,
    pub last_sent: f64,
    pub times_sent: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Notices {
    pub seen: BTreeMap<String, Notice>,
}

/// Why a signal is or is not being sent this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not seen before: send it.
    New,
    /// Still true and the reminder is due: send it again.
    Reminder,
    /// Still true but sent recently: stay quiet.
    Suppressed,
}

impl Notices {
    /// Decide on one signal without recording anything.
    pub fn verdict(&self, key: &str, now: f64, remind_after_s: f64) -> Verdict {
        match self.seen.get(key) {
            None => Verdict::New,
            Some(n) if now - n.last_sent >= remind_after_s => Verdict::Reminder,
            Some(_) => Verdict::Suppressed,
        }
    }

    /// Decide on one signal and record the send if there is one.
    pub fn should_send(&mut self, key: &str, now: f64, remind_after_s: f64) -> bool {
        match self.verdict(key, now, remind_after_s) {
            Verdict::Suppressed => false,
            v => {
                let entry = self.seen.entry(key.to_string()).or_insert(Notice {
                    first_seen: now,
                    last_sent: now,
                    times_sent: 0,
                });
                entry.last_sent = now;
                entry.times_sent += 1;
                let _ = v;
                true
            }
        }
    }

    /// Forget every signal that is no longer true.
    ///
    /// This is what makes a returning signal count as new. Call it once per run with the
    /// full set of keys that are currently live.
    pub fn retain_live(&mut self, live: &[String]) {
        self.seen.retain(|k, _| live.iter().any(|l| l == k));
    }

    /// Filter a set of live signals down to the ones worth sending now.
    pub fn filter<'a>(
        &mut self,
        live: &'a [String],
        now: f64,
        remind_after_s: f64,
    ) -> Vec<&'a String> {
        self.retain_live(live);
        live.iter()
            .filter(|k| self.should_send(k, now, remind_after_s))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: f64 = 1_700_000_000.0;
    const DAY: f64 = 86_400.0;
    const HALF_HOUR: f64 = 1800.0;

    fn keys(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_new_signal_sends_immediately() {
        let mut n = Notices::default();
        assert_eq!(n.verdict("AAA:dip", T0, DAY), Verdict::New);
        assert!(n.should_send("AAA:dip", T0, DAY));
        assert_eq!(n.seen["AAA:dip"].times_sent, 1);
    }

    #[test]
    fn the_same_signal_does_not_resend_every_thirty_minutes() {
        let mut n = Notices::default();
        assert!(n.should_send("AAA:dip", T0, DAY));
        for i in 1..48 {
            let t = T0 + i as f64 * HALF_HOUR;
            assert!(
                !n.should_send("AAA:dip", t, DAY),
                "run {i} at +{}h resent a standing signal",
                i / 2
            );
        }
        assert_eq!(
            n.seen["AAA:dip"].times_sent, 1,
            "one message in a day, not 48"
        );
    }

    #[test]
    fn a_standing_signal_reminds_once_a_day() {
        let mut n = Notices::default();
        assert!(n.should_send("AAA:dip", T0, DAY));
        assert_eq!(n.verdict("AAA:dip", T0 + DAY, DAY), Verdict::Reminder);
        assert!(n.should_send("AAA:dip", T0 + DAY, DAY));
        assert_eq!(n.seen["AAA:dip"].times_sent, 2);
        assert!(
            !n.should_send("AAA:dip", T0 + DAY + HALF_HOUR, DAY),
            "then quiet again"
        );
    }

    #[test]
    fn a_signal_that_goes_away_and_returns_is_new_again() {
        let mut n = Notices::default();
        let live = keys(&["AAA:dip"]);
        assert_eq!(n.filter(&live, T0, DAY).len(), 1);

        n.retain_live(&[]); // the dip recovered
        assert!(n.is_empty(), "a signal that is no longer true is forgotten");

        assert_eq!(
            n.filter(&live, T0 + HALF_HOUR, DAY).len(),
            1,
            "it comes back and that is news, not a duplicate"
        );
    }

    #[test]
    fn filter_handles_a_mixed_set() {
        let mut n = Notices::default();
        let first = keys(&["AAA:dip", "BBB:sell"]);
        assert_eq!(n.filter(&first, T0, DAY).len(), 2, "both are new");

        // AAA still standing, BBB gone, CCC newly true.
        let second = keys(&["AAA:dip", "CCC:breaker"]);
        let send = n.filter(&second, T0 + HALF_HOUR, DAY);
        assert_eq!(send, vec!["CCC:breaker"], "only the genuinely new one");
        assert!(
            !n.seen.contains_key("BBB:sell"),
            "the departed signal was dropped"
        );
    }

    #[test]
    fn a_zero_reminder_window_means_always_send() {
        let mut n = Notices::default();
        assert!(n.should_send("k", T0, 0.0));
        assert!(n.should_send("k", T0, 0.0), "every run, by request");
    }
}
