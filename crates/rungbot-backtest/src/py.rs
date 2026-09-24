//! Python's number formatting, rounding and value printing, reproduced exactly.
//!
//! The backtests' output is a contract: the monthly verdict parses the window and sweep
//! reports with patterns, and people diff these reports month over month. The reference
//! implementation printed them with Python f-strings, so this module reproduces the parts
//! of Python the reports lean on, to the byte:
//!
//! * the format-spec mini-language (`{:>+7.1f}`, `{:,.2f}`, `{:.4g}`, `{:.0%}`),
//! * `repr(float)` (shortest round-trip digits, Python's exponent thresholds),
//! * `round(x, n)` and `round(x)` (correctly rounded, ties to even),
//! * `statistics.mean` (exact, via rational arithmetic) and `statistics.median`,
//! * `json.dumps` and `json.loads` with key order preserved, via [`Py`].

use std::cmp::Ordering;
use std::fmt::Write as _;

pub use rungbot_core::pymath::{floordiv, fsum_py as sum};

// ------------------------------------------------------------------ format spec

#[derive(Debug, Clone, Default)]
struct Spec {
    fill: Option<char>,
    align: Option<char>,
    sign: Option<char>,
    alt: bool,
    zero: bool,
    width: usize,
    grouping: Option<char>,
    precision: Option<usize>,
    ty: Option<char>,
}

fn parse_spec(spec: &str) -> Spec {
    let c: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut s = Spec::default();
    let is_align = |ch: char| matches!(ch, '<' | '>' | '^' | '=');
    if c.len() >= 2 && is_align(c[1]) {
        s.fill = Some(c[0]);
        s.align = Some(c[1]);
        i = 2;
    } else if !c.is_empty() && is_align(c[0]) {
        s.align = Some(c[0]);
        i = 1;
    }
    if i < c.len() && matches!(c[i], '+' | '-' | ' ') {
        s.sign = Some(c[i]);
        i += 1;
    }
    if i < c.len() && c[i] == 'z' {
        i += 1;
    }
    if i < c.len() && c[i] == '#' {
        s.alt = true;
        i += 1;
    }
    if i < c.len() && c[i] == '0' {
        s.zero = true;
        i += 1;
    }
    let mut w = String::new();
    while i < c.len() && c[i].is_ascii_digit() {
        w.push(c[i]);
        i += 1;
    }
    s.width = w.parse().unwrap_or(0);
    if i < c.len() && matches!(c[i], ',' | '_') {
        s.grouping = Some(c[i]);
        i += 1;
    }
    if i < c.len() && c[i] == '.' {
        i += 1;
        let mut p = String::new();
        while i < c.len() && c[i].is_ascii_digit() {
            p.push(c[i]);
            i += 1;
        }
        s.precision = Some(p.parse().unwrap_or(0));
    }
    if i < c.len() {
        s.ty = Some(c[i]);
    }
    s
}

fn group(int_part: &str, sep: char) -> String {
    let digits: Vec<char> = int_part.chars().collect();
    let mut out = String::new();
    for (k, ch) in digits.iter().enumerate() {
        if k > 0 && (digits.len() - k) % 3 == 0 {
            out.push(sep);
        }
        out.push(*ch);
    }
    out
}

/// Pad a formatted number (sign already split off) to the spec's width.
fn pad_number(sign: &str, body: &str, s: &Spec) -> String {
    let len = sign.chars().count() + body.chars().count();
    if len >= s.width {
        return format!("{sign}{body}");
    }
    let n = s.width - len;
    let (fill, align) = match (s.fill, s.align) {
        (f, Some(a)) => (f.unwrap_or(' '), a),
        (_, None) if s.zero => ('0', '='),
        _ => (' ', '>'),
    };
    let pad: String = std::iter::repeat_n(fill, n).collect();
    match align {
        '<' => format!("{sign}{body}{pad}"),
        '^' => {
            let l = n / 2;
            let r = n - l;
            let lp: String = std::iter::repeat_n(fill, l).collect();
            let rp: String = std::iter::repeat_n(fill, r).collect();
            format!("{lp}{sign}{body}{rp}")
        }
        '=' => format!("{sign}{pad}{body}"),
        _ => format!("{pad}{sign}{body}"),
    }
}

fn pad_text(body: &str, s: &Spec) -> String {
    let len = body.chars().count();
    if len >= s.width {
        return body.to_string();
    }
    let n = s.width - len;
    let fill = s.fill.unwrap_or(' ');
    let pad: String = std::iter::repeat_n(fill, n).collect();
    match s.align.unwrap_or('<') {
        '>' => format!("{pad}{body}"),
        '^' => {
            let l = n / 2;
            let lp: String = std::iter::repeat_n(fill, l).collect();
            let rp: String = std::iter::repeat_n(fill, n - l).collect();
            format!("{lp}{body}{rp}")
        }
        _ => format!("{body}{pad}"),
    }
}

fn sign_str(neg: bool, s: &Spec) -> &'static str {
    if neg {
        "-"
    } else {
        match s.sign {
            Some('+') => "+",
            Some(' ') => " ",
            _ => "",
        }
    }
}

/// `(digits, exp10)` of `x` rounded to `sig` significant digits (`x` finite, > 0).
fn sci_parts(x: f64, sig: usize) -> (String, i32) {
    let s = format!("{:.*e}", sig.saturating_sub(1), x);
    let (m, e) = s.split_once('e').expect("LowerExp has an exponent");
    (m.replace('.', ""), e.parse().expect("exponent parses"))
}

/// Shortest round-trip digits of a finite positive `x`.
fn shortest_parts(x: f64) -> (String, i32) {
    let s = format!("{x:e}");
    let (m, e) = s.split_once('e').expect("LowerExp has an exponent");
    (m.replace('.', ""), e.parse().expect("exponent parses"))
}

fn exp_suffix(e: i32) -> String {
    let sign = if e < 0 { '-' } else { '+' };
    format!("e{sign}{:02}", e.abs())
}

fn sci_body(digits: &str, e: i32, alt: bool) -> String {
    let (head, tail) = digits.split_at(1);
    let mut s = head.to_string();
    if !tail.is_empty() || alt {
        s.push('.');
        s.push_str(tail);
    }
    s.push_str(&exp_suffix(e));
    s
}

/// Place a decimal point in `digits` whose first digit has weight 10^e.
fn fixed_from_digits(digits: &str, e: i32) -> (String, String) {
    if e < 0 {
        let zeros = "0".repeat((-e - 1) as usize);
        ("0".to_string(), format!("{zeros}{digits}"))
    } else {
        let k = (e + 1) as usize;
        if digits.len() > k {
            (digits[..k].to_string(), digits[k..].to_string())
        } else {
            (
                format!("{digits}{}", "0".repeat(k - digits.len())),
                String::new(),
            )
        }
    }
}

fn strip_frac_zeros(frac: &str) -> &str {
    frac.trim_end_matches('0')
}

/// Python `repr(float)`.
pub fn repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let neg = x.is_sign_negative();
    let a = x.abs();
    let body = if a == 0.0 {
        "0.0".to_string()
    } else {
        let (digits, e) = shortest_parts(a);
        if !(-4..16).contains(&e) {
            sci_body(&digits, e, false)
        } else {
            let (i, f) = fixed_from_digits(&digits, e);
            if f.is_empty() {
                format!("{i}.0")
            } else {
                format!("{i}.{f}")
            }
        }
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// The unsigned body of a finite float under a spec (no padding, no sign).
fn float_body(a: f64, s: &Spec) -> String {
    let ty = s.ty;
    let group_int = |int_part: String| match s.grouping {
        Some(g) => group(&int_part, g),
        None => int_part,
    };
    match ty {
        Some('f') | Some('F') | Some('%') => {
            let v = if ty == Some('%') { a * 100.0 } else { a };
            let p = s.precision.unwrap_or(6);
            let txt = format!("{v:.p$}");
            let (i, f) = match txt.split_once('.') {
                Some((i, f)) => (i.to_string(), Some(f.to_string())),
                None => (txt, None),
            };
            let mut out = group_int(i);
            match f {
                Some(f) => {
                    out.push('.');
                    out.push_str(&f);
                }
                None if s.alt => out.push('.'),
                None => {}
            }
            if ty == Some('%') {
                out.push('%');
            }
            out
        }
        Some('e') | Some('E') => {
            let p = s.precision.unwrap_or(6);
            if a == 0.0 {
                let frac = if p > 0 {
                    format!(".{}", "0".repeat(p))
                } else {
                    String::new()
                };
                return format!("0{frac}e+00");
            }
            let (digits, e) = sci_parts(a, p + 1);
            let (head, tail) = digits.split_at(1);
            let mut out = head.to_string();
            if p > 0 || s.alt {
                out.push('.');
                out.push_str(tail);
            }
            out.push_str(&exp_suffix(e));
            out
        }
        Some('g') | Some('G') | None => {
            if ty.is_none() && s.precision.is_none() {
                let r = repr(a);
                return match s.grouping {
                    Some(g) if !r.contains('e') => match r.split_once('.') {
                        Some((i, f)) => format!("{}.{f}", group(i, g)),
                        None => group(&r, g),
                    },
                    _ => r,
                };
            }
            let mut p = s.precision.unwrap_or(6);
            if p == 0 {
                p = 1;
            }
            if a == 0.0 {
                return if ty.is_none() {
                    "0.0".into()
                } else if s.alt {
                    format!("0.{}", "0".repeat(p - 1))
                } else {
                    "0".into()
                };
            }
            let (digits, e) = sci_parts(a, p);
            let use_fixed = if ty.is_none() {
                -4 <= e && e < p as i32 - 1
            } else {
                -4 <= e && e < p as i32
            };
            if use_fixed {
                let (i, f) = fixed_from_digits(&digits, e);
                let f = if s.alt {
                    f
                } else {
                    strip_frac_zeros(&f).to_string()
                };
                let i = group_int(i);
                if f.is_empty() {
                    if ty.is_none() {
                        format!("{i}.0")
                    } else {
                        i
                    }
                } else {
                    format!("{i}.{f}")
                }
            } else {
                let d = if s.alt {
                    digits.clone()
                } else {
                    let t = digits.trim_end_matches('0');
                    if t.is_empty() {
                        "0".to_string()
                    } else {
                        t.to_string()
                    }
                };
                sci_body(&d, e, s.alt)
            }
        }
        Some(other) => panic!("unsupported float format type {other:?}"),
    }
}

/// `format(x, spec)` for a float.
pub fn ff(x: f64, spec: &str) -> String {
    let s = parse_spec(spec);
    if x.is_nan() {
        let sign = sign_str(false, &s);
        return pad_number(sign, "nan", &s);
    }
    let neg = x.is_sign_negative();
    let body = if x.is_infinite() {
        "inf".to_string()
    } else {
        float_body(x.abs(), &s)
    };
    let body = if s.ty.is_some_and(|t| t.is_ascii_uppercase()) {
        body.to_uppercase()
    } else {
        body
    };
    pad_number(sign_str(neg, &s), &body, &s)
}

/// `format(i, spec)` for an int. A float presentation type formats `float(i)`.
pub fn fi(i: i64, spec: &str) -> String {
    let s = parse_spec(spec);
    match s.ty {
        Some('f') | Some('F') | Some('e') | Some('E') | Some('g') | Some('G') | Some('%') => {
            ff(i as f64, spec)
        }
        _ => {
            let digits = i.unsigned_abs().to_string();
            let body = match s.grouping {
                Some(g) => group(&digits, g),
                None => digits,
            };
            pad_number(sign_str(i < 0, &s), &body, &s)
        }
    }
}

/// `format(s, spec)` for a string.
pub fn fs(text: &str, spec: &str) -> String {
    let s = parse_spec(spec);
    let body: String = match s.precision {
        Some(p) => text.chars().take(p).collect(),
        None => text.to_string(),
    };
    pad_text(&body, &s)
}

// ------------------------------------------------------------------ rounding

/// Python `round(x, n)`: correctly rounded to `n` decimals, ties to even.
pub fn round(x: f64, n: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    if n >= 0 {
        let n = n as usize;
        let s = format!("{x:.n$}");
        let v: f64 = s.parse().expect("formatted float parses");
        if v == 0.0 {
            // round() keeps the sign of the input for a zero result
            return 0.0f64.copysign(x);
        }
        v
    } else {
        let m = 10f64.powi(-n);
        round_half_even(x / m) as f64 * m
    }
}

/// Python `round(x)` for a float: nearest int, ties to even.
pub fn round_half_even(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 {
        let t = x.trunc();
        if (t as i64) % 2 == 0 {
            t as i64
        } else {
            r as i64
        }
    } else {
        r as i64
    }
}

// ------------------------------------------------------------------ sequences

/// Python's `seq[i]` index for a possibly negative `i`.
pub fn at(len: usize, i: i64) -> usize {
    let j = if i < 0 { len as i64 + i } else { i };
    assert!(
        j >= 0 && (j as usize) < len,
        "IndexError: index {i} out of range for {len}"
    );
    j as usize
}

/// Python's `seq[a:b]` bounds for possibly negative or out-of-range `a`, `b`.
pub fn slice(len: usize, a: i64, b: i64) -> std::ops::Range<usize> {
    let n = len as i64;
    let fix = |x: i64| if x < 0 { (n + x).max(0) } else { x.min(n) };
    let (a, b) = (fix(a), fix(b));
    if b <= a {
        0..0
    } else {
        a as usize..b as usize
    }
}

// ------------------------------------------------------------------ statistics

/// `statistics.median`.
pub fn median(xs: &[f64]) -> f64 {
    let mut v: Vec<f64> = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let n = v.len();
    assert!(n > 0, "no median for empty data");
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// `statistics.mean` over floats: the exact rational mean, rounded once.
pub fn mean(xs: &[f64]) -> f64 {
    assert!(!xs.is_empty(), "mean requires at least one data point");
    crate::exact::mean(xs)
}

/// `statistics.pstdev` over floats.
///
/// Python computes this from an exact rational sum of squares; this uses the exact mean
/// and then compensated float sums, which agrees to well beyond any printed precision.
pub fn pstdev(xs: &[f64]) -> f64 {
    let m = mean(xs);
    let ss = sum(xs.iter().map(|x| (x - m) * (x - m)));
    (ss / xs.len() as f64).sqrt()
}

// ------------------------------------------------------------------ values

/// A Python value, for reproducing `repr()`, `str()` and `json` exactly.
///
/// Dicts keep insertion order, as Python's do.
#[derive(Debug, Clone, PartialEq)]
pub enum Py {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Py>),
    Tuple(Vec<Py>),
    Dict(Vec<(Py, Py)>),
}

impl From<f64> for Py {
    fn from(v: f64) -> Self {
        Py::Float(v)
    }
}
impl From<i64> for Py {
    fn from(v: i64) -> Self {
        Py::Int(v)
    }
}
impl From<usize> for Py {
    fn from(v: usize) -> Self {
        Py::Int(v as i64)
    }
}
impl From<bool> for Py {
    fn from(v: bool) -> Self {
        Py::Bool(v)
    }
}
impl From<&str> for Py {
    fn from(v: &str) -> Self {
        Py::Str(v.to_string())
    }
}
impl From<String> for Py {
    fn from(v: String) -> Self {
        Py::Str(v)
    }
}
impl<T: Into<Py>> From<Option<T>> for Py {
    fn from(v: Option<T>) -> Self {
        v.map(Into::into).unwrap_or(Py::None)
    }
}

/// Build an ordered dict: `dict![("a", 1.0), ("b", "x")]`.
#[macro_export]
macro_rules! pydict {
    ($(($k:expr, $v:expr)),* $(,)?) => {
        $crate::py::Py::Dict(vec![$(($crate::py::Py::from($k), $crate::py::Py::from($v))),*])
    };
}

fn str_repr(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(q);
    for ch in s.chars() {
        match ch {
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

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{:04x}", u);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Py {
    pub fn dict() -> Py {
        Py::Dict(Vec::new())
    }

    /// Insert or replace a key, keeping first-insertion order like a Python dict.
    pub fn set(&mut self, k: impl Into<Py>, v: impl Into<Py>) {
        let (k, v) = (k.into(), v.into());
        if let Py::Dict(items) = self {
            if let Some(slot) = items.iter_mut().find(|(kk, _)| *kk == k) {
                slot.1 = v;
            } else {
                items.push((k, v));
            }
        } else {
            panic!("set on a non-dict");
        }
    }

    pub fn get(&self, k: &str) -> Option<&Py> {
        match self {
            Py::Dict(items) => items
                .iter()
                .find(|(kk, _)| matches!(kk, Py::Str(s) if s == k))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn items(&self) -> &[(Py, Py)] {
        match self {
            Py::Dict(items) => items,
            _ => &[],
        }
    }

    pub fn as_list(&self) -> &[Py] {
        match self {
            Py::List(v) | Py::Tuple(v) => v,
            _ => &[],
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Py::Float(f) => Some(*f),
            Py::Int(i) => Some(*i as f64),
            Py::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Py::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Py::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Py::None)
    }

    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Py::None => false,
            Py::Bool(b) => *b,
            Py::Int(i) => *i != 0,
            Py::Float(f) => *f != 0.0,
            Py::Str(s) => !s.is_empty(),
            Py::List(v) | Py::Tuple(v) => !v.is_empty(),
            Py::Dict(v) => !v.is_empty(),
        }
    }

    /// `repr(value)`.
    pub fn repr(&self) -> String {
        match self {
            Py::None => "None".into(),
            Py::Bool(b) => if *b { "True" } else { "False" }.into(),
            Py::Int(i) => i.to_string(),
            Py::Float(f) => repr(*f),
            Py::Str(s) => str_repr(s),
            Py::List(v) => format!(
                "[{}]",
                v.iter().map(|x| x.repr()).collect::<Vec<_>>().join(", ")
            ),
            Py::Tuple(v) => {
                if v.len() == 1 {
                    format!("({},)", v[0].repr())
                } else {
                    format!(
                        "({})",
                        v.iter().map(|x| x.repr()).collect::<Vec<_>>().join(", ")
                    )
                }
            }
            Py::Dict(items) => format!(
                "{{{}}}",
                items
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.repr(), v.repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// `str(value)`: like repr, except a string prints bare.
    pub fn to_py_string(&self) -> String {
        match self {
            Py::Str(s) => s.clone(),
            other => other.repr(),
        }
    }

    /// `format(value, spec)`.
    pub fn fmt(&self, spec: &str) -> String {
        match self {
            Py::Float(f) => ff(*f, spec),
            Py::Int(i) => fi(*i, spec),
            Py::Bool(b) => fi(*b as i64, spec),
            other => fs(&other.to_py_string(), spec),
        }
    }

    fn json_key(&self) -> String {
        match self {
            Py::Str(s) => json_str(s),
            Py::Int(i) => json_str(&i.to_string()),
            Py::Float(f) => json_str(&repr(*f)),
            Py::Bool(b) => json_str(if *b { "true" } else { "false" }),
            Py::None => json_str("null"),
            other => json_str(&other.to_py_string()),
        }
    }

    /// `json.dumps(value, indent=indent)`.
    pub fn json(&self, indent: Option<usize>) -> String {
        let mut out = String::new();
        self.json_into(&mut out, indent, 0);
        out
    }

    fn json_into(&self, out: &mut String, indent: Option<usize>, level: usize) {
        match self {
            Py::None => out.push_str("null"),
            Py::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Py::Int(i) => out.push_str(&i.to_string()),
            Py::Float(f) => {
                if f.is_nan() {
                    out.push_str("NaN")
                } else if f.is_infinite() {
                    out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" })
                } else {
                    out.push_str(&repr(*f))
                }
            }
            Py::Str(s) => out.push_str(&json_str(s)),
            Py::List(v) | Py::Tuple(v) => {
                if v.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (k, x) in v.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                        if indent.is_none() {
                            out.push(' ');
                        }
                    }
                    newline(out, indent, level + 1);
                    x.json_into(out, indent, level + 1);
                }
                newline(out, indent, level);
                out.push(']');
            }
            Py::Dict(items) => {
                if items.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (k, (key, v)) in items.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                        if indent.is_none() {
                            out.push(' ');
                        }
                    }
                    newline(out, indent, level + 1);
                    out.push_str(&key.json_key());
                    out.push_str(": ");
                    v.json_into(out, indent, level + 1);
                }
                newline(out, indent, level);
                out.push('}');
            }
        }
    }
}

fn newline(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(n) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

// ------------------------------------------------------------------ json.loads

/// `json.loads`, keeping key order and the int/float distinction.
pub fn loads(text: &str) -> Result<Py, String> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value()?;
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
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\n' | b'\r' | b'\t') {
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

    fn value(&mut self) -> Result<Py, String> {
        let Some(&c) = self.s.get(self.i) else {
            return Err("unexpected end of JSON".into());
        };
        match c {
            b'{' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.eat("}") {
                    return Ok(Py::Dict(items));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if !self.eat(":") {
                        return Err(format!("expected ':' at byte {}", self.i));
                    }
                    self.ws();
                    let v = self.value()?;
                    // Python keeps the first position and the last value of a repeated key.
                    let key = Py::Str(k);
                    if let Some(slot) = items.iter_mut().find(|(kk, _): &&mut (Py, Py)| *kk == key)
                    {
                        slot.1 = v;
                    } else {
                        items.push((key, v));
                    }
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("}") {
                        return Ok(Py::Dict(items));
                    }
                    return Err(format!("expected ',' or '}}' at byte {}", self.i));
                }
            }
            b'[' => {
                self.i += 1;
                let mut v = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Ok(Py::List(v));
                }
                loop {
                    self.ws();
                    v.push(self.value()?);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("]") {
                        return Ok(Py::List(v));
                    }
                    return Err(format!("expected ',' or ']' at byte {}", self.i));
                }
            }
            b'"' => Ok(Py::Str(self.string()?)),
            b't' if self.eat("true") => Ok(Py::Bool(true)),
            b'f' if self.eat("false") => Ok(Py::Bool(false)),
            b'n' if self.eat("null") => Ok(Py::None),
            b'N' if self.eat("NaN") => Ok(Py::Float(f64::NAN)),
            b'I' if self.eat("Infinity") => Ok(Py::Float(f64::INFINITY)),
            b'-' if self.eat("-Infinity") => Ok(Py::Float(f64::NEG_INFINITY)),
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Result<Py, String> {
        let start = self.i;
        let mut is_float = false;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'0'..=b'9' | b'-' | b'+' => {}
                b'.' | b'e' | b'E' => is_float = true,
                _ => break,
            }
            self.i += 1;
        }
        let t = std::str::from_utf8(&self.s[start..self.i]).map_err(|e| e.to_string())?;
        if t.is_empty() {
            return Err(format!("unexpected byte at {}", start));
        }
        if is_float {
            t.parse::<f64>()
                .map(Py::Float)
                .map_err(|e| format!("{t}: {e}"))
        } else {
            match t.parse::<i64>() {
                Ok(i) => Ok(Py::Int(i)),
                Err(_) => t
                    .parse::<f64>()
                    .map(Py::Float)
                    .map_err(|e| format!("{t}: {e}")),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if !self.eat("\"") {
            return Err(format!("expected a string at byte {}", self.i));
        }
        let mut out: Vec<u16> = Vec::new();
        let mut raw = String::new();
        let flush = |out: &mut Vec<u16>, raw: &mut String| {
            if !out.is_empty() {
                raw.push_str(&String::from_utf16_lossy(out));
                out.clear();
            }
        };
        loop {
            let Some(&c) = self.s.get(self.i) else {
                return Err("unterminated string".into());
            };
            match c {
                b'"' => {
                    self.i += 1;
                    flush(&mut out, &mut raw);
                    return Ok(raw);
                }
                b'\\' => {
                    let e = *self.s.get(self.i + 1).ok_or("bad escape")?;
                    self.i += 2;
                    let ch = match e {
                        b'"' => Some('"'),
                        b'\\' => Some('\\'),
                        b'/' => Some('/'),
                        b'b' => Some('\u{8}'),
                        b'f' => Some('\u{c}'),
                        b'n' => Some('\n'),
                        b'r' => Some('\r'),
                        b't' => Some('\t'),
                        b'u' => {
                            let h = std::str::from_utf8(&self.s[self.i..self.i + 4])
                                .map_err(|e| e.to_string())?;
                            let u = u16::from_str_radix(h, 16).map_err(|e| e.to_string())?;
                            self.i += 4;
                            out.push(u);
                            None
                        }
                        _ => return Err("bad escape".into()),
                    };
                    if let Some(ch) = ch {
                        flush(&mut out, &mut raw);
                        raw.push(ch);
                    }
                }
                _ => {
                    flush(&mut out, &mut raw);
                    // copy one UTF-8 scalar
                    let rest = std::str::from_utf8(&self.s[self.i..]).map_err(|e| e.to_string())?;
                    let ch = rest.chars().next().expect("non-empty");
                    raw.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_matches_python() {
        assert_eq!(repr(0.1), "0.1");
        assert_eq!(repr(1.0), "1.0");
        assert_eq!(repr(-0.0), "-0.0");
        assert_eq!(repr(1e16), "1e+16");
        assert_eq!(repr(1e15), "1000000000000000.0");
        assert_eq!(repr(0.0001), "0.0001");
        assert_eq!(repr(0.00001), "1e-05");
        assert_eq!(repr(1.5e-7), "1.5e-07");
        assert_eq!(repr(123456.785), "123456.785");
        assert_eq!(repr(2.5e20), "2.5e+20");
    }

    #[test]
    fn format_spec_matches_python() {
        assert_eq!(ff(1234.5, ",.2f"), "1,234.50");
        assert_eq!(ff(-3.24159, "+6.1f"), "  -3.2");
        assert_eq!(ff(3.24159, "+6.1f"), "  +3.2");
        assert_eq!(ff(0.05, ">+7.1f"), "   +0.1");
        assert_eq!(ff(0.02108, ".4g"), "0.02108");
        assert_eq!(ff(2000.0, ".4g"), "2000");
        assert_eq!(ff(12345.0, ".4g"), "1.234e+04");
        assert_eq!(ff(0.00001234, ".4g"), "1.234e-05");
        assert_eq!(ff(1.0, ".4g"), "1");
        assert_eq!(ff(0.2512, "+.0%"), "+25%");
        assert_eq!(ff(12.0, "<9.4g"), "12       ");
        assert_eq!(ff(4158.4, ">8.0f"), "    4158");
        assert_eq!(ff(0.5, ".0f"), "0");
        assert_eq!(ff(1.5, ".0f"), "2");
        assert_eq!(ff(-0.04, ".1f"), "-0.0");
        assert_eq!(ff(1234567.0, ",.0f"), "1,234,567");
        assert_eq!(ff(80811.0, ">10.6g"), "     80811");
        assert_eq!(fi(-4, ">+4"), "  -4");
        assert_eq!(fi(12, ">3d"), " 12");
        assert_eq!(fs("BTC", "5"), "BTC  ");
        assert_eq!(fs("x", ">4"), "   x");
    }

    #[test]
    fn round_is_correct_and_ties_to_even() {
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(0.375, 2), 0.38);
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(3.5), 4);
        assert_eq!(round_half_even(-2.5), -2);
        assert_eq!(round_half_even(2.4), 2);
    }

    #[test]
    fn values_print_like_python() {
        let d = pydict![("a", 1.0), ("b", Py::None), ("c", "it's")];
        assert_eq!(d.repr(), "{'a': 1.0, 'b': None, 'c': \"it's\"}");
        assert_eq!(Py::Tuple(vec![Py::Int(1)]).repr(), "(1,)");
        assert_eq!(d.json(None), r#"{"a": 1.0, "b": null, "c": "it's"}"#);
        assert_eq!(
            pydict![("a", Py::List(vec![Py::Int(1)])), ("b", Py::dict())].json(Some(1)),
            "{\n \"a\": [\n  1\n ],\n \"b\": {}\n}"
        );
    }

    #[test]
    fn loads_keeps_order_and_number_kinds() {
        let v = loads(r#"{"z": 1, "a": 1.0, "s": "éx", "l": [true, null]}"#).unwrap();
        let keys: Vec<String> = v.items().iter().map(|(k, _)| k.repr()).collect();
        assert_eq!(keys, ["'z'", "'a'", "'s'", "'l'"]);
        assert_eq!(v.get("z"), Some(&Py::Int(1)));
        assert_eq!(v.get("a"), Some(&Py::Float(1.0)));
        assert_eq!(v.get("s"), Some(&Py::Str("éx".into())));
    }

    #[test]
    fn statistics_match_python() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert_eq!(mean(&[0.1, 0.2, 0.3]), 0.2);
        assert_eq!(mean(&[1e16, 1.0, -1e16]), 1.0 / 3.0);
    }
}
