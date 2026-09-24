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

/// `math.fsum`: the exactly rounded sum (Shewchuk's partials, with CPython's final
/// half-way correction). `statistics.fmean` is `fsum(xs) / len(xs)`.
pub fn fsum_exact<I: IntoIterator<Item = f64>>(xs: I) -> f64 {
    let mut partials: Vec<f64> = Vec::new();
    for mut x in xs {
        let mut i = 0;
        for j in 0..partials.len() {
            let mut y = partials[j];
            if x.abs() < y.abs() {
                core::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        partials.truncate(i);
        partials.push(x);
    }
    let mut n = partials.len();
    if n == 0 {
        return 0.0;
    }
    n -= 1;
    let mut hi = partials[n];
    let mut lo = 0.0;
    while n > 0 {
        let x = hi;
        n -= 1;
        let y = partials[n];
        hi = x + y;
        let yr = hi - x;
        lo = y - yr;
        if lo != 0.0 {
            break;
        }
    }
    if n > 0 && ((lo < 0.0 && partials[n - 1] < 0.0) || (lo > 0.0 && partials[n - 1] > 0.0)) {
        let y = lo * 2.0;
        let x = hi + y;
        let yr = x - hi;
        if y == yr {
            hi = x;
        }
    }
    hi
}

/// `statistics.median` over floats: the middle value, or the mean of the two middle
/// values (`(a + b) / 2`). `None` for no data.
pub fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsum_is_exact_and_median_averages_the_middle() {
        assert_eq!(fsum_exact([0.1; 10]), 1.0);
        assert_eq!(fsum_exact([1.0, 1e-16, 1e-16]), 1.0000000000000002);
        assert_eq!(fsum_exact([1e100, 1.0, -1e100, 1e-100]), 1.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&[]), None);
    }

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
