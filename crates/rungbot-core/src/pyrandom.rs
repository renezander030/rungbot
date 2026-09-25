//! CPython's `random.Random`, reproduced bit for bit.
//!
//! The dashboard forecast is a Monte Carlo seeded with a fixed number, so its percentiles
//! are the same on every run. The numbers it prints were first produced by CPython's
//! Mersenne Twister, and the port has to draw the identical stream: the same seeding
//! (`init_by_array` over the seed's 32-bit words), the same 53-bit `random()`, and the
//! same Box-Muller `gauss()` with its cached second value.
//!
//! Deterministic by construction: a seed in, a fixed sequence out. No entropy source.

const N: usize = 624;
const M: usize = 397;
const MATRIX_A: u32 = 0x9908_b0df;
const UPPER: u32 = 0x8000_0000;
const LOWER: u32 = 0x7fff_ffff;

/// `random.Random(seed)`.
#[derive(Clone)]
pub struct PyRandom {
    mt: [u32; N],
    mti: usize,
    gauss_next: Option<f64>,
}

impl core::fmt::Debug for PyRandom {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PyRandom").field("mti", &self.mti).finish()
    }
}

impl PyRandom {
    /// `random.Random(seed)` for a non-negative integer seed.
    pub fn new(seed: u64) -> PyRandom {
        let mut r = PyRandom {
            mt: [0; N],
            mti: N + 1,
            gauss_next: None,
        };
        r.seed(seed);
        r
    }

    /// `seed(n)`: the seed's absolute value, split into 32-bit words, least significant
    /// first (`0` is the single word `0`).
    pub fn seed(&mut self, seed: u64) {
        let lo = seed as u32;
        let hi = (seed >> 32) as u32;
        let key: Vec<u32> = if hi == 0 { vec![lo] } else { vec![lo, hi] };
        self.init_by_array(&key);
        self.gauss_next = None;
    }

    fn init_genrand(&mut self, s: u32) {
        self.mt[0] = s;
        for i in 1..N {
            let prev = self.mt[i - 1];
            self.mt[i] = 1_812_433_253u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(i as u32);
        }
        self.mti = N;
    }

    fn init_by_array(&mut self, key: &[u32]) {
        self.init_genrand(19_650_218);
        let (mut i, mut j) = (1usize, 0usize);
        let mut k = N.max(key.len());
        while k > 0 {
            let prev = self.mt[i - 1];
            self.mt[i] = (self.mt[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                self.mt[0] = self.mt[N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
            k -= 1;
        }
        k = N - 1;
        while k > 0 {
            let prev = self.mt[i - 1];
            self.mt[i] = (self.mt[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_566_083_941))
                .wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                self.mt[0] = self.mt[N - 1];
                i = 1;
            }
            k -= 1;
        }
        self.mt[0] = 0x8000_0000;
    }

    /// One tempered 32-bit output (`genrand_uint32`).
    pub fn next_u32(&mut self) -> u32 {
        if self.mti >= N {
            let mag = |y: u32| if y & 1 == 0 { 0 } else { MATRIX_A };
            for kk in 0..N - M {
                let y = (self.mt[kk] & UPPER) | (self.mt[kk + 1] & LOWER);
                self.mt[kk] = self.mt[kk + M] ^ (y >> 1) ^ mag(y);
            }
            for kk in N - M..N - 1 {
                let y = (self.mt[kk] & UPPER) | (self.mt[kk + 1] & LOWER);
                self.mt[kk] = self.mt[kk + M - N] ^ (y >> 1) ^ mag(y);
            }
            let y = (self.mt[N - 1] & UPPER) | (self.mt[0] & LOWER);
            self.mt[N - 1] = self.mt[M - 1] ^ (y >> 1) ^ mag(y);
            self.mti = 0;
        }
        let mut y = self.mt[self.mti];
        self.mti += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// `random()`: a float in `[0, 1)` from 53 random bits.
    pub fn random(&mut self) -> f64 {
        let a = (self.next_u32() >> 5) as f64;
        let b = (self.next_u32() >> 6) as f64;
        (a * 67_108_864.0 + b) * (1.0 / 9_007_199_254_740_992.0)
    }

    /// `gauss(mu, sigma)`: Box-Muller, two values per pair of draws, the second cached
    /// for the next call as CPython does.
    pub fn gauss(&mut self, mu: f64, sigma: f64) -> f64 {
        let z = match self.gauss_next.take() {
            Some(z) => z,
            None => {
                let x2pi = self.random() * (2.0 * core::f64::consts::PI);
                let g2rad = (-2.0 * (1.0 - self.random()).ln()).sqrt();
                self.gauss_next = Some(x2pi.sin() * g2rad);
                x2pi.cos() * g2rad
            }
        };
        mu + z * sigma
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_stream_for_seed_42() {
        // random.Random(42).random() in CPython.
        let mut r = PyRandom::new(42);
        assert_eq!(r.random(), 0.6394267984578837);
        assert_eq!(r.random(), 0.025010755222666936);
    }
}
