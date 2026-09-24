//! Exact rational mean of floats, rounded once — what `statistics.mean` returns.
//!
//! Every finite double is an integer multiple of 2^-1074, so the sum of any list of them
//! is an integer at that scale. That integer fits in a few hundred 32-bit limbs, which
//! is all the big-number machinery this needs: add, subtract, divide by a small integer,
//! and round the quotient to the nearest double with ties to even.

use std::cmp::Ordering;

#[derive(Clone, Default, PartialEq, Eq)]
struct Big(Vec<u32>); // little-endian limbs, no trailing zeros

impl Big {
    fn trim(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }

    fn is_zero(&self) -> bool {
        self.0.is_empty()
    }

    /// `m << shift` for a 53-bit mantissa.
    fn from_shifted(m: u64, shift: u32) -> Big {
        let limbs = (shift / 32) as usize;
        let bits = shift % 32;
        let mut v = vec![0u32; limbs];
        let wide = (m as u128) << bits;
        let mut w = wide;
        while w != 0 {
            v.push(w as u32);
            w >>= 32;
        }
        let mut b = Big(v);
        b.trim();
        b
    }

    fn add(&mut self, o: &Big) {
        let n = self.0.len().max(o.0.len());
        self.0.resize(n, 0);
        let mut carry = 0u64;
        for i in 0..n {
            let s = self.0[i] as u64 + *o.0.get(i).unwrap_or(&0) as u64 + carry;
            self.0[i] = s as u32;
            carry = s >> 32;
        }
        if carry != 0 {
            self.0.push(carry as u32);
        }
    }

    /// `self - o`, requires `self >= o`.
    fn sub(&self, o: &Big) -> Big {
        let mut out = self.0.clone();
        let mut borrow = 0i64;
        for (i, limb) in out.iter_mut().enumerate() {
            let mut d = *limb as i64 - *o.0.get(i).unwrap_or(&0) as i64 - borrow;
            if d < 0 {
                d += 1 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            *limb = d as u32;
        }
        let mut b = Big(out);
        b.trim();
        b
    }

    fn cmp(&self, o: &Big) -> Ordering {
        if self.0.len() != o.0.len() {
            return self.0.len().cmp(&o.0.len());
        }
        for i in (0..self.0.len()).rev() {
            match self.0[i].cmp(&o.0[i]) {
                Ordering::Equal => continue,
                x => return x,
            }
        }
        Ordering::Equal
    }

    fn divrem(&self, d: u64) -> (Big, u64) {
        let mut q = vec![0u32; self.0.len()];
        let mut r: u128 = 0;
        for i in (0..self.0.len()).rev() {
            let cur = (r << 32) | self.0[i] as u128;
            q[i] = (cur / d as u128) as u32;
            r = cur % d as u128;
        }
        let mut b = Big(q);
        b.trim();
        (b, r as u64)
    }

    fn bits(&self) -> u32 {
        match self.0.last() {
            None => 0,
            Some(top) => (self.0.len() as u32 - 1) * 32 + (32 - top.leading_zeros()),
        }
    }

    fn bit(&self, i: u32) -> bool {
        let limb = (i / 32) as usize;
        self.0.get(limb).is_some_and(|l| (l >> (i % 32)) & 1 == 1)
    }

    /// Low `k` bits, compared against 2^(k-1): Less / Equal / Greater.
    fn low_vs_half(&self, k: u32) -> Ordering {
        if k == 0 {
            return Ordering::Less;
        }
        if !self.bit(k - 1) {
            return Ordering::Less;
        }
        for i in 0..k - 1 {
            if self.bit(i) {
                return Ordering::Greater;
            }
        }
        Ordering::Equal
    }

    /// The top bits `[k, k+53)` as an integer.
    fn shr_u64(&self, k: u32) -> u64 {
        let mut out = 0u64;
        for i in (0..64).rev() {
            out <<= 1;
            if self.bit(k + i) {
                out |= 1;
            }
        }
        out
    }
}

/// `(mantissa, exponent)` with `x == mantissa * 2^exponent`, exponent >= -1074.
fn decompose(x: f64) -> (u64, i32) {
    let bits = x.abs().to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    if exp == 0 {
        (frac, -1074)
    } else {
        (frac | (1u64 << 52), exp - 1075)
    }
}

/// 2^e as a double, subnormals included.
fn pow2(e: i32) -> f64 {
    if e >= -1022 {
        f64::from_bits(((e + 1023) as u64) << 52)
    } else {
        f64::from_bits(1u64 << (e + 1074))
    }
}

/// The exact mean of `xs`, correctly rounded. Non-finite input falls back to floats.
pub fn mean(xs: &[f64]) -> f64 {
    if xs.iter().any(|x| !x.is_finite()) {
        return xs.iter().sum::<f64>() / xs.len() as f64;
    }
    let (mut pos, mut neg) = (Big::default(), Big::default());
    for &x in xs {
        if x == 0.0 {
            continue;
        }
        let (m, e) = decompose(x);
        let b = Big::from_shifted(m, (e + 1074) as u32);
        if x < 0.0 {
            neg.add(&b);
        } else {
            pos.add(&b);
        }
    }
    let (mag, negative) = match pos.cmp(&neg) {
        Ordering::Less => (neg.sub(&pos), true),
        _ => (pos.sub(&neg), false),
    };
    if mag.is_zero() {
        return 0.0;
    }
    let (q, r) = mag.divrem(xs.len() as u64);
    let n = xs.len() as u64;
    let len = q.bits();
    let k = len.saturating_sub(53);
    let mut mant = if k == 0 { q.shr_u64(0) } else { q.shr_u64(k) };
    let round_up = if k == 0 {
        match (2 * r as u128).cmp(&(n as u128)) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => mant & 1 == 1,
        }
    } else {
        match q.low_vs_half(k) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => r > 0 || mant & 1 == 1,
        }
    };
    if round_up {
        mant += 1;
    }
    let v = mant as f64 * pow2(k as i32 - 1074);
    if negative {
        -v
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_means() {
        assert_eq!(mean(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(mean(&[0.1, 0.2, 0.3]), 0.2);
        assert_eq!(mean(&[1e16, 1.0, -1e16]), 1.0 / 3.0);
        assert_eq!(mean(&[-1.5, -2.5]), -2.0);
        assert_eq!(mean(&[5e-324, 5e-324]), 5e-324);
        assert_eq!(mean(&[1e300, 1e300]), 1e300);
    }
}
