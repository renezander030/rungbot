//! The decision log: one JSON line per outcome a run decided, so "why did nothing
//! happen?" is answerable without the mail.
//!
//! ```json
//! {"ts": 1758100000.0, "run": "2025-09-17T09:06Z", "sym": "AAA", "side": "buy",
//!  "src": "ladder", "kind": "skip", "text": "$0.00 USDT < min $3.00", "committed": false}
//! ```
//!
//! `src` is `deploy` for the deploy layer's results, `hk` for other housekeeping and
//! `ladder` for this run's signals. Each run ends with one `run` line counting the
//! results by kind; it also carries the trade mode. The file is trimmed to its newer
//! half once it passes `decisions_max_mb`.

use std::io::Write;
use std::path::Path;

use super::RunResult;
use crate::pyfmt;

pub const KINDS: [&str; 5] = ["done", "err", "skip", "warn", "plan"];

/// One line of the log.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub ts: f64,
    pub run: String,
    pub sym: Option<String>,
    pub side: Option<String>,
    pub src: String,
    pub kind: String,
    pub mode: Option<String>,
    pub text: String,
    pub committed: bool,
}

fn opt(s: &Option<String>) -> String {
    match s {
        Some(v) => json_str(v),
        None => "null".into(),
    }
}

/// A JSON string as Python's `json.dumps(..., ensure_ascii=False)` writes it.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Record {
    /// The line as written, key order and separators as the reference wrote them.
    pub fn to_line(&self) -> String {
        let mode = match &self.mode {
            Some(m) => format!("\"mode\": {}, ", json_str(m)),
            None => String::new(),
        };
        format!(
            "{{\"ts\": {}, \"run\": {}, \"sym\": {}, \"side\": {}, \"src\": {}, \"kind\": {}, \
             {mode}\"text\": {}, \"committed\": {}}}",
            pyfmt::float_repr(self.ts),
            json_str(&self.run),
            opt(&self.sym),
            opt(&self.side),
            json_str(&self.src),
            json_str(&self.kind),
            json_str(&self.text),
            if self.committed { "true" } else { "false" }
        )
    }

    /// The run log's `DECISION ...` line.
    pub fn log_line(&self) -> String {
        if self.kind == "run" {
            return format!("DECISION run: {}", self.text);
        }
        let dash = |v: &Option<String>| {
            v.as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("-")
                .to_string()
        };
        format!(
            "DECISION {:5} {:4} {:4} [{}] {}",
            dash(&self.sym),
            dash(&self.side),
            self.kind,
            self.src,
            self.text
        )
    }
}

fn iso_minute(ts: f64) -> String {
    let (y, m, d, h, mi, _) = rungbot_core::time::civil(ts);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}Z")
}

/// `round(x, 1)`.
fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

/// `(kind, text, src)` for one result: the first populated outcome, trimmed to one line
/// of at most 240 characters.
pub fn classify(res: &RunResult) -> Option<(String, String, String)> {
    for k in KINDS {
        if let Some(v) = res.get(k) {
            let text: String = v.trim().replace('\n', " ").chars().take(240).collect();
            let src = if res.deploy {
                "deploy"
            } else if res.hk {
                "hk"
            } else {
                "ladder"
            };
            return Some((k.to_string(), text, src.to_string()));
        }
    }
    None
}

/// This run's records, not yet written.
pub fn lines_for(
    run_ts: f64,
    execution: &[RunResult],
    n_buys: usize,
    n_sells: usize,
    mode: &str,
) -> Vec<Record> {
    let ts = round1(run_ts);
    let run = iso_minute(run_ts);
    let mut counts = [0usize; 5];
    let mut out = Vec::new();
    for res in execution {
        let Some((kind, text, src)) = classify(res) else {
            continue;
        };
        if let Some(i) = KINDS.iter().position(|k| *k == kind) {
            counts[i] += 1;
        }
        out.push(Record {
            ts,
            run: run.clone(),
            sym: res.sym.clone(),
            side: res.side.clone(),
            src,
            kind,
            mode: None,
            text,
            committed: res.committed,
        });
    }
    let summary: Vec<String> = KINDS
        .iter()
        .zip(counts)
        .map(|(k, n)| format!("{k}={n}"))
        .collect();
    out.push(Record {
        ts,
        run,
        sym: None,
        side: None,
        src: "run".into(),
        kind: "run".into(),
        mode: Some(mode.to_string()),
        text: format!(
            "signals buy={n_buys} sell={n_sells}; results {}",
            summary.join(" ")
        ),
        committed: false,
    });
    out
}

/// Append the records (after trimming a file past `max_mb` to its newer half).
pub fn append(path: &Path, recs: &[Record], max_mb: f64) -> Result<(), String> {
    trim_if_large(path, max_mb);
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut text = String::new();
    for r in recs {
        text.push_str(&r.to_line());
        text.push('\n');
    }
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

fn trim_if_large(path: &Path, max_mb: f64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if (meta.len() as f64) <= max_mb * 1024.0 * 1024.0 {
        return;
    }
    if let Ok(text) = std::fs::read_to_string(path) {
        let lines: Vec<&str> = text.lines().collect();
        let keep = lines[lines.len() / 2..].join("\n") + "\n";
        let _ = std::fs::write(path, keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_written_as_the_reference_wrote_it() {
        let res = RunResult {
            sym: Some("AAA".into()),
            side: Some("buy".into()),
            skip: Some("$0.00 USDT < min $3.00".into()),
            ..Default::default()
        };
        let recs = lines_for(1_758_000_000.04, &[res], 1, 0, "live");
        assert_eq!(
            recs[0].to_line(),
            "{\"ts\": 1758000000.0, \"run\": \"2025-09-16T05:20Z\", \"sym\": \"AAA\", \
             \"side\": \"buy\", \"src\": \"ladder\", \"kind\": \"skip\", \"text\": \
             \"$0.00 USDT < min $3.00\", \"committed\": false}"
        );
        assert_eq!(
            recs[0].log_line(),
            "DECISION AAA   buy  skip [ladder] $0.00 USDT < min $3.00"
        );
        assert_eq!(
            recs[1].log_line(),
            "DECISION run: signals buy=1 sell=0; results done=0 err=0 skip=1 warn=0 plan=0"
        );
        assert!(recs[1].to_line().contains("\"mode\": \"live\", \"text\""));
    }
}
