//! A JSON value that behaves like a Python `dict`/`list` tree.
//!
//! The watchers read state files another process wrote and write state files a human (or
//! another tool) reads back. Three things a plain map type gets wrong for that job:
//!
//! * **Key order.** Python dicts keep insertion order and `json.dumps` writes it. A sorted
//!   map reorders every file it touches.
//! * **Integer vs float.** `28` and `28.0` print differently (`str()` gives `28` and
//!   `28.0`), and messages print some of these values verbatim.
//! * **Output bytes.** [`Json::dumps`] writes exactly what `json.dumps(obj)` /
//!   `json.dumps(obj, indent=n)` writes, so a state file round-trips byte for byte.
//!
//! Pure data: no I/O. `serde` does the parsing, so any `serde` format can produce one.

use std::fmt;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::pyfmt;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Json {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    /// `d.get(key)`, `None` for a missing key or a non-object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(e) => e.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Obj(e) => e.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// `d[key] = v`: replaces in place (keeping the key's position) or appends.
    pub fn set(&mut self, key: &str, v: Json) {
        if let Json::Obj(e) = self {
            match e.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = v,
                None => e.push((key.to_string(), v)),
            }
        }
    }

    /// `d.setdefault(key, {})`, returning the slot.
    pub fn setdefault_obj(&mut self, key: &str) -> Option<&mut Json> {
        if !self.contains_key(key) {
            self.set(key, Json::obj());
        }
        self.get_mut(key)
    }

    pub fn remove(&mut self, key: &str) -> Option<Json> {
        match self {
            Json::Obj(e) => {
                let i = e.iter().position(|(k, _)| k == key)?;
                Some(e.remove(i).1)
            }
            _ => None,
        }
    }

    pub fn entries(&self) -> &[(String, Json)] {
        match self {
            Json::Obj(e) => e,
            _ => &[],
        }
    }

    pub fn items(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            _ => &[],
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn is_obj(&self) -> bool {
        matches!(self, Json::Obj(_))
    }

    pub fn is_num(&self) -> bool {
        matches!(self, Json::Int(_) | Json::Float(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// A JSON number as `f64` (bools are not numbers here).
    pub fn num(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Python `float(x)`: numbers, bools and numeric strings; `None` where Python raises.
    pub fn to_float(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Json::Str(s) => pyfmt::parse_float(s),
            _ => None,
        }
    }

    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Json::Null => false,
            Json::Bool(b) => *b,
            Json::Int(i) => *i != 0,
            Json::Float(f) => *f != 0.0,
            Json::Str(s) => !s.is_empty(),
            Json::Arr(a) => !a.is_empty(),
            Json::Obj(o) => !o.is_empty(),
        }
    }

    /// `float(d.get(key, 0) or 0)`: a falsy or missing value is 0.
    pub fn float_or0(&self, key: &str) -> Option<f64> {
        match self.get(key) {
            Some(v) if v.truthy() => v.to_float(),
            _ => Some(0.0),
        }
    }

    /// Python `==` between two decoded JSON values (`1 == 1.0`, `True == 1`).
    pub fn py_eq(&self, other: &Json) -> bool {
        let n = |j: &Json| match j {
            Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Json::Int(i) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            _ => None,
        };
        match (n(self), n(other)) {
            (Some(a), Some(b)) => a == b,
            _ => self == other,
        }
    }

    /// Python `str(x)` for a scalar: `None`, `True`, `28`, `28.0`, the string itself.
    pub fn py_str(&self) -> String {
        match self {
            Json::Null => "None".into(),
            Json::Bool(true) => "True".into(),
            Json::Bool(false) => "False".into(),
            Json::Int(i) => i.to_string(),
            Json::Float(f) => pyfmt::repr(*f),
            Json::Str(s) => s.clone(),
            other => other.dumps(None),
        }
    }

    /// `json.dumps(obj)` (`indent: None`) or `json.dumps(obj, indent=n)`, byte for byte:
    /// `", "`/`": "` separators, `ensure_ascii`, `repr` floats, `NaN`/`Infinity`.
    pub fn dumps(&self, indent: Option<usize>) -> String {
        let mut out = String::new();
        self.write(&mut out, indent, 0);
        out
    }

    fn write(&self, out: &mut String, indent: Option<usize>, level: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(i) => out.push_str(&i.to_string()),
            Json::Float(f) => out.push_str(&float_json(*f)),
            Json::Str(s) => write_str(out, s),
            Json::Arr(a) => {
                if a.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    sep(out, indent, level + 1, i == 0);
                    v.write(out, indent, level + 1);
                }
                close(out, indent, level);
                out.push(']');
            }
            Json::Obj(o) => {
                if o.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    sep(out, indent, level + 1, i == 0);
                    write_str(out, k);
                    out.push_str(": ");
                    v.write(out, indent, level + 1);
                }
                close(out, indent, level);
                out.push('}');
            }
        }
    }
}

fn sep(out: &mut String, indent: Option<usize>, level: usize, first: bool) {
    match indent {
        Some(n) => {
            if !first {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(n * level));
        }
        None => {
            if !first {
                out.push_str(", ");
            }
        }
    }
}

fn close(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(n) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

fn float_json(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        pyfmt::repr(f)
    }
}

/// A JSON string with Python's `ensure_ascii=True` escaping.
pub fn write_str(out: &mut String, s: &str) {
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
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.dumps(None))
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Self {
        Json::Bool(b)
    }
}
impl From<f64> for Json {
    fn from(f: f64) -> Self {
        Json::Float(f)
    }
}
impl From<i64> for Json {
    fn from(i: i64) -> Self {
        Json::Int(i)
    }
}
impl From<&str> for Json {
    fn from(s: &str) -> Self {
        Json::Str(s.to_string())
    }
}
impl From<String> for Json {
    fn from(s: String) -> Self {
        Json::Str(s)
    }
}
impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(o: Option<T>) -> Self {
        o.map(Into::into).unwrap_or(Json::Null)
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => s.serialize_unit(),
            Json::Bool(b) => s.serialize_bool(*b),
            Json::Int(i) => s.serialize_i64(*i),
            Json::Float(f) => s.serialize_f64(*f),
            Json::Str(v) => s.serialize_str(v),
            Json::Arr(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Json::Obj(o) => {
                let mut map = s.serialize_map(Some(o.len()))?;
                for (k, v) in o {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Int(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
        Ok(i64::try_from(v)
            .map(Json::Int)
            .unwrap_or(Json::Float(v as f64)))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        Ok(Json::Float(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Str(v.to_string()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
        Ok(Json::Str(v))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }
    fn visit_none<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Json, D::Error> {
        Json::deserialize(d)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element()? {
            out.push(v);
        }
        Ok(Json::Arr(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        // Document order, and a repeated key keeps its first position with the last
        // value, as Python's json.loads does.
        let mut out = Json::obj();
        while let Some((k, v)) = map.next_entry::<String, Json>()? {
            out.set(&k, v);
        }
        Ok(out)
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        d.deserialize_any(JsonVisitor)
    }
}

/// Build an object from `(key, value)` pairs, in order.
pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_matches_python_separators_and_escapes() {
        let j = obj(vec![
            ("a", Json::Int(1)),
            ("b", Json::Float(2.0)),
            ("c", Json::Arr(vec![Json::Null, Json::Bool(true)])),
            ("d", "\u{fc}\"\u{1F680}".into()),
            ("e", Json::obj()),
        ]);
        assert_eq!(
            j.dumps(None),
            r#"{"a": 1, "b": 2.0, "c": [null, true], "d": "\u00fc\"\ud83d\ude80", "e": {}}"#
        );
        assert_eq!(
            obj(vec![("x", Json::Arr(vec![Json::Int(1)]))]).dumps(Some(2)),
            "{\n  \"x\": [\n    1\n  ]\n}"
        );
    }

    #[test]
    fn set_keeps_position_and_appends_new_keys() {
        let mut j = obj(vec![("a", Json::Int(1)), ("b", Json::Int(2))]);
        j.set("a", Json::Int(9));
        j.set("c", Json::Int(3));
        assert_eq!(j.dumps(None), r#"{"a": 9, "b": 2, "c": 3}"#);
    }

    #[test]
    fn python_equality_and_truthiness() {
        assert!(Json::Int(1).py_eq(&Json::Float(1.0)));
        assert!(Json::Bool(true).py_eq(&Json::Int(1)));
        assert!(!Json::Null.py_eq(&Json::Bool(false)));
        assert!(!Json::Str(String::new()).truthy());
        assert!(Json::Str("0".into()).truthy());
        assert_eq!(Json::Float(28.0).py_str(), "28.0");
        assert_eq!(Json::Int(28).py_str(), "28");
    }
}
