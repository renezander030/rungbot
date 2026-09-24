//! The two layers a live run calls that live elsewhere: the monthly-capital deploy
//! layer and the daily book audit.
//!
//! A run calls them at fixed points, only when it is not a dry run and trades live:
//!
//! 1. after execution and the sell-policy notices, [`DeployHook::check`], whose results
//!    ride the same mails (mark them `deploy` and `hk`);
//! 2. right after it, once a day (a marker file beside the state), [`AuditHook::run`],
//!    whose findings ride the housekeeping mail as warnings.
//!
//! Both get the run's journal and ladder state to read and write; the run saves them
//! afterwards. A hook that fails reports it: the deploy layer as one `deploy layer: …`
//! error result, the audit as a line on stderr.

use serde_json::Value;

use super::config::RunConfig;
use super::RunResult;
use crate::housekeeping::LadderState;
use crate::journal::Journal;
use crate::reconcile::VenueSource;

/// What a hook may read and change.
pub struct HookCtx<'a> {
    pub cfg: &'a RunConfig,
    /// The run's clock, epoch seconds.
    pub now: f64,
    pub venues: &'a dyn VenueSource,
    pub journal: &'a mut Journal,
    pub ladder: &'a mut LadderState,
    /// The regime reading of this run (`regime-state.json`'s shape).
    pub regime: Option<&'a Value>,
    /// The bull sell policy governs sells this run.
    pub policy_on: bool,
    /// Live placement is blocked this run, and why.
    pub blocked: Option<&'a str>,
}

/// The deploy layer: fresh capital on a venue becomes resting limit-buy zones.
pub trait DeployHook {
    /// Do this run's deploy work and append its results to `execution`.
    fn check(&mut self, ctx: &mut HookCtx, execution: &mut Vec<RunResult>) -> Result<(), String>;
}

/// What a book audit found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditReport {
    /// Findings as warnings, for the housekeeping mail.
    pub results: Vec<RunResult>,
    pub ok: bool,
    pub findings: usize,
    /// Pairs checked per venue, in the order they were checked.
    pub checked: Vec<(String, i64)>,
}

impl AuditReport {
    /// The log line: `AUDIT ok: pairs checked {'gate': 2}` or `AUDIT 3 finding(s): ...`.
    pub fn log_line(&self) -> String {
        let checked: Vec<String> = self
            .checked
            .iter()
            .map(|(k, n)| format!("{}: {n}", crate::pyfmt::repr_str(k)))
            .collect();
        format!(
            "AUDIT {}: pairs checked {{{}}}",
            if self.ok {
                "ok".to_string()
            } else {
                format!("{} finding(s)", self.findings)
            },
            checked.join(", ")
        )
    }
}

/// The book audit: journal rows against the venues' open orders and locked balances.
pub trait AuditHook {
    /// Audit now. `None` when no audit is wired in: the run then neither audits nor
    /// touches the daily marker.
    fn run(&mut self, ctx: &mut HookCtx) -> Option<Result<AuditReport, String>>;
}

/// No deploy layer: capital is laddered by hand.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDeploy;

impl DeployHook for NoDeploy {
    fn check(&mut self, _: &mut HookCtx, _: &mut Vec<RunResult>) -> Result<(), String> {
        Ok(())
    }
}

/// No book audit.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAudit;

impl AuditHook for NoAudit {
    fn run(&mut self, _: &mut HookCtx) -> Option<Result<AuditReport, String>> {
        None
    }
}
