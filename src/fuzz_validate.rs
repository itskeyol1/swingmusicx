//! fuzz_validate.rs — validasi vektor untuk port RapidFuzz (blok B2-B4).
//! Baca /tmp/w2vec/fuzz_vectors.json (dijana genvec.py, parity backend C++).
//! Jalankan: cargo test --release fuzz_vector_validation -- --nocapture

use serde_json::Value;
use std::fs;

fn dp(s: &str) -> String {
    crate::fuzz::default_process(s)
}

#[test]
fn fuzz_vector_validation() {
    let path =
        std::env::var("FUZZ_VECTORS").unwrap_or_else(|_| "/tmp/w2vec/fuzz_vectors.json".to_string());
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    let v: Value = serde_json::from_str(&raw).expect("vectors: bad json");

    let pairs = v["pairs"].as_array().expect("pairs array");
    let scorers = ["ratio", "partial_ratio"];
    let mut n = [0usize; 2];
    let mut mism = [0usize; 2];
    let mut max_diff = [0f64; 2];
    let mut dumps: Vec<String> = Vec::new();

    for p in pairs {
        let a_raw = p["a"].as_str().unwrap_or("");
        let b_raw = p["b"].as_str().unwrap_or("");
        let a = dp(a_raw);
        let b = dp(b_raw);
        let cutoff = 0.0f64; // genvec.py: score_cutoff None -> 0

        for (i, name) in scorers.iter().enumerate() {
            let expected = p["scores"][*name].as_f64().unwrap_or(f64::NAN);
            let actual = if i == 0 {
                crate::fuzz::ratio(&a, &b, cutoff)
            } else {
                crate::fuzz::partial_ratio(&a, &b, cutoff)
            };
            n[i] += 1;
            let d = (actual - expected).abs();
            if d > max_diff[i] {
                max_diff[i] = d;
            }
            if !(d <= 1e-6) {
                mism[i] += 1;
                if dumps.len() < 10 {
                    dumps.push(format!(
                        "[{name}] a={a_raw:?}->{a:?} b={b_raw:?}->{b:?} cutoff={cutoff:?} expected={expected:?} actual={actual:?} diff={d:e}"
                    ));
                }
            }
        }
    }

    // Probe default_process (B3): kesamaan string tepat untuk semua entri.
    let mut dp_n = 0usize;
    let mut dp_mism = 0usize;
    let mut dp_dumps: Vec<String> = Vec::new();
    if let Some(arr) = v["default_process"].as_array() {
        for e in arr {
            let input = e["in"].as_str().unwrap_or("");
            let expect = e["out"].as_str().unwrap_or("");
            let got = dp(input);
            dp_n += 1;
            if got != expect {
                dp_mism += 1;
                if dp_dumps.len() < 10 {
                    dp_dumps.push(format!("in={input:?} expected={expect:?} actual={got:?}"));
                }
            }
        }
    }

    // Pin 8 (pasangan kosong) + sampel literal 1/7 yang telah disahkan.
    assert_eq!(crate::fuzz::ratio("", "", 0.0), 100.0, "empty/empty ratio");
    assert_eq!(
        crate::fuzz::partial_ratio("", "", 0.0),
        100.0,
        "empty/empty partial_ratio"
    );
    assert_eq!(crate::fuzz::ratio("", "x", 0.0), 0.0, "one-side-empty ratio");
    assert_eq!(crate::fuzz::ratio("x", "", 0.0), 0.0, "one-side-empty ratio2");
    assert_eq!(
        crate::fuzz::partial_ratio("", "x", 0.0),
        0.0,
        "one-side-empty partial"
    );
    assert_eq!(
        crate::fuzz::ratio("motorhead", "naïve", 0.0),
        14.28571428571429,
        "sample 1/7 exact"
    );
    assert_eq!(
        crate::fuzz::partial_ratio("motorhead", "naïve", 0.0),
        28.57142857142857,
        "sample motorhead partial"
    );

    println!("== fuzz vector validation ==");
    println!("pairs: {}", pairs.len());
    for (i, name) in scorers.iter().enumerate() {
        println!(
            "{name}: cases={} mismatches={} max_abs_diff={:e}",
            n[i], mism[i], max_diff[i]
        );
    }
    println!("default_process probes: cases={dp_n} mismatches={dp_mism}");
    for d in &dumps {
        println!("MISMATCH {d}");
    }
    for d in &dp_dumps {
        println!("DP MISMATCH {d}");
    }

    assert_eq!(mism[0], 0, "ratio mismatches");
    assert_eq!(mism[1], 0, "partial_ratio mismatches");
    assert_eq!(dp_mism, 0, "default_process mismatches");
    println!("ALL PASS");
}
