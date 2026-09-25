//! Translate a Python bot directory's knobs into a `rungbot-exec run` config.
//!
//! The source keeps its knobs in two places: literal defaults in the modules
//! (`os.environ.get("FIRST_PCT", "10")`) and a cron wrapper that exports the values the
//! bot actually runs with (`export FIRST_PCT="${FIRST_PCT:-15}"`). The effective value is
//! the wrapper's, else the module default. The coin tables are module dicts
//! (`WATCHLIST`, `NAMES`, `ENTRIES`, `ROUTING`, `REVX_KLINE_SRC`, `REVX_PAIRS`).
//!
//! The two rail files are carried over as paths, never left to this runtime's defaults:
//! the halt file (`HALT_FILE`) and the manual sell-arm file (`SELL_ARM_FILE`). A halt
//! file the operator touches must stop the new runtime as it stopped the old one. The
//! source spells their defaults `str(Path.home() / ".config" / "name")`; they print as
//! `~/.config/name`. A source whose halt file cannot be told is an error.
//!
//! [`generate`] reads all of that and prints the YAML. It writes nothing: the output
//! carries personal values (addresses, cost basis, allocations) and belongs in the
//! operator's own config file, never in a repository.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use super::config::{env_name, ENV_KNOBS};

/// Every `os.environ.get("NAME", "literal")` default in `src`.
pub fn module_defaults(src: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let pat = "os.environ.get(";
    let mut rest = src;
    while let Some(i) = rest.find(pat) {
        rest = &rest[i + pat.len()..];
        let Some((name, after)) = quoted(rest) else {
            continue;
        };
        let after = after.trim_start();
        let Some(after) = after.strip_prefix(',') else {
            continue;
        };
        if let Some((val, _)) = quoted(after.trim_start()) {
            out.entry(name).or_insert(val);
        }
    }
    out
}

/// A leading `"..."` or `'...'`, and what follows it.
fn quoted(s: &str) -> Option<(String, &str)> {
    let q = s.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let end = s[1..].find(q)? + 1;
    Some((s[1..end].to_string(), &s[end + 1..]))
}

/// The wrapper's `export NAME="${NAME:-value}"` lines, with `$VAR` values resolved from
/// its own `VAR='...'` assignments.
pub fn wrapper_values(sh: &str) -> BTreeMap<String, String> {
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for line in sh.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            let Some((name, val)) = rest.split_once('=') else {
                continue;
            };
            let val = val.trim().trim_matches('"');
            let prefix = format!("${{{name}:-");
            let Some(def) = val.strip_prefix(&prefix) else {
                continue;
            };
            // `${X:-{}}` leaves the closing brace of the default in place.
            let def = def.strip_suffix('}').unwrap_or(def);
            let def = match def.strip_prefix('$') {
                Some(var) if var.chars().all(|c| c.is_ascii_uppercase() || c == '_') => {
                    vars.get(var).cloned().unwrap_or_default()
                }
                _ => home_to_tilde(def),
            };
            out.insert(name.to_string(), def);
        } else if let Some((name, val)) = line.split_once('=') {
            if name.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !name.is_empty() {
                vars.insert(
                    name.to_string(),
                    val.trim().trim_matches('\'').trim_matches('"').to_string(),
                );
            }
        }
    }
    out
}

/// `$HOME/x` or `${HOME}/x` as `~/x`; anything else as it is.
fn home_to_tilde(s: &str) -> String {
    for pre in ["$HOME/", "${HOME}/"] {
        if let Some(rest) = s.strip_prefix(pre) {
            return format!("~/{rest}");
        }
    }
    s.to_string()
}

/// A path default the source builds from the home directory:
/// `os.environ.get("NAME", str(Path.home() / ".config" / "x"))` gives `~/.config/x`.
pub fn home_path_default(src: &str, name: &str) -> Option<String> {
    for q in ["\"", "'"] {
        let pat = format!("os.environ.get({q}{name}{q}");
        let Some(i) = src.find(&pat) else {
            continue;
        };
        let rest = src[i + pat.len()..].trim_start().strip_prefix(',')?;
        let mut rest = rest
            .trim_start()
            .strip_prefix("str(")?
            .trim_start()
            .strip_prefix("Path.home()")?;
        let mut parts = Vec::new();
        while let Some(r) = rest.trim_start().strip_prefix('/') {
            let (part, after) = quoted(r.trim_start())?;
            parts.push(part);
            rest = after;
        }
        if parts.is_empty() || !rest.trim_start().starts_with(')') {
            return None;
        }
        return Some(format!("~/{}", parts.join("/")));
    }
    None
}

/// The `KEY: value` pairs of a module-level dict literal `NAME = { ... }`, in order.
/// Values are the literal text: a quoted string unquoted, a tuple as its items joined
/// by spaces, a number as written.
pub fn module_dict(src: &str, name: &str) -> Option<Vec<(String, String)>> {
    let start = src.find(&format!("\n{name} = {{"))?;
    let body_start = start + src[start..].find('{')? + 1;
    let mut depth = 1usize;
    let mut end = body_start;
    for (i, c) in src[body_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = body_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &src[body_start..end];
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        // Skip comments and whitespace up to the next quoted key.
        let mut trimmed = rest.trim_start_matches([' ', '\n', '\t', ',', '\r']);
        while trimmed.starts_with('#') {
            trimmed = trimmed
                .split_once('\n')
                .map_or("", |(_, r)| r)
                .trim_start_matches([' ', '\n', '\t', ',', '\r']);
        }
        let Some((key, after)) = quoted(trimmed) else {
            break;
        };
        let after = after.trim_start().strip_prefix(':')?.trim_start();
        let (val, next) = if let Some(t) = after.strip_prefix('(') {
            let close = t.find(')')?;
            let items: Vec<String> = t[..close]
                .split(',')
                .map(|x| x.trim().trim_matches('"').trim_matches('\'').to_string())
                .filter(|x| !x.is_empty())
                .collect();
            (items.join(" "), &t[close + 1..])
        } else if let Some((v, n)) = quoted(after) {
            (v, n)
        } else {
            let cut = after.find([',', '\n', '}']).unwrap_or(after.len());
            (after[..cut].trim().to_string(), &after[cut..])
        };
        out.push((key, val));
        rest = next;
    }
    Some(out)
}

/// A YAML scalar that reads back as the same string.
fn yq(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-~".contains(c))
        && !matches!(
            s.to_ascii_lowercase().as_str(),
            "yes" | "no" | "on" | "off" | "true" | "false" | "y" | "n" | "null" | "~"
        );
    if plain {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn json_block(key: &str, raw: &str, out: &mut String) {
    // Keep the source's key order.
    let ordered: indexmap::IndexMap<String, Value> =
        serde_json::from_str(if raw.trim().is_empty() { "{}" } else { raw }).unwrap_or_default();
    if ordered.is_empty() {
        return;
    }
    out.push_str(&format!("{key}:\n"));
    for (k, v) in ordered {
        match v {
            Value::Object(o) => {
                out.push_str(&format!("  {k}:\n"));
                for (ik, iv) in o {
                    let s = match iv {
                        Value::Array(a) => a
                            .iter()
                            .map(|x| x.to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                        other => other.to_string(),
                    };
                    out.push_str(&format!("    {ik}: \"{s}\"\n"));
                }
            }
            Value::Array(a) => {
                let s: Vec<String> = a.iter().map(|x| x.to_string()).collect();
                out.push_str(&format!("  {k}: \"{}\"\n", s.join(",")));
            }
            other => out.push_str(&format!("  {k}: {other}\n")),
        }
    }
}

const MAP_KNOBS: [&str; 5] = [
    "bands",
    "sell_giveback",
    "sell_trail_arm",
    "deploy_alloc",
    "deploy_zones",
];

/// How many of the run's knobs a wrapper exports.
fn knob_count(sh: &str) -> usize {
    let exported = wrapper_values(sh);
    ENV_KNOBS
        .iter()
        .filter(|k| exported.contains_key(&env_name(k)))
        .count()
}

/// Print the config for the bot in `dir`. Every `*.py` there is read for module
/// defaults and coin tables; of the `*-cron.sh` files that export `TRADE_MODE`, the one
/// exporting the most knobs is the wrapper.
pub fn generate(dir: &Path) -> Result<String, String> {
    let mut py = String::new();
    let mut wrapper: Option<(String, String, usize)> = None;
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.ends_with(".py") && !name.starts_with("test_") {
            py.push('\n');
            py.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
        } else if name.ends_with("-cron.sh") {
            let t = std::fs::read_to_string(&p).unwrap_or_default();
            if t.contains("export TRADE_MODE=") {
                // Side jobs (a backtest, a watcher) export TRADE_MODE too, pinned off. The
                // bot's own wrapper is the one that exports the most knobs; on a tie the
                // first by name.
                let n = knob_count(&t);
                if wrapper.as_ref().is_none_or(|(_, _, best)| n > *best) {
                    wrapper = Some((name.trim_end_matches("-cron.sh").to_string(), t, n));
                }
            }
        }
    }
    let wrapper = wrapper.map(|(bot, sh, _)| (bot, sh));
    let (bot, sh) = wrapper.ok_or_else(|| {
        format!(
            "no *-cron.sh exporting TRADE_MODE in {}; nothing to translate",
            dir.display()
        )
    })?;
    let mut values = module_defaults(&py);
    values.extend(wrapper_values(&sh));

    let mut y = String::new();
    y.push_str(&format!(
        "# rungbot-exec run config, translated from {}.\n# It holds personal values: keep it \
         out of any repository. Review every line before a live run.\n\n",
        dir.display()
    ));
    for key in ENV_KNOBS {
        let var = env_name(key);
        let Some(v) = values.get(&var) else {
            continue;
        };
        if MAP_KNOBS.contains(key) {
            json_block(key, v, &mut y);
        } else if !key.ends_with("_path")
            && !matches!(
                *key,
                "halt_file"
                    | "sell_arm_file"
                    | "order_journal"
                    | "pnl_ledger"
                    | "ttl_warn_state"
                    | "decisions_log"
                    | "signal_notices"
                    | "btc_alert_state"
                    | "regime_state"
                    | "froth_state"
                    | "run_lock"
                    | "deploy_state"
                    | "audit_state"
                    | "fillodds_cache"
            )
        {
            y.push_str(&format!("{key}: {}\n", yq(v)));
        }
    }
    // The rails: the halt file must be the one the operator already knows to touch.
    for (key, var, required) in [
        ("halt_file", "HALT_FILE", true),
        ("sell_arm_file", "SELL_ARM_FILE", false),
    ] {
        let v = values
            .get(var)
            .map(|v| home_to_tilde(v))
            .or_else(|| home_path_default(&py, var))
            .filter(|v| !v.trim().is_empty());
        match v {
            Some(v) => y.push_str(&format!("{key}: {}\n", yq(&v))),
            None if required => {
                return Err(format!(
                    "cannot tell which {var} the bot in {} honours; export {var} in its \
                     cron wrapper and run this again",
                    dir.display()
                ))
            }
            None => {}
        }
    }
    y.push_str(&format!("mail_name: {}\n", yq(&bot)));
    if let Some(log) = sh
        .lines()
        .find_map(|l| l.trim().strip_prefix("LOG="))
        .map(|l| l.trim_matches('"').to_string())
    {
        y.push_str(&format!("log_hint: {}\n", yq(&log)));
    }
    for (table, key) in [
        ("WATCHLIST", "watchlist"),
        ("NAMES", "names"),
        ("ENTRIES", "entries"),
        ("ROUTING", "routing"),
        ("REVX_KLINE_SRC", "regime_kline_source"),
        ("REVX_PAIRS", "revx_pairs"),
    ] {
        if let Some(rows) = module_dict(&py, table) {
            y.push_str(&format!("{key}:\n"));
            for (k, v) in rows {
                let v = if key == "entries" { v } else { yq(&v) };
                y.push_str(&format!("  {}: {v}\n", yq(&k)));
            }
        }
    }
    if let (Some(to), Some(from)) = (values.get("EMAIL_TO"), values.get("EMAIL_FROM")) {
        y.push_str(&format!(
            "notify:\n  email:\n    from: {}\n    to: {}\n    api_key_file: ~/.config/resend.env\n",
            yq(from),
            yq(to)
        ));
    }
    Ok(y)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PY: &str = r#"
HALT_FILE = Path(os.environ.get("HALT_FILE", str(Path.home() / ".config" / "bot-halt")))
MANUAL_ARM_FILE = Path(os.environ.get('SELL_ARM_FILE', str(Path.home() / '.config' / 'bot-armed')))
FIRST_PCT = float(os.environ.get("FIRST_PCT", "10"))
TRADE_MODE = os.environ.get("TRADE_MODE", "off").lower()
X = os.environ.get('SELL_POLICY', 'auto')

WATCHLIST = {
    "AAA":  "aaa-coin",   # a comment
    "BBB":  "bbb-coin",
}
NAMES = {"AAA": "Alpha Coin", "BBB": "Beta Coin"}
ENTRIES = {
    "AAA": 1.25,
    "BBB": 0.0042,
}
ROUTING = {
    "AAA": ("revx", "AAA/USD", "USD"),
    "BBB": ("gate", "BBB_USDT", "USDT"),
}
"#;

    const SH: &str = r#"
LOG=/var/log/example.log
export TRADE_MODE="${TRADE_MODE:-live}"
export FIRST_PCT="${FIRST_PCT:-15}"
DEFAULT_ALLOC='{"AAA":3,"BBB":1}'
export DEPLOY_ALLOC_JSON="${DEPLOY_ALLOC_JSON:-$DEFAULT_ALLOC}"
export SELL_GIVEBACK_JSON="${SELL_GIVEBACK_JSON:-{}}"
export EMAIL_TO="${EMAIL_TO:-me@example.com}"
export EMAIL_FROM="${EMAIL_FROM:-bot@example.com}"
"#;

    #[test]
    fn the_wrapper_wins_over_the_module_default() {
        let d = std::env::temp_dir().join(format!("rungbot-cexcfg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("bot.py"), PY).unwrap();
        std::fs::write(d.join("examplebot-cron.sh"), SH).unwrap();
        let y = generate(&d).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert!(y.contains("trade_mode: live\n"), "{y}");
        assert!(y.contains("first_pct: 15\n"), "{y}");
        assert!(y.contains("sell_policy: auto\n"), "{y}");
        assert!(y.contains("deploy_alloc:\n  AAA: 3\n  BBB: 1\n"), "{y}");
        assert!(
            !y.contains("sell_giveback:"),
            "an empty map prints nothing: {y}"
        );
        assert!(y.contains("routing:\n  AAA: \"revx AAA/USD USD\"\n"), "{y}");
        assert!(y.contains("entries:\n  AAA: 1.25\n  BBB: 0.0042\n"), "{y}");
        assert!(y.contains("names:\n  AAA: \"Alpha Coin\"\n"), "{y}");
        assert!(
            y.contains("mail_name: examplebot\nlog_hint: /var/log/example.log\n"),
            "{y}"
        );
        assert!(y.contains("    to: \"me@example.com\"\n"), "{y}");

        // And it loads.
        let c = super::super::config::RunConfig::from_yaml(&y, &|_| None).unwrap();
        assert_eq!(c.trade_mode, "live");
        assert_eq!(c.first_pct, 15.0);
        assert_eq!(c.route("BBB").unwrap().pair, "BBB_USDT");
        assert_eq!(
            c.deploy_alloc,
            vec![("AAA".into(), 3.0), ("BBB".into(), 1.0)]
        );
    }

    fn generated(py: &str, sh: &str, tag: &str) -> Result<String, String> {
        let d = std::env::temp_dir().join(format!("rungbot-cexcfg-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("bot.py"), py).unwrap();
        std::fs::write(d.join("examplebot-cron.sh"), sh).unwrap();
        let y = generate(&d);
        let _ = std::fs::remove_dir_all(&d);
        y
    }

    /// Side jobs export `TRADE_MODE="off"` too; the bot's own wrapper wins whatever the
    /// file names sort as.
    #[test]
    fn the_wrapper_is_the_one_exporting_the_most_knobs() {
        let d = std::env::temp_dir().join(format!("rungbot-cexcfg-side-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("bot.py"), PY).unwrap();
        std::fs::write(d.join("examplebot-cron.sh"), SH).unwrap();
        let side = "export TRADE_MODE=\"off\"\nexport LIVE_TRADING_ENABLED=\"no\"\n";
        std::fs::write(d.join("aaa-backtest-cron.sh"), side).unwrap();
        std::fs::write(d.join("zzz-watch-cron.sh"), side).unwrap();
        let y = generate(&d);
        let _ = std::fs::remove_dir_all(&d);
        let y = y.unwrap();
        let c = super::super::config::RunConfig::from_yaml(&y, &|_| None).unwrap();
        assert_eq!(c.trade_mode, "live", "{y}");
        assert!(y.contains("mail_name: examplebot\n"), "{y}");
    }

    #[test]
    fn the_rail_files_carry_over_as_the_source_honours_them() {
        let y = generated(PY, SH, "rails").unwrap();
        assert!(y.contains("halt_file: ~/.config/bot-halt\n"), "{y}");
        assert!(y.contains("sell_arm_file: ~/.config/bot-armed\n"), "{y}");
        let c = super::super::config::RunConfig::from_yaml(&y, &|_| None).unwrap();
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
        assert_eq!(c.halt_file, home.join(".config/bot-halt"));
        assert_eq!(c.sell_arm_file, home.join(".config/bot-armed"));

        // The wrapper's export wins, `$HOME` included.
        let sh = format!("{SH}export HALT_FILE=\"${{HALT_FILE:-$HOME/run/halt}}\"\n");
        let y = generated(PY, &sh, "wrapper").unwrap();
        assert!(y.contains("halt_file: ~/run/halt\n"), "{y}");
    }

    #[test]
    fn a_source_whose_halt_file_cannot_be_told_is_an_error() {
        let py = PY.replace("HALT_FILE", "OTHER_FILE");
        let e = generated(&py, SH, "nohalt").unwrap_err();
        assert!(e.contains("HALT_FILE"), "{e}");
    }
}
