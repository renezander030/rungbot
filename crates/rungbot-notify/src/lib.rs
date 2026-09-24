//! `rungbot-notify` — getting a message to a person, shared by the `rungbot` and
//! `rungbot-exec` binaries.
//!
//! Three channels, each a plain HTTPS POST:
//!
//! * [`email`] — the Resend HTTP API, text with an optional HTML part.
//! * [`telegram`] — a bot `sendMessage`, plain text.
//! * [`webhook`] — a JSON POST to any URL.
//!
//! [`Notifier`] is the config block that names them, deserializable from YAML. Secrets
//! are never part of it: the config names the **environment variable** that holds a
//! token, so a config file stays safe to paste into an issue, and no `Debug` output or
//! error message in this crate carries a token.
//!
//! [`signal_notices`] is the dedupe that decides which ladder signals are worth a mail.
//!
//! Every send honours `RUNGBOT_OFFLINE=1`: the request is built, then refused before it
//! leaves the process. That is what lets tests assert the exact bytes of a request
//! without ever sending one.
//!
//! This crate holds no venue key and signs nothing. A failed send is a value, never a
//! panic: a notification that cannot go out must not take the run down with it.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

use std::fmt;

pub mod config;
pub mod email;
#[cfg(test)]
mod golden_tests;
pub mod http;
pub mod pyjson;
pub mod signal_notices;
pub mod telegram;
#[cfg(test)]
mod testenv;
pub mod webhook;

pub use config::{Delivery, Notifier};
pub use email::{EmailConfig, EmailMessage};
pub use http::{HttpRequest, HttpResponse};
pub use telegram::TelegramConfig;
pub use webhook::WebhookConfig;

/// The User-Agent every request sends unless its channel config names another.
pub const USER_AGENT: &str = concat!(
    "rungbot/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/renezander030/rungbot)"
);

/// The switch that keeps tests and dry runs from reaching the network.
pub const OFFLINE_ENV: &str = "RUNGBOT_OFFLINE";

/// True when `RUNGBOT_OFFLINE=1`, the project-wide convention.
pub fn offline() -> bool {
    std::env::var(OFFLINE_ENV).as_deref() == Ok("1")
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotifyError {
    /// `RUNGBOT_OFFLINE=1` refused the send.
    Offline(String),
    /// The request went out and failed, or could not go out.
    Failed(String),
    /// The channel cannot send as configured (no token, no key, no text).
    Misconfigured(String),
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotifyError::Offline(m) | NotifyError::Failed(m) | NotifyError::Misconfigured(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for NotifyError {}

/// JSON-encode one string the way `serde_json` does (UTF-8 kept as is).
pub fn json_escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}
