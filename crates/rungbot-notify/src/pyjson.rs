//! JSON text byte-identical to Python's `json.dumps` defaults.
//!
//! Two files and one request body have to match what the Python bot wrote, byte for
//! byte, so a state file can be carried across and a request diffed. Python's defaults
//! differ from `serde_json` in two ways that matter here:
//!
//! * `ensure_ascii=True`: every character outside printable ASCII is written as a
//!   `\uXXXX` escape (a surrogate pair above U+FFFF), and so is DEL (0x7f).
//! * The compact separators are `", "` and `": "`, not `","` and `":"`.
//!
//! With `indent`, Python's separators become `","` + newline and `": "`, which is what
//! `serde_json`'s pretty printer writes too, so only the escaping needs fixing there.

use serde::Serialize;

/// Rewrite every non-ASCII character and DEL in JSON text as Python's `\uXXXX` escape.
///
/// Safe on whole documents: outside string literals JSON is pure ASCII, so only string
/// contents are touched.
pub fn ensure_ascii(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() && c != '\u{7f}' {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// A string literal as Python's `json.dumps(s)` writes it.
pub fn string(s: &str) -> String {
    ensure_ascii(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()))
}

/// `json.dumps(value, indent=1, sort_keys=True)`. Keys come out sorted when `value`
/// serializes as maps with sorted keys (`BTreeMap`, or struct fields declared in order).
pub fn dumps_indent1<T: Serialize>(value: &T) -> String {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    if value.serialize(&mut ser).is_err() {
        return "{}".into();
    }
    ensure_ascii(&String::from_utf8(buf).unwrap_or_default())
}

/// A compact object with Python's `", "` / `": "` separators, keys in the given order.
/// Each value must already be JSON text.
pub fn object(pairs: &[(&str, String)]) -> String {
    let fields: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}: {v}", string(k)))
        .collect();
    format!("{{{}}}", fields.join(", "))
}

/// A compact array with Python's `", "` separator. Each item must already be JSON text.
pub fn array(items: &[String]) -> String {
    format!("[{}]", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_match_python() {
        // json.dumps("caf\u00e9 \U0001F9ED \x7f\x01\n\"") in CPython
        assert_eq!(
            string("caf\u{e9} \u{1F9ED} \u{7f}\u{1}\n\""),
            "\"caf\\u00e9 \\ud83e\\udded \\u007f\\u0001\\n\\\"\""
        );
    }

    #[test]
    fn compact_separators_match_python() {
        let o = object(&[
            ("a", string("x")),
            ("b", array(&[string("y"), string("z")])),
        ]);
        assert_eq!(o, r#"{"a": "x", "b": ["y", "z"]}"#);
    }

    #[test]
    fn indent_one_matches_python() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("k", vec![1.5, 2.0]);
        assert_eq!(dumps_indent1(&m), "{\n \"k\": [\n  1.5,\n  2.0\n ]\n}");
        let empty: std::collections::BTreeMap<String, u8> = Default::default();
        assert_eq!(dumps_indent1(&empty), "{}");
    }
}
