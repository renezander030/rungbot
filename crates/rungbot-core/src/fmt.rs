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

#[cfg(test)]
mod tests {
    use super::*;

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
