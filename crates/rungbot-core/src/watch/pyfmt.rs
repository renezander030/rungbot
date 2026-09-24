//! Python's number formatting, reproduced exactly.
//!
//! Every watcher message and log line was defined by a Python format spec (`:,.0f`,
//! `:.6g`, `:+.2f`, `repr`), and the mails are compared byte for byte against the old
//! ones. Rust's `{:.N}` already rounds exactly like Python (half to even on the exact
//! binary value); what Rust lacks is `g`, the thousands separator and Python's `repr`
//! rules for when to switch to an exponent. Those are here.

/// `format(x, '.Nf')`, with Python's spellings of the non-finite values.
pub fn fixed(x: f64, prec: usize) -> String {
    match nonfinite(x) {
        Some(s) => s,
        None => format!("{x:.prec$}"),
    }
}

/// `format(x, '+.Nf')`.
pub fn signed(x: f64, prec: usize) -> String {
    match nonfinite(x) {
        Some(s) if s.starts_with('-') => s,
        Some(s) => format!("+{s}"),
        None => format!("{x:+.prec$}"),
    }
}

/// `format(x, ',.Nf')`.
pub fn comma(x: f64, prec: usize) -> String {
    group(&fixed(x, prec))
}

/// `format(x, '.Ng')` (`prec` 0 behaves as 1, as in Python).
pub fn g(x: f64, prec: usize) -> String {
    if let Some(s) = nonfinite(x) {
        return s;
    }
    let p = prec.max(1);
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let sci = format!("{:.*e}", p - 1, x);
    let (mant, exp) = sci.split_once('e').expect("{:e} always has an exponent");
    let exp: i32 = exp.parse().expect("the exponent is an integer");
    if (-4..p as i32).contains(&exp) {
        strip_zeros(&format!("{:.*}", (p as i32 - 1 - exp) as usize, x))
    } else {
        format!("{}e{}", strip_zeros(mant), exp_str(exp))
    }
}

/// `format(x, 'g')`: six significant digits.
pub fn g6(x: f64) -> String {
    g(x, 6)
}

/// `format(x, ',.Ng')`.
pub fn comma_g(x: f64, prec: usize) -> String {
    let s = g(x, prec);
    if s.contains('e') || nonfinite(x).is_some() {
        s
    } else {
        group(&s)
    }
}

/// `repr(x)` / `str(x)` for a float: the shortest round-trip digits, fixed notation for
/// decimal exponents in `-4..16`, else `1e+16` style; always a `.0` on whole numbers.
pub fn repr(x: f64) -> String {
    if let Some(s) = nonfinite(x) {
        return s;
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let sci = format!("{x:e}");
    let (mant, exp) = sci.split_once('e').expect("{:e} always has an exponent");
    let exp: i32 = exp.parse().expect("the exponent is an integer");
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let n = digits.len() as i32;
    let body = if (-4..16).contains(&exp) {
        if exp >= n - 1 {
            format!("{digits}{}.0", "0".repeat((exp - n + 1) as usize))
        } else if exp >= 0 {
            let (a, b) = digits.split_at((exp + 1) as usize);
            format!("{a}.{b}")
        } else {
            format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
        }
    } else {
        let m = if n == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{m}e{}", exp_str(exp))
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// Python's `sum()` over floats, which since 3.12 is compensated (Neumaier): the
/// running total carries a correction term, added back at the end. A plain left fold
/// differs in the last bits, and a last bit is enough to flip `px > sma` on a tie.
pub fn sum<I: IntoIterator<Item = f64>>(vals: I) -> f64 {
    let mut it = vals.into_iter();
    let Some(first) = it.next() else {
        return 0.0;
    };
    // `0 + first`: the int start value turns a -0.0 into 0.0.
    let mut s = 0.0 + first;
    let mut c = 0.0;
    for x in it {
        let t = s + x;
        if s.abs() >= x.abs() {
            c += (s - t) + x;
        } else {
            c += (x - t) + s;
        }
        s = t;
    }
    if c != 0.0 && c.is_finite() {
        s += c;
    }
    s
}

/// Python `round(x, nd)` for a float result.
pub fn round(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.nd$}").parse().unwrap_or(x)
}

/// Python `float(s)` for the forms a JSON string or venue field can take.
pub fn parse_float(s: &str) -> Option<f64> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "nan" | "+nan" | "-nan" => return Some(f64::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => return Some(f64::INFINITY),
        "-inf" | "-infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    let t: String = t.chars().filter(|c| *c != '_').collect();
    t.parse().ok()
}

/// `format(s, 'Ns')`: left-justify to `width` characters.
pub fn ljust(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// Right-justify to `width` characters (numbers' default alignment).
pub fn rjust(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - n))
    }
}

/// Python `a // b` for integers (floor division).
pub fn floordiv(a: i64, b: i64) -> i64 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

fn nonfinite(x: f64) -> Option<String> {
    if x.is_nan() {
        Some("nan".into())
    } else if x.is_infinite() {
        Some(if x > 0.0 { "inf" } else { "-inf" }.into())
    } else {
        None
    }
}

fn exp_str(exp: i32) -> String {
    format!("{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
}

fn strip_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Insert `,` every three digits of the integer part.
fn group(s: &str) -> String {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => match s.strip_prefix('+') {
            Some(r) => ("+", r),
            None => ("", s),
        },
    };
    let (int, frac) = match rest.find('.') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    if !int.bytes().all(|b| b.is_ascii_digit()) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + int.len() / 3);
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}{frac}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_formats_the_watchers_print() {
        assert_eq!(comma(52000.5, 0), "52,000");
        assert_eq!(comma(-1234.5678, 2), "-1,234.57");
        assert_eq!(g6(0.00147), "0.00147");
        assert_eq!(g6(1e16), "1e+16");
        assert_eq!(g6(12.0), "12");
        assert_eq!(comma_g(1234.5678, 6), "1,234.57");
        assert_eq!(signed(0.0, 2), "+0.00");
        assert_eq!(repr(40000.0), "40000.0");
        assert_eq!(repr(1e-5), "1e-05");
        assert_eq!(repr(1e16), "1e+16");
        assert_eq!(repr(0.1), "0.1");
        assert_eq!(round(61234.567, 2), 61234.57);
        assert_eq!(floordiv(-7, 2), -4);
    }
}
