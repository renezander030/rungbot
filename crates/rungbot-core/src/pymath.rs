//! Two pieces of float arithmetic that must match the reference implementation bit for bit.
//!
//! The strategy was first written in Python, and its rung maths leans on two operations
//! whose results differ from the naive Rust spelling in the last bit, which is enough to
//! move a rung boundary:
//!
//! * `a // b` on floats is not `(a / b).floor()`. Python derives it from `fmod`, so
//!   `1.0 // 0.1` is `9.0` where `(1.0 / 0.1).floor()` is `10.0`.
//! * `sum()` over floats is compensated (Neumaier) since Python 3.12, not a plain
//!   left fold.
//!
//! Both are pure and cheap, so the core uses them wherever the reference did.

/// Python's float floor division, `a // b`, as CPython computes it.
pub fn floordiv(a: f64, b: f64) -> f64 {
    if b == 0.0 || !a.is_finite() || !b.is_finite() {
        return (a / b).floor();
    }
    let mut m = a % b; // C fmod: same sign as `a`
    let mut div = (a - m) / b;
    if m != 0.0 {
        if (b < 0.0) != (m < 0.0) {
            m += b;
            div -= 1.0;
        }
    } else {
        m = 0.0f64.copysign(b);
    }
    let _ = m;
    if div != 0.0 {
        let mut f = div.floor();
        if div - f > 0.5 {
            f += 1.0;
        }
        f
    } else {
        0.0f64.copysign(a / b)
    }
}

/// Python's built-in `sum()` over floats (3.12+): Neumaier-compensated.
pub fn fsum_py<I: IntoIterator<Item = f64>>(xs: I) -> f64 {
    let mut s = 0.0f64;
    let mut c = 0.0f64;
    for x in xs {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_division_follows_fmod_not_the_quotient() {
        assert_eq!(floordiv(1.0, 0.1), 9.0, "the classic divergence");
        assert_eq!((1.0f64 / 0.1).floor(), 10.0);
        assert_eq!(floordiv(7.0, 2.0), 3.0);
        assert_eq!(floordiv(-7.0, 2.0), -4.0);
        assert_eq!(floordiv(15.0, 5.0), 3.0);
        assert_eq!(floordiv(0.0, 5.0), 0.0);
    }

    #[test]
    fn sum_is_compensated() {
        assert_eq!(
            fsum_py([0.1; 10]),
            1.0,
            "a plain fold gives 0.9999999999999999"
        );
        assert_eq!(fsum_py([1e100, 1.0, -1e100]), 1.0);
        assert_eq!(fsum_py(std::iter::empty()), 0.0);
    }
}
