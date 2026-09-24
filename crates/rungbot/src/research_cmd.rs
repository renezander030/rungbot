//! `rungbot research <stage>`: the weekly research pipeline.
//!
//! This module only turns the config's `research:` block (and an optional theses file)
//! into [`Settings`] and hands over to `rungbot-research`. The bare `rungbot research`
//! one-shot screen stays in `main.rs`.
//!
//! ```yaml
//! research:
//!   dir: ~/.local/state/rungbot/research   # ledger + indexes
//!   theses: ~/.config/rungbot/theses.yaml  # optional; `rungbot research theses --example`
//!   log_file: ~/.local/state/rungbot/research.log
//!   link_label: Open the note
//!   llm:
//!     command: "my-llm --print"            # prompt on stdin, answer on stdout
//!     max_budget_usd: 0.10                 # always passed, as `<budget_flag> <amount>`
//!     budget_flag: --max-budget-usd
//!     timeout_s: 120
//!     env_remove: "VAR_A, VAR_B"
//!   search:
//!     key_env: EXA_API_KEY
//!     key_file: ~/.config/exa.env
//!   email:
//!     from: alerts@example.com
//!     to: me@example.com
//!     api_key_env: RESEND_API_KEY
//!   notes:
//!     project_id: "<task project id>"
//!     token_env: TICKTICK_TOKEN
//! ```

use std::path::{Path, PathBuf};

use rungbot_core::ConfigError;
use rungbot_notify::EmailConfig;
use rungbot_research::cli::{NotesConfig, Settings};
use rungbot_research::clients::{
    expand_home, ExaSearch, LlmCommand, DEFAULT_BUDGET_FLAG, DEFAULT_MAX_BUDGET_USD,
};
use rungbot_research::report::RegimeRead;
use rungbot_research::theses::{Node, Theses};

use crate::yaml::{self, Yaml};

fn cerr<T>(m: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError(m.into()))
}

fn text(n: &Yaml, k: &str) -> Option<String> {
    n.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

fn number(n: &Yaml, k: &str, what: &str) -> Result<Option<f64>, ConfigError> {
    match n.get(k) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| ConfigError(format!("`{what}` must be a number"))),
    }
}

fn path(n: &Yaml, k: &str) -> Option<PathBuf> {
    text(n, k).map(|p| expand_home(&p))
}

/// A YAML tree as the theses reader wants it.
fn to_node(y: &Yaml, at: &str) -> Result<Node, ConfigError> {
    match y {
        Yaml::Map(e) => e
            .iter()
            .map(|(k, v)| Ok((k.clone(), to_node(v, &format!("{at}.{k}"))?)))
            .collect::<Result<Vec<_>, _>>()
            .map(Node::Map),
        Yaml::Null => Ok(Node::Text(String::new())),
        Yaml::Flow(raw) => cerr(format!(
            "`{at}: {raw}` uses inline YAML; write a comma-separated value or a block"
        )),
        other => Ok(Node::Text(other.as_str().unwrap_or_default())),
    }
}

pub fn load_theses(file: &Path) -> Result<Theses, ConfigError> {
    let body = std::fs::read_to_string(file)
        .map_err(|e| ConfigError(format!("cannot read {}: {e}", file.display())))?;
    let doc = yaml::parse(&body).map_err(|e| ConfigError(format!("{}: {e}", file.display())))?;
    Theses::from_node(&to_node(&doc, "theses file")?)
        .map_err(|e| ConfigError(format!("{}: {e}", file.display())))
}

/// Settings from the `research:` block; every part is optional.
pub fn settings_from(node: Option<&Yaml>) -> Result<Settings, ConfigError> {
    let mut s = Settings::new(crate::state::default_research_dir());
    let Some(n) = node else { return Ok(s) };
    if let Some(d) = path(n, "dir") {
        s.dir = d;
    }
    if let Some(t) = path(n, "theses") {
        s.theses = load_theses(&t)?;
    }
    s.log_file = path(n, "log_file");
    if let Some(l) = text(n, "link_label") {
        s.link_label = l;
    }

    if let Some(llm) = n.get("llm") {
        if let Some(cmd) = text(llm, "command") {
            let flag = text(llm, "budget_flag").unwrap_or_else(|| DEFAULT_BUDGET_FLAG.into());
            let cap = number(llm, "max_budget_usd", "research.llm.max_budget_usd")?
                .unwrap_or(DEFAULT_MAX_BUDGET_USD);
            let mut c = LlmCommand::new(&cmd, &flag, cap).map_err(ConfigError)?;
            if let Some(t) = number(llm, "timeout_s", "research.llm.timeout_s")? {
                if t.is_nan() || t < 1.0 {
                    return cerr("`research.llm.timeout_s` must be at least 1");
                }
                c.timeout_s = t as u64;
            }
            c.env_remove = text(llm, "env_remove")
                .map(|v| {
                    v.split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            s.llm = Some(c);
        }
    }

    if let Some(sr) = n.get("search") {
        s.exa = ExaSearch {
            key_env: text(sr, "key_env").unwrap_or(s.exa.key_env),
            key_file: text(sr, "key_file"),
            ..s.exa
        };
    }

    if let Some(e) = n.get("email") {
        let (Some(from), Some(to)) = (text(e, "from"), text(e, "to")) else {
            return cerr("`research.email` needs both `from` and `to`");
        };
        let mut c = EmailConfig::new(from, to);
        if let Some(k) = text(e, "api_key_env") {
            c.api_key_env = k;
        }
        c.api_key_file = text(e, "api_key_file");
        s.email = Some(c);
    }

    if let Some(nt) = n.get("notes") {
        let Some(project) = text(nt, "project_id") else {
            return cerr("`research.notes` needs `project_id`");
        };
        let mut c = NotesConfig::new(project);
        if let Some(v) = text(nt, "token_env") {
            c.token_env = v;
        }
        c.token_file = text(nt, "token_file");
        if let Some(v) = text(nt, "api_base") {
            c.api_base = v;
        }
        if let Some(v) = text(nt, "link_template") {
            c.link_template = v;
        }
        s.notes = Some(c);
    }
    Ok(s)
}

/// Pull `--config PATH` out of the research arguments; the rest go to the stage.
fn split_config(argv: &[String]) -> (Option<String>, Vec<String>) {
    let (mut cfg, mut rest) = (None, Vec::new());
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == "--config" {
            cfg = it.next().cloned();
        } else if let Some(p) = a.strip_prefix("--config=") {
            cfg = Some(p.to_string());
        } else {
            rest.push(a.clone());
        }
    }
    (cfg, rest)
}

/// `rungbot research <stage> ...`. Returns the exit code.
pub fn run(argv: &[String]) -> u8 {
    let (cfg_arg, rest) = split_config(argv);
    let cfg_path = cfg_arg
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(crate::state::default_config_path);
    // Without a config file the keyless stages still run on defaults; an explicitly
    // named config that is missing or broken is an error.
    let (doc, cli) = if cfg_path.exists() || cfg_arg.is_some() {
        let text = match std::fs::read_to_string(&cfg_path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("config error: cannot read {}: {e}", cfg_path.display());
                return 2;
            }
        };
        let doc = match yaml::parse(&text) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("config error: {e}");
                return 2;
            }
        };
        (Some(doc), crate::config_file::from_str(&text).ok())
    } else {
        (None, None)
    };
    let settings = match settings_from(doc.as_ref().and_then(|d| d.get("research"))) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("config error: {}", e.0);
            return 2;
        }
    };
    let regime = move || -> Result<RegimeRead, String> {
        let cfg = cli
            .as_ref()
            .ok_or_else(|| "no usable config with coins to read the regime from".to_string())?;
        let r = crate::read_regime(cfg).map_err(|e| match e {
            crate::Failure::Config(m) | crate::Failure::Prices(m) | crate::Failure::Other(m) => m,
            crate::Failure::Exit(code) => format!("regime read exited with code {code}"),
        })?;
        Ok(regime_read(&r))
    };
    rungbot_research::cli::run(&rest, &settings, &regime)
}

/// The regime as the report reads it: the label, and each coin's basing signals (none
/// for a coin whose candles failed).
pub fn regime_read(r: &rungbot_core::regime::Regime) -> RegimeRead {
    RegimeRead {
        market: r.market.as_str().to_string(),
        coins: r
            .coins
            .iter()
            .map(|c| {
                let sig = c.error.is_none().then(|| {
                    vec![
                        ("above_sma30".to_string(), c.signals.above_sma30),
                        ("ret30_strong".to_string(), c.signals.ret30_strong),
                        ("fresh_30d_high".to_string(), c.signals.fresh_30d_high),
                        ("higher_lows".to_string(), c.signals.higher_lows),
                    ]
                });
                (c.sym.clone(), sig)
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_theses_file_reads_back_as_the_builtin_set() {
        let dir = std::env::temp_dir().join(format!("rungbot-theses-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("theses.yaml");
        std::fs::write(&f, rungbot_research::theses::EXAMPLE_YAML).unwrap();
        assert_eq!(load_theses(&f).unwrap(), Theses::builtin());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_research_block_builds_every_part() {
        let doc = yaml::parse(
            "research:\n  dir: /tmp/r\n  link_label: Open it\n  llm:\n    command: my-llm -p\n    \
             max_budget_usd: 0.5\n    timeout_s: 30\n    env_remove: A, B\n  search:\n    \
             key_env: MY_SEARCH\n  email:\n    from: a@example.com\n    to: b@example.com\n  \
             notes:\n    project_id: p1\n",
        )
        .unwrap();
        let s = settings_from(doc.get("research")).unwrap();
        assert_eq!(s.dir, PathBuf::from("/tmp/r"));
        assert_eq!(s.link_label, "Open it");
        let llm = s.llm.unwrap();
        assert_eq!(
            llm.argv(),
            vec!["my-llm", "-p", "--max-budget-usd", "0.5"],
            "the cap is always appended"
        );
        assert_eq!((llm.timeout_s, llm.env_remove.len()), (30, 2));
        assert_eq!(s.exa.key_env, "MY_SEARCH");
        assert!(s.email.is_some());
        assert_eq!(s.notes.unwrap().project_id, "p1");
    }

    #[test]
    fn no_llm_command_means_no_llm_and_a_zero_cap_is_refused() {
        let s = settings_from(None).unwrap();
        assert!(s.llm.is_none(), "there is no default LLM command");
        let doc =
            yaml::parse("research:\n  llm:\n    command: x\n    max_budget_usd: 0\n").unwrap();
        assert!(settings_from(doc.get("research")).is_err());
    }

    #[test]
    fn a_half_configured_email_or_note_sink_is_an_error() {
        let doc = yaml::parse("research:\n  email:\n    from: a@example.com\n").unwrap();
        assert!(settings_from(doc.get("research")).is_err());
        let doc = yaml::parse("research:\n  notes:\n    token_env: X\n").unwrap();
        assert!(settings_from(doc.get("research")).is_err());
    }
}
