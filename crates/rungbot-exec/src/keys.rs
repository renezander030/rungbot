//! Loading venue credentials, and refusing to load them carelessly.
//!
//! Two sources, in order: the environment, then a file you own and nobody else can read.
//! **Never the watchlist config** — that file is meant to be shareable, and a credential
//! in it is a credential in a screenshot.
//!
//! On Unix a key file that is group- or world-readable is refused outright rather than
//! warned about. A warning gets scrolled past. The same goes for a Revolut X private
//! key file.
//!
//! | venue | environment | file `~/.config/rungbot/<venue>.env` |
//! |---|---|---|
//! | Gate | `RUNGBOT_GATE_KEY`, `RUNGBOT_GATE_SECRET` | `KEY`, `SECRET` |
//! | Binance | `RUNGBOT_BINANCE_KEY`, `RUNGBOT_BINANCE_SECRET` | `KEY`, `SECRET` |
//! | Revolut X | `RUNGBOT_REVX_KEY`, `RUNGBOT_REVX_PRIVATE_KEY_PEM` (a path) | `KEY`, `PRIVATE_KEY_PEM` |
//!
//! Nothing here prints a secret: `Debug` redacts it.

use std::path::{Path, PathBuf};

use crate::revx::RevxCredentials;

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub key: String,
    pub secret: String,
}

impl core::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Credentials")
            .field("key", &self.key)
            .field("secret", &"<redacted>")
            .finish()
    }
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
    crate::run::config::default_config_dir().join(format!("{venue}.env"))
}

/// Days since `venue`'s default key file last changed, when the key comes from that file
/// (no key in the environment). A file is replaced when its key is, so this is the key's
/// age as near as this machine can tell.
pub fn key_file_age_days(venue: &str, now: std::time::SystemTime) -> Option<f64> {
    let (a, b) = if venue == "revx" {
        (REVX_ENV.0.to_string(), REVX_ENV.1.to_string())
    } else {
        venue_env(venue)
    };
    if env_pair(&a, &b).is_some() {
        return None;
    }
    let modified = std::fs::metadata(default_key_path(venue))
        .and_then(|m| m.modified())
        .ok()?;
    Some(now.duration_since(modified).ok()?.as_secs_f64() / 86_400.0)
}

/// Gate and Binance disable a key without an IP allowlist 90 days after it was made.
pub const UNLISTED_KEY_DAYS: f64 = 90.0;

/// What `keys check` says about a key file `age_days` old on `venue`: a warning from
/// day 80 on the venues that expire keys, the plain age elsewhere.
pub fn age_note(venue: &str, age_days: f64) -> String {
    let days = age_days.floor();
    if !matches!(venue, "gate" | "binance") {
        return format!("the key file was last changed {days:.0} days ago");
    }
    if age_days >= UNLISTED_KEY_DAYS - 10.0 {
        format!(
            "! the key file was last changed {days:.0} days ago: without an IP allowlist \
             {venue} disables the key at {UNLISTED_KEY_DAYS:.0} days. Allowlist this \
             machine's IP or make a new key."
        )
    } else {
        format!(
            "the key file was last changed {days:.0} days ago ({:.0} days before a key \
             without an IP allowlist stops working)",
            (UNLISTED_KEY_DAYS - age_days).floor()
        )
    }
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

/// One credential pair: both environment variables when set, else the venue's key file
/// (`path`, or the venue's default), which must be private. In the file each value goes
/// by its variable's name or by its short `alt` name.
/// Revolut X's key and private-key-path variables.
const REVX_ENV: (&str, &str) = ("RUNGBOT_REVX_KEY", "RUNGBOT_REVX_PRIVATE_KEY_PEM");

/// Any other venue's key and secret variables.
fn venue_env(venue: &str) -> (String, String) {
    let v = venue.to_uppercase();
    (format!("RUNGBOT_{v}_KEY"), format!("RUNGBOT_{v}_SECRET"))
}

/// Both variables, trimmed, when the environment sets both to something; the
/// environment wins over a key file only then.
fn env_pair(a_env: &str, b_env: &str) -> Option<(String, String)> {
    let (a, b) = (std::env::var(a_env).ok()?, std::env::var(b_env).ok()?);
    (!a.trim().is_empty() && !b.trim().is_empty())
        .then(|| (a.trim().to_string(), b.trim().to_string()))
}

fn read_pair(
    venue: &str,
    path: Option<&Path>,
    (a_env, a_alt): (&str, &str),
    (b_env, b_alt): (&str, &str),
) -> Result<(String, String), KeyError> {
    if let Some(pair) = env_pair(a_env, b_env) {
        return Ok(pair);
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
            "no credentials for {venue}: set {a_env} and {b_env}, or create {} (mode 600)",
            path.display()
        )));
    }
    check_permissions(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| KeyError::Unreadable(format!("cannot read {}: {e}", path.display())))?;
    let vars = parse_env(&text);
    let a = vars.get(a_env).or_else(|| vars.get(a_alt));
    let b = vars.get(b_env).or_else(|| vars.get(b_alt));
    match (a, b) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => Ok((a.clone(), b.clone())),
        _ => Err(KeyError::Malformed(format!(
            "{} has no {a_env}/{b_env} (or {a_alt}/{b_alt}) pair",
            path.display()
        ))),
    }
}

/// Revolut X: an API key and the path of the Ed25519 private key registered to it.
/// Environment first, then `revx.env`. The key file itself must be private too.
pub fn load_revx(path: Option<&Path>) -> Result<RevxCredentials, KeyError> {
    let (key, pem) = read_pair(
        "revx",
        path,
        (REVX_ENV.0, "KEY"),
        (REVX_ENV.1, "PRIVATE_KEY_PEM"),
    )?;
    let pem_path = PathBuf::from(pem);
    if !pem_path.exists() {
        return Err(KeyError::Missing(format!(
            "the Revolut X private key {} does not exist",
            pem_path.display()
        )));
    }
    check_permissions(&pem_path)?;
    Ok(RevxCredentials { key, pem_path })
}

/// Environment first, then the key file.
pub fn load(venue: &str, path: Option<&Path>) -> Result<Credentials, KeyError> {
    let (k_env, s_env) = venue_env(venue);
    let (key, secret) = read_pair(venue, path, (&k_env, "KEY"), (&s_env, "SECRET"))?;
    Ok(Credentials { key, secret })
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

    #[cfg(unix)]
    #[test]
    fn each_venue_names_its_own_variables_when_a_pair_is_missing() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_REVX_KEY", None),
            ("RUNGBOT_REVX_PRIVATE_KEY_PEM", None),
            ("RUNGBOT_GATE_KEY", None),
            ("RUNGBOT_GATE_SECRET", None),
        ]);
        let none = Path::new("/definitely/not/here.env");
        assert_eq!(
            load_revx(Some(none)).unwrap_err().to_string(),
            "no credentials for revx: set RUNGBOT_REVX_KEY and RUNGBOT_REVX_PRIVATE_KEY_PEM, \
             or create /definitely/not/here.env (mode 600)"
        );
        assert_eq!(
            load("gate", Some(none)).unwrap_err().to_string(),
            "no credentials for gate: set RUNGBOT_GATE_KEY and RUNGBOT_GATE_SECRET, \
             or create /definitely/not/here.env (mode 600)"
        );
        let p = tmpfile("halfpair", "KEY=only\n", 0o600);
        assert_eq!(
            load_revx(Some(&p)).unwrap_err().to_string(),
            format!(
                "{} has no RUNGBOT_REVX_KEY/RUNGBOT_REVX_PRIVATE_KEY_PEM (or KEY/PRIVATE_KEY_PEM) pair",
                p.display()
            )
        );
        assert_eq!(
            load("gate", Some(&p)).unwrap_err().to_string(),
            format!(
                "{} has no RUNGBOT_GATE_KEY/RUNGBOT_GATE_SECRET (or KEY/SECRET) pair",
                p.display()
            )
        );
    }

    #[test]
    fn a_key_file_near_ninety_days_warns_on_the_venues_that_expire_keys() {
        assert!(age_note("gate", 79.9).starts_with("the key file was last changed 79 days"));
        assert!(age_note("gate", 79.9)
            .ends_with("(10 days before a key without an IP allowlist stops working)"));
        assert!(age_note("binance", 80.0).starts_with("! the key file was last changed 80 days"));
        assert_eq!(
            age_note("revx", 200.0),
            "the key file was last changed 200 days ago"
        );
    }

    #[test]
    fn debug_never_prints_a_secret() {
        let c = Credentials {
            key: "k".into(),
            secret: "very-secret".into(),
        };
        assert!(!format!("{c:?}").contains("very-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn a_revx_key_loads_from_its_file_and_its_pem_must_be_private() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("RUNGBOT_REVX_KEY", None),
            ("RUNGBOT_REVX_PRIVATE_KEY_PEM", None),
        ]);
        let pem = tmpfile("revx-pem", "-----BEGIN PRIVATE KEY-----\n", 0o600);
        let env = tmpfile(
            "revx",
            &format!("KEY=rk\nPRIVATE_KEY_PEM={}\n", pem.display()),
            0o600,
        );
        let c = load_revx(Some(&env)).expect("private files load");
        assert_eq!(c.key, "rk");
        assert_eq!(c.pem_path, pem);

        let open = tmpfile("revx-pem-open", "x", 0o644);
        let env2 = tmpfile(
            "revx2",
            &format!("KEY=rk\nPRIVATE_KEY_PEM={}\n", open.display()),
            0o600,
        );
        assert!(matches!(
            load_revx(Some(&env2)),
            Err(KeyError::BadPermissions { .. })
        ));
    }

    #[test]
    fn the_default_path_is_under_the_config_dir_not_next_to_the_watchlist() {
        let _env = crate::testenv::EnvGuard::set(&[("XDG_CONFIG_HOME", Some("/tmp/cfg"))]);
        let p = default_key_path("gate");
        assert_eq!(p, PathBuf::from("/tmp/cfg/rungbot/gate.env"));
    }
}
