//! What each venue accepts as a client order id, and how a journal id becomes one.
//!
//! A venue's id constraint is a contract, and breaking it fails in a misleading way:
//! Binance reports an over-long id with the same "illegal characters" error as a bad
//! character, so an id that quietly grew reads like a charset bug. Coercion happens
//! before the signed request, so a bad id fails here, loudly, instead of as an opaque
//! rejection several retries later.
//!
//! * Binance (and the journal itself): `[a-zA-Z0-9_-]`, at most 36 characters.
//! * Gate: a `t-` prefix on the wire, then `[a-zA-Z0-9_.-]`, at most 28 characters.
//! * Revolut X: a UUID. A journal id is turned into a **uuid5**, so the same intent
//!   always derives the same UUID and the venue's duplicate check still applies.
//!
//! Over-long ids keep a readable prefix and end in a 6-character SHA-1 tail, so they stay
//! unique and still grep back to the order they belong to.

use sha1::{Digest, Sha1};

pub const CID_MAX: usize = 36;
pub const GATE_CID_MAX: usize = 28;

/// The fixed uuid5 namespace for Revolut X client ids. Changing it would change every
/// derived id, and with it the venue's duplicate protection across a restart.
pub const REVX_NAMESPACE: [u8; 16] = [
    0x6b, 0x84, 0x65, 0xf2, 0x75, 0x2a, 0x56, 0x8d, 0xa9, 0x81, 0x77, 0x42, 0x91, 0x77, 0x75, 0xd0,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdError(pub String);

impl core::fmt::Display for IdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IdError {}

fn sha1_hex(s: &str) -> String {
    let d = Sha1::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn coerce(raw: &str, max: usize, legal: impl Fn(char) -> bool) -> Result<String, IdError> {
    let mut candidate: String = raw
        .chars()
        .map(|c| if legal(c) { c } else { '-' })
        .collect();
    if candidate.chars().count() > max {
        let digest = sha1_hex(&candidate);
        let head: String = candidate.chars().take(max - 7).collect();
        candidate = format!("{head}-{}", &digest[..6]);
    }
    if candidate.is_empty() {
        return Err(IdError("a client order id cannot be empty".into()));
    }
    Ok(candidate)
}

/// A journal id the strictest venue accepts: illegal characters become `-`, an id over
/// 36 characters keeps 29 of them plus `-` and a hash. Idempotent.
pub fn safe_cid(raw: &str) -> Result<String, IdError> {
    coerce(raw, CID_MAX, |c| {
        c.is_ascii_alphanumeric() || c == '_' || c == '-'
    })
}

/// The part of Gate's `text` field after its mandatory `t-`. A leading `t-` is
/// tolerated, so coercing twice changes nothing.
pub fn gate_cid(raw: &str) -> Result<String, IdError> {
    let raw = raw.strip_prefix("t-").unwrap_or(raw);
    coerce(raw, GATE_CID_MAX, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
    })
}

/// Gate's `text` field as sent: `t-` plus [`gate_cid`].
pub fn safe_gate_text(raw: &str) -> Result<String, IdError> {
    Ok(format!("t-{}", gate_cid(raw)?))
}

/// A Revolut X client id: a UUID passes through in canonical lowercase form, anything
/// else becomes the uuid5 of it, so the same journal id always maps to the same UUID.
pub fn safe_revx_cid(raw: &str) -> Result<String, IdError> {
    if let Some(u) = parse_uuid(raw) {
        return Ok(fmt_uuid(u));
    }
    if raw.is_empty() {
        return Err(IdError("a client order id cannot be empty".into()));
    }
    Ok(fmt_uuid(uuid5(&REVX_NAMESPACE, raw)))
}

/// Version 5 UUID: SHA-1 of namespace and name, with the version and variant bits set.
pub fn uuid5(namespace: &[u8; 16], name: &str) -> [u8; 16] {
    let mut h = Sha1::new();
    h.update(namespace);
    h.update(name.as_bytes());
    let d = h.finalize();
    let mut u = [0u8; 16];
    u.copy_from_slice(&d[..16]);
    u[6] = (u[6] & 0x0f) | 0x50;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}

pub fn fmt_uuid(u: [u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Parse a UUID the permissive way the reference does: `urn:` and `uuid:` are dropped,
/// braces stripped from the ends, every hyphen removed, and the 32 characters left are
/// read as a base-16 integer (which also tolerates surrounding whitespace, one sign and
/// single underscores between digits). Anything else is not a UUID.
pub fn parse_uuid(raw: &str) -> Option<[u8; 16]> {
    let s = raw.replace("urn:", "").replace("uuid:", "");
    let s = s.trim_matches(|c| c == '{' || c == '}').replace('-', "");
    if s.chars().count() != 32 {
        return None;
    }
    let t = s.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'+') => (false, &t[1..]),
        Some(b'-') => (true, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let clean = digits.replace('_', "");
    if clean.is_empty() || !clean.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u128::from_str_radix(&clean, 16).ok()?;
    if neg && v != 0 {
        return None;
    }
    Some(v.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_namespace_is_a_valid_uuid5() {
        assert_eq!(REVX_NAMESPACE[6] >> 4, 5);
        assert_eq!(REVX_NAMESPACE[8] >> 6, 0b10);
    }

    #[test]
    fn coercion_is_idempotent() {
        let long = "csVERYLONGSYMBOLNAMEb944444r1u93041u93042";
        let once = safe_cid(long).unwrap();
        assert_eq!(once.len(), 36);
        assert_eq!(safe_cid(&once).unwrap(), once);
        let g = safe_gate_text(long).unwrap();
        assert_eq!(safe_gate_text(&g).unwrap(), g);
        let r = safe_revx_cid("csAAAb1r1").unwrap();
        assert_eq!(safe_revx_cid(&r).unwrap(), r);
    }

    #[test]
    fn an_empty_id_is_refused_everywhere() {
        assert!(safe_cid("").is_err());
        assert!(gate_cid("t-").is_err());
        assert!(safe_revx_cid("").is_err());
    }
}
