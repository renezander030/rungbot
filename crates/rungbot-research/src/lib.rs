//! `rungbot-research` — the weekly research pipeline behind `rungbot research`.
//!
//! Research only. Nothing here can place an order or reach a venue's private API:
//! the crate holds no venue key and is not a dependency of the executor.
//!
//! The pipeline, stage by stage, all composing through one ledger file
//! (`opportunity-ledger.json`) in a data directory:
//!
//! * [`oppscan`] — the technical half: CoinPaprika's whole market in one call, kept to a
//!   tradable band and ranked by distance from the all-time high.
//! * [`survivor`] — the bear/chop screen: a DefiLlama value index (fees, TVL, category),
//!   a value-first selection, then web evidence and an LLM verdict per coin.
//! * [`catalyst`] — the bull screen: web evidence of a forward catalyst, then an LLM
//!   verdict per coin.
//! * [`unlocks`] — a token-unlock calendar from DefiLlama's open dataset CDN.
//! * [`theses`] — which of those runs in which market regime, and how it is titled.
//! * [`report`] — reads the regime, runs the matching thesis, and writes one dated note
//!   plus a short HTML email.
//!
//! Every outside call goes through a trait ([`Fetch`], [`Search`], [`Llm`],
//! [`report::NoteSink`], [`report::Mailer`]) so the golden tests replay recorded
//! responses and never touch the network. The real clients live in [`clients`].
//!
//! ## The LLM command
//!
//! No model provider is built in. You name a command in the config; the prompt goes to
//! its stdin and its stdout is the answer. The command always gets a spend cap appended
//! (`--max-budget-usd <N>` by default; both the flag and the amount are configurable,
//! neither can be switched off), and nothing grants it extra permissions: whatever
//! flags your tool needs, you write them into the command yourself.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

use std::io::Write;
use std::path::{Path, PathBuf};

pub mod catalyst;
pub mod cli;
pub mod clients;
pub mod date;
#[cfg(test)]
mod golden_tests;
pub mod oppscan;
pub mod py;
pub mod report;
pub mod survivor;
#[cfg(test)]
mod testenv;
pub mod theses;
pub mod unlocks;

pub use py::Json;

/// The ledger every stage reads and writes.
pub const LEDGER: &str = "opportunity-ledger.json";
/// `SYMBOL -> {name, tvl, category, gecko_id, fees30d, revenue30d}` from DefiLlama.
pub const VALUE_INDEX: &str = "value-index.json";
/// `SYMBOL -> {slug, gecko_id}` for the unlock calendar.
pub const UNLOCK_INDEX: &str = "unlock-index.json";

/// The User-Agent the public data sources were read with.
pub const UA_SCAN: &str = "oppscan/0.1";
pub const UA_BROWSER_SCAN: &str = "Mozilla/5.0 oppscan";

#[derive(Debug, Clone, PartialEq)]
pub enum ResearchError {
    /// A stage refused to run, with the message to print (exit 1).
    Exit(String),
    /// The configuration cannot do what was asked (exit 2).
    Config(String),
    /// A read, a parse or a write failed (exit 1).
    Failed(String),
}

impl std::fmt::Display for ResearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResearchError::Exit(m) | ResearchError::Config(m) | ResearchError::Failed(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for ResearchError {}

pub type Result<T> = std::result::Result<T, ResearchError>;

/// A keyless JSON GET.
pub trait Fetch {
    fn get_json(
        &self,
        url: &str,
        user_agent: &str,
        timeout_s: u64,
    ) -> std::result::Result<Json, String>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum SearchError {
    /// No search key is configured. Each stage words this itself.
    NoKey,
    /// The call failed; the text is `Kind: message`.
    Failed(String),
}

/// A web search: one JSON request body in, the parsed JSON response out.
pub trait Search {
    fn search(&self, body: &str) -> std::result::Result<Json, SearchError>;
}

/// An LLM behind a command: the prompt in, raw stdout out. An `Err` is `Kind: message`.
pub trait Llm {
    fn ask(&self, prompt: &str) -> std::result::Result<String, String>;
}

/// Where a stage prints. The real one is stdout/stderr; tests capture it.
pub trait Console {
    fn out(&mut self, line: &str);
    fn err(&mut self, line: &str);
}

/// stdout and stderr, one `print` per call.
pub struct StdConsole;

impl Console for StdConsole {
    fn out(&mut self, line: &str) {
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "{line}");
    }
    fn err(&mut self, line: &str) {
        let mut e = std::io::stderr().lock();
        let _ = writeln!(e, "{line}");
    }
}

/// Everything a stage printed, for tests.
#[derive(Debug, Default, Clone)]
pub struct BufConsole {
    pub out: String,
    pub err: String,
}

impl Console for BufConsole {
    fn out(&mut self, line: &str) {
        self.out.push_str(line);
        self.out.push('\n');
    }
    fn err(&mut self, line: &str) {
        self.err.push_str(line);
        self.err.push('\n');
    }
}

/// The data directory the ledger and indexes live in.
#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Paths { dir: dir.into() }
    }
    pub fn ledger(&self) -> PathBuf {
        self.dir.join(LEDGER)
    }
    pub fn value_index(&self) -> PathBuf {
        self.dir.join(VALUE_INDEX)
    }
    pub fn unlock_index(&self) -> PathBuf {
        self.dir.join(UNLOCK_INDEX)
    }
}

/// Read and parse a JSON file.
pub fn read_json(path: &Path) -> Result<Json> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ResearchError::Failed(format!("cannot read {}: {e}", path.display())))?;
    py::parse(&text)
        .map_err(|e| ResearchError::Failed(format!("{} is not valid JSON: {e}", path.display())))
}

/// Write `value` as indented JSON, through a temp file and a rename so a reader never
/// sees half a file.
pub fn write_json(path: &Path, value: &Json) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| {
            ResearchError::Failed(format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, py::dumps_indent(value, 2))
        .map_err(|e| ResearchError::Failed(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ResearchError::Failed(format!("cannot replace {}: {e}", path.display()))
    })
}

/// The ledger as a list of records.
pub fn read_ledger(paths: &Paths) -> Result<Vec<Json>> {
    match read_json(&paths.ledger())? {
        Json::Arr(a) => Ok(a),
        _ => Err(ResearchError::Failed(format!(
            "{} is not a list",
            paths.ledger().display()
        ))),
    }
}

pub fn write_ledger(paths: &Paths, cands: &[Json]) -> Result<()> {
    write_json(&paths.ledger(), &Json::Arr(cands.to_vec()))
}

/// Indices of `cands` ordered by `dislocation_score`, deepest first; equal scores keep
/// their ledger order.
pub fn by_dislocation(cands: &[Json]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..cands.len()).collect();
    let score = |i: usize| {
        cands[i]
            .get("dislocation_score")
            .and_then(Json::as_f64)
            .unwrap_or(0.0)
    };
    idx.sort_by(|&a, &b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    idx
}

/// The shared pieces of a stage that reaches outside.
pub struct Ctx<'a> {
    pub paths: Paths,
    pub fetch: &'a dyn Fetch,
    pub search: &'a dyn Search,
    /// `None` when no LLM command is configured; the synthesis stages refuse to run.
    pub llm: Option<&'a dyn Llm>,
    /// The local calendar date the run is dated with.
    pub today: date::Date,
}

/// The message a synthesis stage stops with when no LLM command is configured.
pub fn no_llm() -> ResearchError {
    ResearchError::Config(
        "no LLM command configured: set research.llm.command in the config \
         (the spend cap research.llm.max_budget_usd is always passed to it)"
            .into(),
    )
}
