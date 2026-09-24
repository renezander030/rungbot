//! `rungbot research <stage> ...`: argument parsing and wiring of the real clients.
//!
//! The `rungbot` binary reads the config file, builds [`Settings`], and hands the rest
//! of the command line here together with a way to read the market regime.

use std::path::PathBuf;

use rungbot_notify::EmailConfig;

use crate::clients::{
    resolve_secret, EmailMailer, ExaSearch, HttpFetch, LlmCommand, TickTickSink, TICKTICK_API,
    TICKTICK_LINK,
};
use crate::date::Date;
use crate::report::{self, Logger, Mailer, NoteSink, RegimeRead};
use crate::theses::{Step, Theses};
use crate::{
    catalyst, oppscan, survivor, unlocks, Console, Ctx, Json, Llm, Paths, ResearchError, Result,
    StdConsole,
};

pub const USAGE: &str = "\
rungbot research — weekly research. Never trades, never wired to the ladder.

USAGE:
  rungbot research [--config PATH] [--json] [--llm CMD]      the one-shot value screen
  rungbot research oppscan  [--min-rank N] [--max-rank N] [--min-vol N]
                            [--min-dd PCT] [--max-dd PCT] [--limit N] [--json]
  rungbot research survivor build-index | select | run
  rungbot research catalyst search | synthesize
  rungbot research unlocks  build-index | enrich [--now EPOCH]
  rungbot research report   [--refresh] [--no-synth] [--regime bear|chop|bull] [--commit]
  rungbot research theses   [--example]

COMMON OPTIONS:
  --config PATH    the rungbot config (its `research:` block)
  --dir PATH       the data directory for the ledger and the indexes
  --today DATE     date the run YYYY-MM-DD instead of the local calendar day

`report` without --commit is a dry run: it runs the steps and prints the note and
the email, and sends nothing. The LLM stages (`survivor run`, `catalyst synthesize`)
need research.llm.command; its spend cap is always passed.
";

/// Where the report's note goes.
#[derive(Debug, Clone, PartialEq)]
pub struct NotesConfig {
    pub project_id: String,
    /// Environment variable holding the access token.
    pub token_env: String,
    /// A JSON file with an `accessToken` field, read when the variable is unset.
    pub token_file: Option<String>,
    pub api_base: String,
    pub link_template: String,
}

impl NotesConfig {
    pub fn new(project_id: impl Into<String>) -> Self {
        NotesConfig {
            project_id: project_id.into(),
            token_env: "TICKTICK_TOKEN".into(),
            token_file: None,
            api_base: TICKTICK_API.into(),
            link_template: TICKTICK_LINK.into(),
        }
    }

    pub fn token(&self) -> Option<String> {
        if let Ok(v) = std::env::var(&self.token_env) {
            if !v.is_empty() {
                return Some(v);
            }
        }
        let path = crate::clients::expand_home(self.token_file.as_deref()?);
        let text = std::fs::read_to_string(path).ok()?;
        crate::py::parse(&text)
            .ok()?
            .get_some("accessToken")
            .map(Json::display)
    }
}

/// Everything the research commands need from the config.
#[derive(Debug, Clone)]
pub struct Settings {
    pub dir: PathBuf,
    pub theses: Theses,
    /// `None` until `research.llm.command` is set.
    pub llm: Option<LlmCommand>,
    pub exa: ExaSearch,
    pub email: Option<EmailConfig>,
    pub notes: Option<NotesConfig>,
    pub log_file: Option<PathBuf>,
    /// The email button's text.
    pub link_label: String,
}

pub const DEFAULT_LINK_LABEL: &str = "Open the note";

impl Settings {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Settings {
            dir: dir.into(),
            theses: Theses::builtin(),
            llm: None,
            exa: ExaSearch::default(),
            email: None,
            notes: None,
            log_file: None,
            link_label: DEFAULT_LINK_LABEL.into(),
        }
    }
}

/// A sink that cannot send because its token is missing: every create fails, loudly.
struct MissingToken(String);

impl NoteSink for MissingToken {
    fn create(&self, _: &str, _: &str) -> std::result::Result<Option<String>, String> {
        Err(format!("no token: set {}", self.0))
    }
    fn link(&self, _: &str) -> Option<String> {
        None
    }
}

struct Flags {
    positional: Vec<String>,
    flags: Vec<(String, String)>,
}

impl Flags {
    fn parse(argv: &[String], valued: &[&str]) -> std::result::Result<Flags, String> {
        let mut it = argv.iter();
        let (mut positional, mut flags) = (Vec::new(), Vec::new());
        while let Some(a) = it.next() {
            let Some(bare) = a.strip_prefix("--") else {
                positional.push(a.clone());
                continue;
            };
            if let Some((k, v)) = bare.split_once('=') {
                flags.push((k.to_string(), v.to_string()));
            } else if valued.contains(&bare) {
                let v = it.next().ok_or_else(|| format!("--{bare} needs a value"))?;
                flags.push((bare.to_string(), v.clone()));
            } else {
                flags.push((bare.to_string(), "1".into()));
            }
        }
        Ok(Flags { positional, flags })
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    }

    fn has(&self, k: &str) -> bool {
        self.get(k).is_some()
    }

    fn check(&self, allowed: &[&str]) -> std::result::Result<(), String> {
        match self
            .flags
            .iter()
            .find(|(k, _)| !allowed.contains(&k.as_str()))
        {
            Some((k, _)) => Err(format!("unknown option --{k}")),
            None => Ok(()),
        }
    }

    fn num<T: std::str::FromStr>(&self, k: &str, default: T) -> std::result::Result<T, String> {
        match self.get(k) {
            Some(v) => v
                .trim()
                .parse()
                .map_err(|_| format!("--{k}: invalid value {v:?}")),
            None => Ok(default),
        }
    }
}

const VALUED: [&str; 12] = [
    "min-rank", "max-rank", "min-vol", "min-dd", "max-dd", "limit", "now", "regime", "today",
    "dir", "config", "llm",
];
const COMMON: [&str; 3] = ["today", "dir", "config"];

/// Run `rungbot research <stage> ...`. Returns the process exit code.
pub fn run(
    argv: &[String],
    settings: &Settings,
    regime: &dyn Fn() -> std::result::Result<RegimeRead, String>,
) -> u8 {
    match dispatch(argv, settings, regime, &mut StdConsole) {
        Ok(()) => 0,
        Err(ResearchError::Config(m)) => {
            eprintln!("config error: {m}");
            2
        }
        Err(ResearchError::Exit(m)) | Err(ResearchError::Failed(m)) => {
            eprintln!("{m}");
            1
        }
    }
}

fn usage_err(m: impl Into<String>) -> ResearchError {
    ResearchError::Config(format!("{}\n\n{USAGE}", m.into()))
}

/// Run one pipeline step as its own command would.
pub fn run_step(ctx: &Ctx, step: Step, now: i64, con: &mut dyn Console) -> Result<()> {
    match step {
        Step::Oppscan => oppscan::run(ctx, &oppscan::Args::default(), con).map(|_| ()),
        Step::CatalystSearch => catalyst::search(ctx, con),
        Step::CatalystSynthesize => catalyst::synthesize(ctx, con),
        Step::SurvivorBuildIndex => survivor::build_index(ctx, con),
        Step::SurvivorSelect => survivor::select(ctx, con),
        Step::SurvivorRun => survivor::run_synth(ctx, con),
        Step::UnlocksBuildIndex => unlocks::build_index(ctx, con).map(|_| ()),
        Step::UnlocksEnrich => unlocks::enrich(ctx, now, con),
    }
}

pub fn dispatch(
    argv: &[String],
    settings: &Settings,
    regime: &dyn Fn() -> std::result::Result<RegimeRead, String>,
    con: &mut dyn Console,
) -> Result<()> {
    if argv
        .iter()
        .any(|a| a == "help" || a == "--help" || a == "-h")
    {
        con.out(USAGE.trim_end());
        return Ok(());
    }
    let f = Flags::parse(argv, &VALUED).map_err(usage_err)?;
    let Some(stage) = f.positional.first().cloned() else {
        return Err(usage_err("which research stage?"));
    };
    let sub = f.positional.get(1).cloned().unwrap_or_default();
    if f.positional.len() > 2 {
        return Err(usage_err(format!(
            "unexpected argument {:?}",
            f.positional[2]
        )));
    }
    let today = match f.get("today") {
        Some(t) => Date::parse(t)
            .ok_or_else(|| usage_err(format!("--today: expected YYYY-MM-DD, got {t:?}")))?,
        None => Date::today_local(),
    };
    let dir = f
        .get("dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| settings.dir.clone());
    let llm = settings.llm.as_ref().map(|l| l as &dyn Llm);
    let ctx = Ctx {
        paths: Paths::new(dir),
        fetch: &HttpFetch,
        search: &settings.exa,
        llm,
        today,
    };
    let allow = |extra: &[&str]| -> Result<()> {
        let mut all: Vec<&str> = COMMON.to_vec();
        all.extend_from_slice(extra);
        f.check(&all).map_err(usage_err)
    };
    let needs_sub = |subs: &[&str]| -> Result<()> {
        if subs.contains(&sub.as_str()) {
            Ok(())
        } else {
            Err(usage_err(format!(
                "research {stage}: expected one of {}",
                subs.join(" | ")
            )))
        }
    };

    match stage.as_str() {
        "oppscan" => {
            allow(&[
                "min-rank", "max-rank", "min-vol", "min-dd", "max-dd", "limit", "json",
            ])?;
            let d = oppscan::Args::default();
            let a = oppscan::Args {
                min_rank: f.num("min-rank", d.min_rank).map_err(usage_err)?,
                max_rank: f.num("max-rank", d.max_rank).map_err(usage_err)?,
                min_vol: f.num("min-vol", d.min_vol).map_err(usage_err)?,
                min_dd: f.num("min-dd", d.min_dd).map_err(usage_err)?,
                max_dd: f.num("max-dd", d.max_dd).map_err(usage_err)?,
                limit: f.num("limit", d.limit).map_err(usage_err)?,
                json: f.has("json"),
            };
            oppscan::run(&ctx, &a, con).map(|_| ())
        }
        "survivor" => {
            allow(&[])?;
            needs_sub(&["build-index", "select", "run"])?;
            survivor::run(&ctx, &sub, con)
        }
        "catalyst" => {
            allow(&[])?;
            needs_sub(&["search", "synthesize"])?;
            catalyst::run(&ctx, &sub, con)
        }
        "unlocks" => {
            allow(&["now"])?;
            needs_sub(&["build-index", "enrich"])?;
            let now = f.num("now", crate::date::now_epoch()).map_err(usage_err)?;
            unlocks::run(&ctx, &sub, now, con)
        }
        "theses" => {
            allow(&["example"])?;
            if f.has("example") {
                con.out(crate::theses::EXAMPLE_YAML.trim_end());
                return Ok(());
            }
            for t in &settings.theses.theses {
                con.out(&format!(
                    "{:<6} {}  refresh: {}  synth: {}  rank by: {}",
                    t.regime,
                    t.title,
                    t.refresh
                        .iter()
                        .map(Step::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                    t.synth
                        .iter()
                        .map(Step::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                    t.score_key
                ));
            }
            con.out(&format!(
                "an unknown regime falls back to {}",
                settings.theses.fallback
            ));
            Ok(())
        }
        "report" => {
            allow(&["commit", "refresh", "no-synth", "regime"])?;
            let forced = f.get("regime").map(str::to_lowercase);
            if let Some(r) = &forced {
                if settings.theses.get(r).is_none() {
                    return Err(usage_err(format!(
                        "--regime must be one of {}",
                        settings.theses.names().join(", ")
                    )));
                }
            }
            let opts = report::Opts {
                commit: f.has("commit"),
                refresh: f.has("refresh"),
                no_synth: f.has("no-synth"),
                regime: forced,
            };
            let now = crate::date::now_epoch();
            let mut step = |s: Step, con: &mut dyn Console| {
                if let Err(e) = run_step(&ctx, s, now, con) {
                    con.err(&format!("step `{s}` failed: {e}"));
                }
            };
            let sink_box: Option<Box<dyn NoteSink>> =
                settings.notes.as_ref().map(|n| match n.token() {
                    Some(token) => Box::new(TickTickSink {
                        project_id: n.project_id.clone(),
                        token,
                        api_base: n.api_base.clone(),
                        link_template: n.link_template.clone(),
                    }) as Box<dyn NoteSink>,
                    None => Box::new(MissingToken(n.token_env.clone())),
                });
            let mailer = settings
                .email
                .clone()
                .map(|e| EmailMailer(e.with_env_overrides()));
            let logger = Logger::system(settings.log_file.clone());
            let deps = report::Deps {
                theses: &settings.theses,
                regime,
                step: &mut step,
                sink: sink_box.as_deref(),
                mailer: mailer.as_ref().map(|m| m as &dyn Mailer),
                logger: &logger,
                link_label: &settings.link_label,
            };
            report::run(&ctx, &opts, deps, con).map(|_| ())
        }
        other => Err(usage_err(format!("unknown research stage {other:?}"))),
    }
}

/// `true` when a key for the search API can be found, for a friendly preflight.
pub fn search_key_present(exa: &ExaSearch) -> bool {
    resolve_secret(&exa.key_env, exa.key_file.as_deref()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BufConsole;

    fn run_capture(argv: &[&str], settings: &Settings) -> (Result<()>, BufConsole) {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let mut con = BufConsole::default();
        let r = dispatch(&argv, settings, &|| Err("no regime".into()), &mut con);
        (r, con)
    }

    #[test]
    fn unknown_stages_and_steps_are_usage_errors() {
        let s = Settings::new(std::env::temp_dir());
        assert!(matches!(
            run_capture(&["nope"], &s).0,
            Err(ResearchError::Config(_))
        ));
        assert!(matches!(
            run_capture(&["survivor", "sprint"], &s).0,
            Err(ResearchError::Config(_))
        ));
        assert!(matches!(
            run_capture(&["oppscan", "--bogus"], &s).0,
            Err(ResearchError::Config(_))
        ));
        assert!(matches!(
            run_capture(&["report", "--regime", "moon"], &s).0,
            Err(ResearchError::Config(_))
        ));
    }

    #[test]
    fn theses_example_prints_the_editable_file() {
        let s = Settings::new(std::env::temp_dir());
        let (r, con) = run_capture(&["theses", "--example"], &s);
        r.unwrap();
        assert!(con.out.starts_with("# Which screen runs"));
        let (r, con) = run_capture(&["theses"], &s);
        r.unwrap();
        assert!(con
            .out
            .contains("bull   Catalyst Watch  refresh: oppscan, catalyst search"));
    }

    #[test]
    fn the_llm_stages_refuse_without_a_command() {
        let dir = std::env::temp_dir().join(format!("rungbot-research-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::LEDGER), "[]").unwrap();
        std::fs::write(dir.join(crate::VALUE_INDEX), "{}").unwrap();
        let s = Settings::new(&dir);
        for argv in [["catalyst", "synthesize"], ["survivor", "run"]] {
            match run_capture(&argv, &s).0 {
                Err(ResearchError::Config(m)) => assert!(m.contains("research.llm.command"), "{m}"),
                other => panic!("{other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_index_stops_select_with_the_next_command() {
        let dir =
            std::env::temp_dir().join(format!("rungbot-research-noidx-{}", std::process::id()));
        let s = Settings::new(&dir);
        match run_capture(&["survivor", "select"], &s).0 {
            Err(ResearchError::Exit(m)) => {
                assert_eq!(
                    m,
                    "no value-index.json — run: rungbot research survivor build-index"
                )
            }
            other => panic!("{other:?}"),
        }
    }
}
