//! A guard for tests that need particular environment variables.
//!
//! Rust runs tests as threads inside one process, so `set_var` and `remove_var` are
//! global. A test that cleared `RUNGBOT_OFFLINE` when it finished switched the network
//! guard off underneath every test still running, which showed up as one unrelated
//! assertion failing on one CI platform and passing everywhere else.
//!
//! Taking this guard serialises those tests against each other and restores whatever
//! the variables held before, so a test leaves the process as it found it.

use std::sync::{Mutex, MutexGuard, OnceLock};

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub struct EnvGuard {
    // Held for the lifetime of the guard; dropping it releases the lock.
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    /// Set (or, with `None`, unset) each variable for as long as the guard lives.
    pub fn set(vars: &[(&str, Option<&str>)]) -> Self {
        let lock = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            // A test that panicked while holding the lock poisoned it; the environment
            // is still restored by that test's Drop, so carrying on is correct.
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let saved = vars
            .iter()
            .map(|(k, _)| ((*k).to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            apply(k, v.as_deref());
        }
        EnvGuard { _lock: lock, saved }
    }

    /// Set `RUNGBOT_OFFLINE=1`, the common case.
    pub fn offline() -> Self {
        Self::set(&[("RUNGBOT_OFFLINE", Some("1"))])
    }
}

fn apply(key: &str, value: Option<&str>) {
    match value {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            apply(k, v.as_deref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_restores_what_was_there_before() {
        std::env::set_var("RUNGBOT_TESTENV_A", "original");
        {
            let _g = EnvGuard::set(&[("RUNGBOT_TESTENV_A", Some("changed"))]);
            assert_eq!(std::env::var("RUNGBOT_TESTENV_A").unwrap(), "changed");
        }
        assert_eq!(
            std::env::var("RUNGBOT_TESTENV_A").unwrap(),
            "original",
            "the guard put back what it found"
        );
        std::env::remove_var("RUNGBOT_TESTENV_A");
    }

    #[test]
    fn a_variable_that_was_unset_goes_back_to_unset() {
        std::env::remove_var("RUNGBOT_TESTENV_B");
        {
            let _g = EnvGuard::set(&[("RUNGBOT_TESTENV_B", Some("x"))]);
            assert!(std::env::var("RUNGBOT_TESTENV_B").is_ok());
        }
        assert!(
            std::env::var("RUNGBOT_TESTENV_B").is_err(),
            "an absent variable must not be left set"
        );
    }

    #[test]
    fn a_guard_can_also_unset_for_the_duration() {
        std::env::set_var("RUNGBOT_TESTENV_C", "present");
        {
            let _g = EnvGuard::set(&[("RUNGBOT_TESTENV_C", None)]);
            assert!(std::env::var("RUNGBOT_TESTENV_C").is_err());
        }
        assert_eq!(std::env::var("RUNGBOT_TESTENV_C").unwrap(), "present");
        std::env::remove_var("RUNGBOT_TESTENV_C");
    }
}
