//! JSON and text with the semantics the ledger files were first written with.
//!
//! The ledger and index files are plain JSON, but three details of how they were
//! written matter for carrying them across unchanged, and `serde_json` differs on all
//! three:
//!
//! * **Key order.** Stages add keys to existing records, and a record that is written
//!   back keeps its keys where they were. [`Json::Obj`] is an ordered list, and
//!   [`Json::set`] replaces a value in place or appends a new key.
//! * **Integers and floats are different values.** `72` and `72.0` round-trip as
//!   written, so a rounded volume stays an integer and a rounded percentage a float.
//! * **Float text.** Floats print in the shortest round-trip form with a `1e+16` /
//!   `1e-05` exponent style, and non-ASCII text is written as `\uXXXX` escapes.
//!
//! The text helpers ([`strip`], [`splitlines`], [`split_ws`], [`parse_float`]) follow
//! the same conventions, so a verdict line an LLM writes is cut and read the same way.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    /// The value under `key`, if this is an object that has it.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(e) => e.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// `d.get(key) or default`-free lookup: a missing key and `null` are both `None`.
    pub fn get_some(&self, key: &str) -> Option<&Json> {
        self.get(key).filter(|v| !v.is_null())
    }

    /// Replace the value in place, or append the key: dict assignment.
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Obj(e) = self {
            match e.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = value,
                None => e.push((key.to_string(), value)),
            }
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(e) => Some(e),
            _ => None,
        }
    }

    /// A number (a bool counts as 0/1, as it does in arithmetic).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    /// Truthiness: `null`, `false`, `0`, `0.0`, `""`, `[]` and `{}` are false.
    pub fn truthy(&self) -> bool {
        match self {
            Json::Null => false,
            Json::Bool(b) => *b,
            Json::Int(i) => *i != 0,
            Json::Float(f) => *f != 0.0,
            Json::Str(s) => !s.is_empty(),
            Json::Arr(a) => !a.is_empty(),
            Json::Obj(e) => !e.is_empty(),
        }
    }

    /// The text a value prints as when interpolated: `None`, `True`, `72.3`, the string.
    pub fn display(&self) -> String {
        match self {
            Json::Null => "None".into(),
            Json::Bool(true) => "True".into(),
            Json::Bool(false) => "False".into(),
            Json::Int(i) => i.to_string(),
            Json::Float(f) => float_repr(*f),
            Json::Str(s) => s.clone(),
            Json::Arr(_) | Json::Obj(_) => self.repr(),
        }
    }

    /// The literal form used inside a printed list or dict.
    fn repr(&self) -> String {
        match self {
            Json::Str(s) => str_repr(s),
            Json::Arr(a) => format!(
                "[{}]",
                a.iter().map(Json::repr).collect::<Vec<_>>().join(", ")
            ),
            Json::Obj(e) => format!(
                "{{{}}}",
                e.iter()
                    .map(|(k, v)| format!("{}: {}", str_repr(k), v.repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            other => other.display(),
        }
    }
}

fn str_repr(s: &str) -> String {
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
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

// ---------------------------------------------------------------------------------
// Numbers

/// The shortest round-trip text of a float, in the `repr` style: fixed notation for
/// exponents -4..=15, otherwise `d.ddde+XX` with at least two exponent digits.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    let sci = format!("{x:e}");
    let (mant, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.push_str(&"0".repeat(int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.push_str(&"0".repeat((-exp - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(
            out,
            "e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.unsigned_abs()
        );
    }
    out
}

/// `'%.Nf' % x`: correctly rounded, ties to even on the exact binary value.
pub fn fixed(x: f64, prec: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    format!("{x:.prec$}")
}

/// `round(x, n)` for a float.
pub fn round_f(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    fixed(x, n).parse().unwrap_or(x)
}

/// `round(x)`: to an integer, ties to even.
pub fn round_i(x: f64) -> i128 {
    if !x.is_finite() {
        return 0;
    }
    x.round_ties_even() as i128
}

/// `round(v, n)` for a JSON number: an integer stays an integer.
pub fn round_json(v: &Json, n: usize) -> Json {
    match v {
        Json::Int(i) => Json::Int(*i),
        Json::Bool(b) => Json::Int(i128::from(*b)),
        other => Json::Float(round_f(other.as_f64().unwrap_or(0.0), n)),
    }
}

/// `round(v)` for a JSON number: always an integer.
pub fn round_json_int(v: &Json) -> Json {
    match v {
        Json::Int(i) => Json::Int(*i),
        other => Json::Int(round_i(other.as_f64().unwrap_or(0.0))),
    }
}

/// `-v` for a JSON number.
pub fn neg_json(v: &Json) -> Json {
    match v {
        Json::Int(i) => Json::Int(-*i),
        Json::Bool(b) => Json::Int(-i128::from(*b)),
        other => Json::Float(-other.as_f64().unwrap_or(0.0)),
    }
}

/// `v or 0`: the value when truthy, else the integer 0.
pub fn or_zero(v: Option<&Json>) -> Json {
    match v {
        Some(x) if x.truthy() => x.clone(),
        _ => Json::Int(0),
    }
}

/// `float(s)` for text: surrounding whitespace allowed, `_` between digits allowed.
pub fn parse_float(s: &str) -> Option<f64> {
    let t = strip(s);
    if t.contains('_') {
        let b: Vec<char> = t.chars().collect();
        for (i, c) in b.iter().enumerate() {
            if *c == '_'
                && !(i > 0
                    && b[i - 1].is_ascii_digit()
                    && b.get(i + 1).is_some_and(|n| n.is_ascii_digit()))
            {
                return None;
            }
        }
        return t.replace('_', "").parse().ok();
    }
    t.parse().ok()
}

// ---------------------------------------------------------------------------------
// Text

/// Whitespace as `str.strip()` / `str.split()` count it: Unicode White_Space plus the
/// four ASCII separators 0x1c..0x1f.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `str.split()` with no argument.
pub fn split_ws(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|p| !p.is_empty()).collect()
}

fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// `str.splitlines()`: every line boundary, `\r\n` as one, no empty tail.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if is_line_break(c) {
            out.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(j, '\n')) = it.peek() {
                    it.next();
                    end = j + 1;
                }
            }
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// The first `n` characters: `s[:n]`.
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Right-align to `w` characters, `{:>w}`.
pub fn rjust(s: &str, w: usize) -> String {
    format!("{s:>w$}")
}

/// Left-align to `w` characters, `{:<w}`.
pub fn ljust(s: &str, w: usize) -> String {
    format!("{s:<w$}")
}

/// Fill `{name}` placeholders in one pass, so a value that itself contains a
/// placeholder is not expanded again.
pub fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 256);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match values.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------------
// JSON text

/// Parse JSON text, keeping key order and the integer/float distinction. `NaN` and
/// `Infinity` are accepted, as the ledger writer could produce them.
pub fn parse(text: &str) -> Result<Json, String> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("trailing data at byte {}", p.i));
    }
    Ok(v)
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

    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("{what} at byte {}", self.i))
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.s[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            self.err("invalid literal")
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > 512 {
            return self.err("nesting too deep");
        }
        match self.s.get(self.i) {
            None => self.err("unexpected end"),
            Some(b'{') => {
                self.i += 1;
                let mut obj = Json::obj();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(obj);
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&b'"') {
                        return self.err("expected a key");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return self.err("expected ':'");
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    obj.set(&k, v);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(obj);
                        }
                        _ => return self.err("expected ',' or '}'"),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut arr = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(arr));
                }
                loop {
                    self.ws();
                    arr.push(self.value(depth + 1)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(arr));
                        }
                        _ => return self.err("expected ',' or ']'"),
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(b'N') => self.lit("NaN", Json::Float(f64::NAN)),
            Some(b'I') => self.lit("Infinity", Json::Float(f64::INFINITY)),
            Some(b'-') if self.s[self.i..].starts_with(b"-Infinity") => {
                self.i += 9;
                Ok(Json::Float(f64::NEG_INFINITY))
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => self.err("unexpected character"),
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        let mut is_float = false;
        if self.s[self.i] == b'-' {
            self.i += 1;
        }
        let digits_start = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        if self.i == digits_start {
            return self.err("invalid number");
        }
        if self.s.get(self.i) == Some(&b'.') {
            is_float = true;
            self.i += 1;
            let f = self.i;
            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                self.i += 1;
            }
            if self.i == f {
                return self.err("invalid number");
            }
        }
        if matches!(self.s.get(self.i), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            let e = self.i;
            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                self.i += 1;
            }
            if self.i == e {
                return self.err("invalid number");
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("0");
        if !is_float {
            if let Ok(i) = text.parse::<i128>() {
                return Ok(Json::Int(i));
            }
        }
        text.parse::<f64>()
            .map(Json::Float)
            .or_else(|_| self.err("invalid number"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self
            .s
            .get(self.i..self.i + 4)
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|t| u32::from_str_radix(t, 16).ok());
        match h {
            Some(v) => {
                self.i += 4;
                Ok(v)
            }
            None => self.err("invalid \\u escape"),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let run_start = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'"' && self.s[self.i] != b'\\' {
                self.i += 1;
            }
            out.push_str(
                std::str::from_utf8(&self.s[run_start..self.i])
                    .map_err(|_| format!("invalid UTF-8 at byte {run_start}"))?,
            );
            match self.s.get(self.i) {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                _ => {
                    self.i += 1;
                    let Some(&e) = self.s.get(self.i) else {
                        return self.err("unterminated escape");
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xd800..0xdc00).contains(&hi)
                                && self.s[self.i..].starts_with(b"\\u")
                            {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xdc00..0xe000).contains(&lo) {
                                    0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                                } else {
                                    self.i = save;
                                    hi
                                }
                            } else {
                                hi
                            };
                            // A lone surrogate has no char; it becomes U+FFFD.
                            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        }
                        _ => return self.err("invalid escape"),
                    }
                }
            }
        }
    }
}

/// A string literal with every non-printable-ASCII character escaped.
pub fn quote(s: &str) -> String {
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
            ' '..='~' => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{u:04x}");
                }
            }
        }
    }
    out.push('"');
    out
}

fn number_text(v: &Json) -> String {
    match v {
        Json::Int(i) => i.to_string(),
        Json::Float(f) if f.is_nan() => "NaN".into(),
        Json::Float(f) if f.is_infinite() => {
            if *f > 0.0 {
                "Infinity".into()
            } else {
                "-Infinity".into()
            }
        }
        Json::Float(f) => float_repr(*f),
        _ => unreachable!("number_text on a non-number"),
    }
}

/// Compact text with `", "` and `": "` separators: a request body.
pub fn dumps(v: &Json) -> String {
    let mut out = String::new();
    write_compact(&mut out, v);
    out
}

fn write_compact(out: &mut String, v: &Json) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(_) | Json::Float(_) => out.push_str(&number_text(v)),
        Json::Str(s) => out.push_str(&quote(s)),
        Json::Arr(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_compact(out, x);
            }
            out.push(']');
        }
        Json::Obj(e) => {
            out.push('{');
            for (i, (k, x)) in e.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&quote(k));
                out.push_str(": ");
                write_compact(out, x);
            }
            out.push('}');
        }
    }
}

/// Indented text: the ledger and index file format (`indent=2`, no trailing newline).
pub fn dumps_indent(v: &Json, indent: usize) -> String {
    let mut out = String::new();
    write_indent(&mut out, v, indent, 0);
    out
}

fn write_indent(out: &mut String, v: &Json, step: usize, level: usize) {
    match v {
        Json::Arr(a) if !a.is_empty() => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&" ".repeat(step * (level + 1)));
                write_indent(out, x, step, level + 1);
            }
            out.push('\n');
            out.push_str(&" ".repeat(step * level));
            out.push(']');
        }
        Json::Obj(e) if !e.is_empty() => {
            out.push('{');
            for (i, (k, x)) in e.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&" ".repeat(step * (level + 1)));
                out.push_str(&quote(k));
                out.push_str(": ");
                write_indent(out, x, step, level + 1);
            }
            out.push('\n');
            out.push_str(&" ".repeat(step * level));
            out.push('}');
        }
        other => write_compact(out, other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_text_matches_the_repr_rules() {
        let cases = [
            (72.3, "72.3"),
            (-72.25, "-72.25"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (1.5e-7, "1.5e-07"),
            (1.5e300, "1.5e+300"),
            (-0.0, "-0.0"),
            (100.0, "100.0"),
            (0.1 + 0.2, "0.30000000000000004"),
        ];
        for (x, want) in cases {
            assert_eq!(float_repr(x), want, "{x:e}");
        }
    }

    #[test]
    fn rounding_is_ties_to_even_on_the_exact_value() {
        assert_eq!(round_f(72.25, 1), 72.2);
        assert_eq!(round_f(2.675, 2), 2.67);
        assert_eq!(round_f(0.375, 2), 0.38);
        assert_eq!(round_i(2.5), 2);
        assert_eq!(round_i(3.5), 4);
        assert_eq!(round_i(-2.5), -2);
    }

    #[test]
    fn parse_then_dump_keeps_order_types_and_escapes() {
        let text = r#"[{"b": 1, "a": 2.0, "s": "café 🧭", "n": null, "l": [], "o": {}}]"#;
        let v = parse(text).unwrap();
        assert_eq!(
            dumps_indent(&v, 2),
            "[\n  {\n    \"b\": 1,\n    \"a\": 2.0,\n    \"s\": \"caf\\u00e9 \\ud83e\\udded\",\n    \"n\": null,\n    \"l\": [],\n    \"o\": {}\n  }\n]"
        );
        assert_eq!(
            dumps(&v),
            "[{\"b\": 1, \"a\": 2.0, \"s\": \"caf\\u00e9 \\ud83e\\udded\", \"n\": null, \"l\": [], \"o\": {}}]"
        );
    }

    #[test]
    fn a_duplicate_key_keeps_its_first_position_and_last_value() {
        let v = parse(r#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(dumps(&v), r#"{"a": 3, "b": 2}"#);
    }

    #[test]
    fn splitlines_and_strip_follow_the_text_rules() {
        assert_eq!(splitlines("a\r\nb\rc\n"), vec!["a", "b", "c"]);
        assert_eq!(splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert!(splitlines("").is_empty());
        assert_eq!(strip("\u{1c} x \u{a0}"), "x");
        assert_eq!(
            split_ws("  SURVIVOR  (strong)"),
            vec!["SURVIVOR", "(strong)"]
        );
    }

    #[test]
    fn float_text_parsing_accepts_what_float_accepts() {
        assert_eq!(parse_float(" 72 "), Some(72.0));
        assert_eq!(parse_float("1_000"), Some(1000.0));
        assert_eq!(parse_float("1__0"), None);
        assert_eq!(parse_float("72/100"), None);
        assert_eq!(parse_float(""), None);
    }

    #[test]
    fn fill_expands_each_placeholder_once() {
        assert_eq!(
            fill("{a} and {b} {c}", &[("a", "{b}"), ("b", "x")]),
            "{b} and x {c}"
        );
    }
}
