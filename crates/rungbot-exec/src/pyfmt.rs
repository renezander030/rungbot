//! Text formats the journal and the venue clients share with the reference implementation.
//!
//! Venue request bodies are signed byte for byte, error text is stored in the journal and
//! mailed, and the journal file is read by other programs. All three were written by a
//! Python implementation first, so this module reproduces its formats exactly:
//!
//! * [`float_repr`]: Python's `repr(float)`, shortest round-trip digits, scientific below
//!   1e-4 and from 1e16 up (`1e-05`, `1e+16`), always a `.0` on integral values.
//! * [`fixed`] / [`fixed_stripped`]: `f"{x:.Nf}"` and the `.rstrip("0").rstrip(".")` idiom.
//! * [`py_str`]: what `f"{body}"` prints for a parsed JSON response (a dict prints as its
//!   repr, with single quotes and `True`/`None`), keeping the response's key order.
//! * [`dumps`]: `json.dumps` with its default separators or `indent=2`, `ensure_ascii`,
//!   and the float repr above.
//! * [`urlencode`]: `urllib.parse.urlencode` (quote_plus).

use serde::Serialize;

/// Python's `repr(float)`.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    // `{:e}` gives the shortest round-trip digits: "1.2345e-5", "1e16".
    let sci = format!("{:e}", x.abs());
    let (mant, exp) = sci.split_once('e').expect("{:e} always has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let sign = if x < 0.0 { "-" } else { "" };
    if !(-4..16).contains(&exp) {
        let m = if digits.len() == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        let es = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{m}e{es}{:02}", exp.abs());
    }
    let n = digits.len() as i32;
    let body = if exp < 0 {
        format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
    } else if exp + 1 >= n {
        format!("{}{}.0", digits, "0".repeat((exp + 1 - n) as usize))
    } else {
        let p = (exp + 1) as usize;
        format!("{}.{}", &digits[..p], &digits[p..])
    };
    format!("{sign}{body}")
}

/// `f"{x:.{prec}f}"`. Rust's fixed formatting rounds the exact binary value half to
/// even, which is what Python does too.
pub fn fixed(x: f64, prec: usize) -> String {
    format!("{x:.prec$}")
}

/// `f"{x:.8f}".rstrip("0").rstrip(".")`, the amount format two venues are sent.
pub fn fixed_stripped(x: f64, prec: usize) -> String {
    let s = fixed(x, prec);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `urllib.parse.urlencode(pairs)`: `quote_plus` on every key and value.
pub fn urlencode(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn quote_plus(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ------------------------------------------------------------------ JSON, ordered

/// A JSON value that keeps its object keys in document order, the way Python's `json`
/// module does. Only used where order is visible: error text and signed bodies.
#[derive(Debug, Clone, PartialEq)]
pub enum PyVal {
    Null,
    Bool(bool),
    /// An integer literal, kept as written (Python ints are unbounded).
    Int(String),
    Float(f64),
    Str(String),
    List(Vec<PyVal>),
    Dict(Vec<(String, PyVal)>),
}

impl PyVal {
    pub fn str(s: impl Into<String>) -> PyVal {
        PyVal::Str(s.into())
    }

    pub fn dict(pairs: Vec<(&str, PyVal)>) -> PyVal {
        PyVal::Dict(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
}

/// Parse JSON keeping key order. `None` when the text is not JSON.
pub fn parse_ordered(text: &str) -> Option<PyVal> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    (p.i == p.s.len()).then_some(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Option<PyVal> {
        match *self.s.get(self.i)? {
            b'{' => {
                self.i += 1;
                let mut out = Vec::new();
                self.ws();
                if self.eat("}") {
                    return Some(PyVal::Dict(out));
                }
                loop {
                    self.ws();
                    let PyVal::Str(k) = self.string()? else {
                        return None;
                    };
                    self.ws();
                    if !self.eat(":") {
                        return None;
                    }
                    self.ws();
                    let v = self.value()?;
                    // Python keeps the last duplicate, at the first one's position.
                    if let Some(slot) = out.iter_mut().find(|(ek, _)| *ek == k) {
                        slot.1 = v;
                    } else {
                        out.push((k, v));
                    }
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("}").then_some(PyVal::Dict(out));
                }
            }
            b'[' => {
                self.i += 1;
                let mut out = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Some(PyVal::List(out));
                }
                loop {
                    self.ws();
                    out.push(self.value()?);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("]").then_some(PyVal::List(out));
                }
            }
            b'"' => self.string(),
            b't' => self.eat("true").then_some(PyVal::Bool(true)),
            b'f' => self.eat("false").then_some(PyVal::Bool(false)),
            b'n' => self.eat("null").then_some(PyVal::Null),
            b'N' => self.eat("NaN").then_some(PyVal::Float(f64::NAN)),
            b'I' => self.eat("Infinity").then_some(PyVal::Float(f64::INFINITY)),
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Option<PyVal> {
        if self.eat("-Infinity") {
            return Some(PyVal::Float(f64::NEG_INFINITY));
        }
        let start = self.i;
        let mut float = false;
        while let Some(&c) = self.s.get(self.i) {
            match c {
                b'0'..=b'9' | b'-' | b'+' => {}
                b'.' | b'e' | b'E' => float = true,
                _ => break,
            }
            self.i += 1;
        }
        let lit = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        if lit.is_empty() {
            return None;
        }
        if float {
            lit.parse().ok().map(PyVal::Float)
        } else {
            lit.parse::<i128>().ok()?;
            Some(PyVal::Int(lit.trim_start_matches('+').to_string()))
        }
    }

    fn string(&mut self) -> Option<PyVal> {
        if !self.eat("\"") {
            return None;
        }
        let start = self.i;
        let mut escaped = false;
        while self.i < self.s.len() {
            let c = self.s[self.i];
            self.i += 1;
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                let raw = std::str::from_utf8(&self.s[start - 1..self.i]).ok()?;
                return serde_json::from_str::<String>(raw).ok().map(PyVal::Str);
            }
        }
        None
    }
}

/// What `f"{v}"` prints: a string prints bare, everything else as its repr.
pub fn py_str(v: &PyVal) -> String {
    match v {
        PyVal::Str(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python's `repr` of a parsed JSON value.
pub fn py_repr(v: &PyVal) -> String {
    match v {
        PyVal::Null => "None".into(),
        PyVal::Bool(true) => "True".into(),
        PyVal::Bool(false) => "False".into(),
        PyVal::Int(s) => s.clone(),
        PyVal::Float(f) => float_repr(*f),
        PyVal::Str(s) => repr_str(s),
        PyVal::List(xs) => format!(
            "[{}]",
            xs.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        PyVal::Dict(kv) => format!(
            "{{{}}}",
            kv.iter()
                .map(|(k, v)| format!("{}: {}", repr_str(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python's `repr(str)`: single quotes unless the text holds one and no double quote.
pub fn repr_str(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c if (0x80..0xa0).contains(&(c as u32)) => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// The body text of a venue response as `f"{body}"` prints it: parsed JSON as its repr,
/// anything else verbatim.
pub fn body_str(text: &str) -> String {
    match parse_ordered(text) {
        Some(v) => py_str(&v),
        None => text.to_string(),
    }
}

/// Take the first `n` characters, as Python's `s[:n]` does.
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ------------------------------------------------------------------ JSON, writing

/// `json.dumps(v)` for an ordered value: default separators (`", "`, `": "`), or the
/// compact ones (`","`, `":"`) when `compact`.
pub fn dumps_ordered(v: &PyVal, compact: bool) -> String {
    let (item, key) = if compact { (",", ":") } else { (", ", ": ") };
    match v {
        PyVal::Null => "null".into(),
        PyVal::Bool(b) => b.to_string(),
        PyVal::Int(s) => s.clone(),
        PyVal::Float(f) => json_float(*f),
        PyVal::Str(s) => json_str(s),
        PyVal::List(xs) => format!(
            "[{}]",
            xs.iter()
                .map(|x| dumps_ordered(x, compact))
                .collect::<Vec<_>>()
                .join(item)
        ),
        PyVal::Dict(kv) => format!(
            "{{{}}}",
            kv.iter()
                .map(|(k, x)| format!("{}{key}{}", json_str(k), dumps_ordered(x, compact)))
                .collect::<Vec<_>>()
                .join(item)
        ),
    }
}

fn json_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        float_repr(f)
    }
}

/// A JSON string literal with `ensure_ascii`.
pub fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        push_json_char(&mut out, c);
    }
    out.push('"');
    out
}

fn push_json_char(out: &mut String, c: char) {
    match c {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{08}' => out.push_str("\\b"),
        '\u{0c}' => out.push_str("\\f"),
        ' '..='~' => out.push(c),
        c => {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
}

/// `json.dumps(value, indent=…)` for anything serde can serialise, in the value's own
/// field order: `indent: None` gives the default separators, `Some(2)` the journal's
/// layout. Floats print as Python prints them.
pub fn dumps<T: Serialize + ?Sized>(value: &T, indent: Option<usize>) -> String {
    let mut buf = Vec::new();
    let fmt = PyFormatter {
        indent,
        level: 0,
        has_value: false,
    };
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    value
        .serialize(&mut ser)
        .expect("serialising to memory cannot fail");
    String::from_utf8(buf).expect("the formatter writes ASCII")
}

struct PyFormatter {
    indent: Option<usize>,
    level: usize,
    has_value: bool,
}

impl PyFormatter {
    fn newline<W: ?Sized + std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        if let Some(n) = self.indent {
            w.write_all(b"\n")?;
            w.write_all(" ".repeat(n * self.level).as_bytes())?;
        }
        Ok(())
    }
}

impl serde_json::ser::Formatter for PyFormatter {
    fn write_f64<W: ?Sized + std::io::Write>(&mut self, w: &mut W, v: f64) -> std::io::Result<()> {
        w.write_all(json_float(v).as_bytes())
    }

    fn write_f32<W: ?Sized + std::io::Write>(&mut self, w: &mut W, v: f32) -> std::io::Result<()> {
        self.write_f64(w, v as f64)
    }

    fn write_string_fragment<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        let mut out = String::new();
        for c in fragment.chars() {
            push_json_char(&mut out, c);
        }
        w.write_all(out.as_bytes())
    }

    fn begin_array<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.level += 1;
        self.has_value = false;
        w.write_all(b"[")
    }

    fn end_array<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.level -= 1;
        if self.has_value {
            self.newline(w)?;
        }
        w.write_all(b"]")
    }

    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if !first {
            w.write_all(if self.indent.is_some() { b"," } else { b", " })?;
        }
        self.newline(w)
    }

    fn end_array_value<W: ?Sized + std::io::Write>(&mut self, _w: &mut W) -> std::io::Result<()> {
        self.has_value = true;
        Ok(())
    }

    fn begin_object<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.level += 1;
        self.has_value = false;
        w.write_all(b"{")
    }

    fn end_object<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.level -= 1;
        if self.has_value {
            self.newline(w)?;
        }
        w.write_all(b"}")
    }

    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if !first {
            w.write_all(if self.indent.is_some() { b"," } else { b", " })?;
        }
        self.newline(w)
    }

    fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        w.write_all(b": ")
    }

    fn end_object_value<W: ?Sized + std::io::Write>(&mut self, _w: &mut W) -> std::io::Result<()> {
        self.has_value = true;
        Ok(())
    }
}

// ------------------------------------------------------------------ loose values

/// Python truthiness of a JSON value.
pub fn truthy(v: Option<&serde_json::Value>) -> bool {
    use serde_json::Value::*;
    match v {
        None | Some(Null) => false,
        Some(Bool(b)) => *b,
        Some(Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(String(s)) => !s.is_empty(),
        Some(Array(a)) => !a.is_empty(),
        Some(Object(o)) => !o.is_empty(),
    }
}

/// `float(v or 0)`: a falsy value is 0, a number is itself, a string must parse the way
/// Python's `float()` parses it. Anything else is an error, as it is there.
pub fn float_or_zero(v: Option<&serde_json::Value>) -> Result<f64, String> {
    if !truthy(v) {
        return Ok(0.0);
    }
    to_float(v.expect("truthy implies present"))
}

/// Python's `float(v)` for a JSON value.
pub fn to_float(v: &serde_json::Value) -> Result<f64, String> {
    use serde_json::Value::*;
    match v {
        Number(n) => n.as_f64().ok_or_else(|| format!("bad number {n}")),
        Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        String(s) => parse_float(s),
        other => Err(format!(
            "float() argument must be a string or a real number, not '{}'",
            type_name(other)
        )),
    }
}

/// Python's `float(str)`: surrounding whitespace, `inf`/`nan` spellings and digit
/// underscores are accepted.
pub fn parse_float(s: &str) -> Result<f64, String> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    if matches!(unsigned, "inf" | "infinity" | "nan") {
        return lower
            .replace("infinity", "inf")
            .parse()
            .map_err(|_| format!("could not convert string to float: {}", repr_str(s)));
    }
    let clean = if t.contains('_') {
        if t.starts_with('_') || t.ends_with('_') || t.contains("__") {
            return Err(format!(
                "could not convert string to float: {}",
                repr_str(s)
            ));
        }
        t.replace('_', "")
    } else {
        t.to_string()
    };
    let ok_chars = !clean.is_empty()
        && clean
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    match clean.parse::<f64>() {
        Ok(f) if ok_chars => Ok(f),
        _ => Err(format!(
            "could not convert string to float: {}",
            repr_str(s)
        )),
    }
}

/// Python's `str(v)` for a JSON value: a string is itself, `null` is `None`.
pub fn value_str(v: &serde_json::Value) -> String {
    use serde_json::Value::*;
    match v {
        String(s) => s.clone(),
        Null => "None".into(),
        Bool(true) => "True".into(),
        Bool(false) => "False".into(),
        Number(n) if n.is_f64() => float_repr(n.as_f64().unwrap_or(0.0)),
        Number(n) => n.to_string(),
        other => body_str(&other.to_string()),
    }
}

fn type_name(v: &serde_json::Value) -> &'static str {
    use serde_json::Value::*;
    match v {
        Null => "NoneType",
        Bool(_) => "bool",
        Number(_) => "float",
        String(_) => "str",
        Array(_) => "list",
        Object(_) => "dict",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_switches_to_scientific_where_python_does() {
        assert_eq!(float_repr(1e-4), "0.0001");
        assert_eq!(float_repr(1e-5), "1e-05");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1234567890123456.0), "1234567890123456.0");
        assert_eq!(float_repr(-2.5), "-2.5");
        assert_eq!(float_repr(100.0), "100.0");
    }

    #[test]
    fn a_dict_body_prints_as_its_repr_in_document_order() {
        assert_eq!(
            body_str(r#"{"label": "X", "a": [1, 2.0, true, null]}"#),
            "{'label': 'X', 'a': [1, 2.0, True, None]}"
        );
        assert_eq!(body_str("<html>"), "<html>");
        assert_eq!(body_str(r#""quoted""#), "quoted");
    }

    #[test]
    fn floats_parse_like_python() {
        assert_eq!(parse_float(" 1.5 ").unwrap(), 1.5);
        assert_eq!(parse_float("1_0").unwrap(), 10.0);
        assert!(parse_float("abc").is_err());
        assert!(parse_float("0x10").is_err());
        assert!(parse_float("inf").unwrap().is_infinite());
    }
}
