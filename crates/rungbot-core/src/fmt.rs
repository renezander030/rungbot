//! Number formatting shared by the core's human-readable strings.

/// A number the way a person would write it: `10` not `10.0`, but `12.5` stays `12.5`.
///
/// Rust has no `%g`, and every threshold printed here is a tunable a human set, so
/// trailing `.0` on all of them is noise.
pub fn g(v: f64) -> String {
    if v.is_finite() && (v - v.round()).abs() < 1e-9 {
        format!("{:.0}", v)
    } else {
        format!("{v}")
    }
}

/// Python's `f"{x:.{prec}g}"` (`f"{x:g}"` is `prec` 6): `prec` significant digits, fixed
/// notation for decimal exponents from -4 up to `prec - 1` and scientific outside that
/// (`1.5e-05`, `1.23457e+08`), trailing zeros and a bare point removed.
///
/// The live runtime's mails and logs were first written by a Python bot; every number
/// they print through `%g` goes through here so the text stays byte-identical.
pub fn py_g(x: f64, prec: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let p = prec.max(1);
    // Round to `p` significant digits once; the exponent of that rounding picks the
    // notation, which is how Python picks it.
    let sci = format!("{:.*e}", p - 1, x);
    let (mant, exp) = sci.split_once('e').expect("{:e} always has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let strip = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if (-4..p as i32).contains(&exp) {
        strip(&format!("{:.*}", (p as i32 - 1 - exp) as usize, x))
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip(mant), exp.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_g_matches_python() {
        assert_eq!(py_g(4.0, 6), "4");
        assert_eq!(py_g(0.000015, 6), "1.5e-05");
        assert_eq!(py_g(123456789.0, 6), "1.23457e+08");
        assert_eq!(py_g(0.123456789, 6), "0.123457");
        assert_eq!(py_g(1500.0, 6), "1500");
    }

    #[test]
    fn whole_numbers_lose_the_decimal_point() {
        assert_eq!(g(10.0), "10");
        assert_eq!(g(0.0), "0");
        assert_eq!(g(-40.0), "-40");
    }

    #[test]
    fn fractions_keep_their_precision() {
        assert_eq!(g(12.5), "12.5");
        assert_eq!(g(0.25), "0.25");
    }
}
