//! CPython's `random.Random` stream, frozen from the reference interpreter
//! (`tests/golden/pyrandom.json`): the first 1000 `random()` values, 400 `gauss()`
//! values and an interleaved run, for four seeds (one wider than 32 bits).

use rungbot_core::pyrandom::PyRandom;
use rungbot_core::watch::json::Json;

fn golden() -> Json {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("pyrandom.json");
    serde_json::from_str(&std::fs::read_to_string(p).expect("golden")).expect("json")
}

fn floats(j: &Json) -> Vec<f64> {
    j.items().iter().map(|x| x.num().expect("number")).collect()
}

#[test]
fn the_stream_matches_cpython_bit_for_bit() {
    let g = golden();
    for (seed, v) in g.entries() {
        let seed: u64 = seed.parse().unwrap();
        let mut r = PyRandom::new(seed);
        let got: Vec<f64> = (0..1000).map(|_| r.random()).collect();
        assert_eq!(
            got,
            floats(v.get("random").unwrap()),
            "seed {seed}: random()"
        );

        let mut r = PyRandom::new(seed);
        let mut got = Vec::new();
        for _ in 0..200 {
            got.push(r.gauss(0.0, 1.0));
            got.push(r.gauss(1.5, 0.25));
        }
        let want = floats(v.get("gauss").unwrap());
        for (i, (a, b)) in got.iter().zip(&want).enumerate() {
            // Box-Muller goes through the platform's log/cos/sin; identical on the
            // reference's platform, within an ulp elsewhere.
            let ulps = (a.to_bits() as i64 - b.to_bits() as i64).abs();
            assert!(
                a == b || (cfg!(not(target_os = "linux")) && ulps <= 2),
                "seed {seed}: gauss #{i}: {a} vs {b}"
            );
        }

        let mut r = PyRandom::new(seed);
        let got: Vec<f64> = (0..300)
            .map(|i| {
                if i % 3 != 0 {
                    r.random()
                } else {
                    r.gauss(0.0, 1.0)
                }
            })
            .collect();
        let want = floats(v.get("mixed").unwrap());
        for (i, (a, b)) in got.iter().zip(&want).enumerate() {
            let ulps = (a.to_bits() as i64 - b.to_bits() as i64).abs();
            assert!(
                a == b || (cfg!(not(target_os = "linux")) && ulps <= 2),
                "seed {seed}: mixed #{i}: {a} vs {b}"
            );
        }
    }
}
