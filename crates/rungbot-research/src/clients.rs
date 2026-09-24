//! The real clients behind the research traits.
//!
//! Every one honours `RUNGBOT_OFFLINE=1`: the call is refused before anything leaves
//! the process. Keys are read from the environment variable the config names, or from
//! an env-style file; they are never part of the config and never printed.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rungbot_notify::{EmailConfig, HttpRequest};

use crate::py::{self, Json};
use crate::report::{Mailer, NoteSink};
use crate::{Fetch, Llm, Search, SearchError};

fn offline() -> bool {
    rungbot_notify::offline()
}

/// `~/` expanded against `$HOME`.
pub fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

/// The value of `NAME=` in an env-style file (the line trimmed, the rest after `=`).
pub fn env_file_value(path: &str, name: &str) -> Option<String> {
    let body = std::fs::read_to_string(expand_home(path)).ok()?;
    let prefix = format!("{name}=");
    body.lines()
        .find(|l| l.starts_with(&prefix))
        .map(|l| {
            l.trim()
                .split_once('=')
                .map(|x| x.1)
                .unwrap_or("")
                .to_string()
        })
        .filter(|v| !v.is_empty())
}

/// A secret from an environment variable, else from an env-style file.
pub fn resolve_secret(env: &str, file: Option<&str>) -> Option<String> {
    if let Ok(v) = std::env::var(env) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    env_file_value(file?, env)
}

fn http_error(status: impl std::fmt::Display, reason: &str) -> String {
    format!("HTTPError: HTTP Error {status}: {reason}")
}

/// Keyless GETs with `minreq`.
pub struct HttpFetch;

impl Fetch for HttpFetch {
    fn get_json(&self, url: &str, user_agent: &str, timeout_s: u64) -> Result<Json, String> {
        if offline() {
            return Err(format!(
                "URLError: RUNGBOT_OFFLINE=1 refuses network call: {url}"
            ));
        }
        let resp = minreq::get(url)
            .with_header("User-Agent", user_agent)
            .with_timeout(timeout_s)
            .send()
            .map_err(|e| format!("URLError: {url}: {e}"))?;
        if !(200..300).contains(&resp.status_code) {
            return Err(http_error(resp.status_code, &resp.reason_phrase));
        }
        let body = resp
            .as_str()
            .map_err(|e| format!("ValueError: {url}: non-UTF-8 response: {e}"))?;
        py::parse(body).map_err(|e| format!("JSONDecodeError: {url}: {e}"))
    }
}

pub const EXA_API: &str = "https://api.exa.ai/search";
pub const EXA_TIMEOUT_S: u64 = 30;

/// The exa search API. The key comes from `key_env`, else `key_file`.
#[derive(Debug, Clone)]
pub struct ExaSearch {
    pub endpoint: String,
    pub key_env: String,
    pub key_file: Option<String>,
}

impl Default for ExaSearch {
    fn default() -> Self {
        ExaSearch {
            endpoint: EXA_API.into(),
            key_env: "EXA_API_KEY".into(),
            key_file: None,
        }
    }
}

impl Search for ExaSearch {
    fn search(&self, body: &str) -> Result<Json, SearchError> {
        let key =
            resolve_secret(&self.key_env, self.key_file.as_deref()).ok_or(SearchError::NoKey)?;
        if offline() {
            return Err(SearchError::Failed(
                "URLError: RUNGBOT_OFFLINE=1 refuses network call".into(),
            ));
        }
        let resp = minreq::post(&self.endpoint)
            .with_header("x-api-key", key.as_str())
            .with_header("Content-Type", "application/json")
            .with_body(body)
            .with_timeout(EXA_TIMEOUT_S)
            .send()
            .map_err(|e| {
                SearchError::Failed(format!("URLError: {}", e.to_string().replace(&key, "***")))
            })?;
        if !(200..300).contains(&resp.status_code) {
            return Err(SearchError::Failed(http_error(
                resp.status_code,
                &resp.reason_phrase,
            )));
        }
        let text = resp
            .as_str()
            .map_err(|e| SearchError::Failed(format!("UnicodeDecodeError: {e}")))?;
        py::parse(text).map_err(|e| SearchError::Failed(format!("JSONDecodeError: {e}")))
    }
}

/// The LLM as a command: the prompt on stdin, the answer on stdout.
///
/// The spend cap is not optional: `budget_flag max_budget_usd` is always appended to the
/// arguments. Nothing else is added, so no permission-widening flag is ever passed
/// unless the configured command itself contains it.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmCommand {
    pub program: String,
    pub args: Vec<String>,
    pub budget_flag: String,
    pub max_budget_usd: f64,
    pub timeout_s: u64,
    /// Environment variables removed before the command runs.
    pub env_remove: Vec<String>,
}

pub const DEFAULT_BUDGET_FLAG: &str = "--max-budget-usd";
pub const DEFAULT_MAX_BUDGET_USD: f64 = 0.10;
pub const DEFAULT_LLM_TIMEOUT_S: u64 = 120;

impl LlmCommand {
    /// Split `command` on whitespace (no shell) and attach the cap.
    pub fn new(command: &str, budget_flag: &str, max_budget_usd: f64) -> Result<Self, String> {
        let mut parts = command.split_whitespace().map(String::from);
        let program = parts
            .next()
            .ok_or_else(|| "research.llm.command is empty".to_string())?;
        if budget_flag.trim().is_empty() {
            return Err(
                "research.llm.budget_flag cannot be empty: the spend cap is always passed".into(),
            );
        }
        if !(max_budget_usd.is_finite() && max_budget_usd > 0.0) {
            return Err("research.llm.max_budget_usd must be a positive number".into());
        }
        Ok(LlmCommand {
            program,
            args: parts.collect(),
            budget_flag: budget_flag.trim().to_string(),
            max_budget_usd,
            timeout_s: DEFAULT_LLM_TIMEOUT_S,
            env_remove: Vec::new(),
        })
    }

    /// The full argument list, cap included.
    pub fn argv(&self) -> Vec<String> {
        let mut v = vec![self.program.clone()];
        v.extend(self.args.iter().cloned());
        v.push(self.budget_flag.clone());
        v.push(format!("{}", self.max_budget_usd));
        v
    }
}

impl Llm for LlmCommand {
    fn ask(&self, prompt: &str) -> Result<String, String> {
        let argv = self.argv();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for k in &self.env_remove {
            cmd.env_remove(k);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("FileNotFoundError: cannot run {:?}: {e}", argv[0]))?;
        // Feed stdin and drain stdout on their own threads, so neither pipe can fill up
        // and stall the child while this thread watches the clock.
        let mut stdin = child.stdin.take();
        let prompt = prompt.to_string();
        let writer = std::thread::spawn(move || {
            if let Some(s) = stdin.as_mut() {
                let _ = s.write_all(prompt.as_bytes());
            }
        });
        let mut stdout = child.stdout.take();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_end(&mut buf);
            }
            buf
        });
        let mut stderr = child.stderr.take();
        let err_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_end(&mut buf);
            }
            buf
        });
        let deadline = Instant::now() + Duration::from_secs(self.timeout_s);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "TimeoutExpired: Command '{}' timed out after {} seconds",
                        argv.join(" "),
                        self.timeout_s
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(format!("OSError: {e}")),
            }
        }
        let _ = writer.join();
        let _ = err_reader.join();
        let out = reader.join().unwrap_or_default();
        // A non-zero exit is not an error here: whatever the command printed is the
        // answer, and an empty answer simply yields no verdict line.
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

/// Notes in a TickTick project, through the Open API.
#[derive(Clone)]
pub struct TickTickSink {
    pub project_id: String,
    pub token: String,
    pub api_base: String,
    /// `{project}` and `{id}` are filled in.
    pub link_template: String,
}

pub const TICKTICK_API: &str = "https://api.ticktick.com/open/v1";
pub const TICKTICK_LINK: &str = "https://ticktick.com/webapp/#p/{project}/tasks/{id}";
pub const SINK_TIMEOUT_S: u64 = 45;
/// The note sink's and the report mail's User-Agent.
pub const BROWSER_UA: &str = "Mozilla/5.0";

impl std::fmt::Debug for TickTickSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TickTickSink")
            .field("project_id", &"<set>")
            .field("token", &"***")
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl TickTickSink {
    pub fn create_request(&self, title: &str, content: &str) -> HttpRequest {
        let body = py::dumps(&Json::Obj(vec![
            ("projectId".into(), Json::str(self.project_id.as_str())),
            ("title".into(), Json::str(title)),
            ("content".into(), Json::str(content)),
            ("kind".into(), Json::str("NOTE")),
        ]));
        self.post(&format!("{}/task", self.api_base), body)
    }

    /// The follow-up that makes sure the task is a note.
    pub fn kind_request(&self, id: &str) -> HttpRequest {
        let body = py::dumps(&Json::Obj(vec![
            ("id".into(), Json::str(id)),
            ("projectId".into(), Json::str(self.project_id.as_str())),
            ("kind".into(), Json::str("NOTE")),
        ]));
        self.post(&format!("{}/task/{id}", self.api_base), body)
    }

    fn post(&self, url: &str, body: String) -> HttpRequest {
        HttpRequest::post(url, body, SINK_TIMEOUT_S)
            .header("Content-Type", "application/json")
            .header("User-Agent", BROWSER_UA)
            .header("Authorization", format!("Bearer {}", self.token))
            .secret(&self.token)
    }
}

impl NoteSink for TickTickSink {
    fn create(&self, title: &str, content: &str) -> Result<Option<String>, String> {
        let r = self
            .create_request(title, content)
            .send()
            .map_err(|e| e.to_string())?;
        if !r.is_success() {
            return Err(format!("{} {}", r.status, py::head(&r.body, 200)));
        }
        let created = if py::strip(&r.body).is_empty() {
            Json::obj()
        } else {
            py::parse(&r.body).map_err(|e| format!("bad JSON from the task API: {e}"))?
        };
        let Some(id) = created.get_some("id").map(Json::display) else {
            return Ok(None);
        };
        let r2 = self.kind_request(&id).send().map_err(|e| e.to_string())?;
        if !r2.is_success() {
            return Err(format!("{} {}", r2.status, py::head(&r2.body, 200)));
        }
        Ok(Some(id))
    }

    fn link(&self, id: &str) -> Option<String> {
        Some(
            self.link_template
                .replace("{project}", &self.project_id)
                .replace("{id}", id),
        )
    }
}

/// The report's email through the shared Resend sender.
pub struct EmailMailer(pub EmailConfig);

impl Mailer for EmailMailer {
    fn send(&self, subject: &str, html: &str) -> String {
        self.0.send_report_html(subject, html)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spend_cap_is_always_appended() {
        let c = LlmCommand::new("my-llm -p --model small", "--max-budget-usd", 0.25).unwrap();
        assert_eq!(
            c.argv(),
            vec![
                "my-llm",
                "-p",
                "--model",
                "small",
                "--max-budget-usd",
                "0.25"
            ]
        );
        assert!(LlmCommand::new("x", "", 0.25).is_err());
        assert!(LlmCommand::new("x", "--cap", 0.0).is_err());
        assert!(LlmCommand::new("   ", "--cap", 1.0).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn the_command_gets_the_prompt_and_its_stdout_is_the_answer() {
        // `sh -c 'cat; echo'` echoes stdin; the cap arguments land in $0/$1 and are ignored.
        let c = LlmCommand::new("sh -c cat", "--cap", 1.0).unwrap();
        assert_eq!(
            c.ask("BTC | verdict: WATCH").unwrap(),
            "BTC | verdict: WATCH"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_hung_command_is_killed_at_its_timeout() {
        let mut c = LlmCommand::new("sleep 5", "--cap", 1.0).unwrap();
        // `sleep 5 --cap 1` is invalid on some platforms; use a shell that ignores extras.
        c.program = "sh".into();
        c.args = vec!["-c".into(), "sleep 5".into()];
        c.timeout_s = 1;
        let e = c.ask("").unwrap_err();
        assert!(e.starts_with("TimeoutExpired: "), "{e}");
    }

    #[test]
    #[cfg(unix)]
    fn a_missing_command_is_an_error_naming_it() {
        let c = LlmCommand::new("definitely-not-a-real-binary-xyz", "--cap", 1.0).unwrap();
        assert!(c
            .ask("hi")
            .unwrap_err()
            .contains("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn the_sink_requests_match_the_note_api_and_hide_the_token() {
        let s = TickTickSink {
            project_id: "p1".into(),
            token: "tok123".into(),
            api_base: TICKTICK_API.into(),
            link_template: TICKTICK_LINK.into(),
        };
        let r = s.create_request("T", "caf\u{e9}");
        assert_eq!(r.url, "https://api.ticktick.com/open/v1/task");
        assert_eq!(
            r.body,
            "{\"projectId\": \"p1\", \"title\": \"T\", \"content\": \"caf\\u00e9\", \"kind\": \"NOTE\"}"
        );
        assert_eq!(r.header_value("authorization"), Some("Bearer tok123"));
        assert!(!format!("{r:?}").contains("tok123"));
        assert!(!format!("{s:?}").contains("tok123"));
        assert_eq!(
            s.kind_request("n9").body,
            r#"{"id": "n9", "projectId": "p1", "kind": "NOTE"}"#
        );
        assert_eq!(
            s.link("n9").as_deref(),
            Some("https://ticktick.com/webapp/#p/p1/tasks/n9")
        );
    }

    #[test]
    fn every_client_refuses_offline() {
        let _g = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_OFFLINE", Some("1")),
            ("RUNGBOT_RESEARCH_TEST_KEY", Some("k")),
        ]);
        assert!(HttpFetch
            .get_json("https://example.invalid", "x", 1)
            .is_err());
        let exa = ExaSearch {
            key_env: "RUNGBOT_RESEARCH_TEST_KEY".into(),
            ..ExaSearch::default()
        };
        assert!(matches!(exa.search("{}"), Err(SearchError::Failed(_))));
        let s = TickTickSink {
            project_id: "p".into(),
            token: "t".into(),
            api_base: TICKTICK_API.into(),
            link_template: TICKTICK_LINK.into(),
        };
        assert!(s.create("T", "C").is_err());
    }

    #[test]
    fn no_search_key_is_its_own_error() {
        let _g = crate::testenv::EnvGuard::set(&[("RUNGBOT_RESEARCH_NO_KEY", None)]);
        let exa = ExaSearch {
            key_env: "RUNGBOT_RESEARCH_NO_KEY".into(),
            ..ExaSearch::default()
        };
        assert_eq!(exa.search("{}"), Err(SearchError::NoKey));
    }
}
