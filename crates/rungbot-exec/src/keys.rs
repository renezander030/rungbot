//! Loading venue credentials, and refusing to load them carelessly.
//!
//! Two sources, in order: the environment, then a file you own and nobody else can read.
//! **Never the watchlist config** — that file is meant to be shareable, and a credential
//! in it is a credential in a screenshot.
//!
//! On Unix a key file that is group- or world-readable is refused outright rather than
//! warned about. A warning gets scrolled past.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub key: String,
    pub secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    Missing(String),
    Unreadable(String),
    BadPermissions { path: String, mode: u32 },
    Malformed(String),
}

impl core::fmt::Display for KeyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KeyError::Missing(m) => write!(f, "{m}"),
            KeyError::Unreadable(m) => write!(f, "{m}"),
            KeyError::BadPermissions { path, mode } => write!(
                f,
                "{path} is mode {mode:04o}; others can read your credentials. \
                 Run: chmod 600 {path}"
            ),
            KeyError::Malformed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for KeyError {}

pub fn default_key_path(venue: &str) -> PathBuf {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config"),
    };
    base.join("rungbot").join(format!("{venue}.env"))
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<(), KeyError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .map_err(|e| KeyError::Unreadable(format!("cannot stat {}: {e}", path.display())))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(KeyError::BadPermissions {
            path: path.display().to_string(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<(), KeyError> {
    Ok(())
}

/// Parse `KEY=value` lines, ignoring blanks and `#` comments.
fn parse_env(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            // Tolerate quoting, because a value pasted from a UI often arrives quoted.
            let v = v
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .unwrap_or(v);
            let v = v
                .strip_prefix('\'')
                .and_then(|x| x.strip_suffix('\''))
                .unwrap_or(v);
            out.insert(k.trim().to_uppercase(), v.to_string());
        }
    }
    out
}

/// Environment first, then the key file.
pub fn load(venue: &str, path: Option<&Path>) -> Result<Credentials, KeyError> {
    let v = venue.to_uppercase();
    let (k_env, s_env) = (format!("RUNGBOT_{v}_KEY"), format!("RUNGBOT_{v}_SECRET"));

    if let (Ok(key), Ok(secret)) = (std::env::var(&k_env), std::env::var(&s_env)) {
        if !key.trim().is_empty() && !secret.trim().is_empty() {
            return Ok(Credentials {
                key: key.trim().to_string(),
                secret: secret.trim().to_string(),
            });
        }
    }

    let owned;
    let path = match path {
        Some(p) => p,
        None => {
            owned = default_key_path(venue);
            &owned
        }
    };
    if !path.exists() {
        return Err(KeyError::Missing(format!(
            "no credentials for {venue}: set {k_env} and {s_env}, or create {} (mode 600)",
            path.display()
        )));
    }
    check_permissions(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| KeyError::Unreadable(format!("cannot read {}: {e}", path.display())))?;
    let vars = parse_env(&text);
    let key = vars.get(&k_env).or_else(|| vars.get("KEY"));
    let secret = vars.get(&s_env).or_else(|| vars.get("SECRET"));
    match (key, secret) {
        (Some(k), Some(s)) if !k.is_empty() && !s.is_empty() => Ok(Credentials {
            key: k.clone(),
            secret: s.clone(),
        }),
        _ => Err(KeyError::Malformed(format!(
            "{} has no {k_env}/{s_env} (or KEY/SECRET) pair",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A fixture path unique per test: two tests sharing one path race under the
    /// default parallel runner, and the failure looks like a logic bug.
    fn tmpfile(name: &str, body: &str, mode: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rungbot-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{name}.env"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        p
    }

    #[test]
    fn the_environment_wins_over_a_file() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_TESTV_KEY", Some("envkey")),
            ("RUNGBOT_TESTV_SECRET", Some("envsecret")),
        ]);
        let c = load("testv", None).expect("env is enough");
        assert_eq!(c.key, "envkey");
    }

    #[test]
    fn a_missing_credential_says_exactly_what_to_set() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_NOPEV_KEY", None),
            ("RUNGBOT_NOPEV_SECRET", None),
        ]);
        let e = load("nopev", Some(Path::new("/definitely/not/here.env"))).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("RUNGBOT_NOPEV_KEY"), "{m}");
        assert!(m.contains("mode 600"), "{m}");
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_key_file_is_refused_not_warned_about() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_GATE_KEY", None),
            ("RUNGBOT_GATE_SECRET", None),
        ]);
        let p = tmpfile("world-readable", "KEY=a\nSECRET=b\n", 0o644);
        let e = load("gate", Some(&p)).unwrap_err();
        assert!(matches!(e, KeyError::BadPermissions { .. }), "{e}");
        assert!(e.to_string().contains("chmod 600"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_private_key_file_loads_and_tolerates_quoting() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_GATE_KEY", None),
            ("RUNGBOT_GATE_SECRET", None),
        ]);
        let p = tmpfile("private", "# gate\nKEY=\"abc\"\nSECRET='def'\n", 0o600);
        let c = load("gate", Some(&p)).expect("0600 is fine");
        assert_eq!(c.key, "abc");
        assert_eq!(c.secret, "def", "quotes a UI added are stripped");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_without_a_pair_is_malformed_rather_than_half_loaded() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_GATE_KEY", None),
            ("RUNGBOT_GATE_SECRET", None),
        ]);
        let p = tmpfile("half", "KEY=only\n", 0o600);
        assert!(matches!(
            load("gate", Some(&p)),
            Err(KeyError::Malformed(_))
        ));
    }

    #[test]
    fn the_default_path_is_under_the_config_dir_not_next_to_the_watchlist() {
        let _env = crate::testenv::EnvGuard::set(&[("XDG_CONFIG_HOME", Some("/tmp/cfg"))]);
        let p = default_key_path("gate");
        assert_eq!(p, PathBuf::from("/tmp/cfg/rungbot/gate.env"));
    }
}
