//! fuzz_validate.rs — validasi vektor untuk port RapidFuzz (blok B2-B6).
//! Baca /tmp/w2vec/fuzz_vectors.json (dijana genvec.py, parity backend C++).
//! Jalankan: cargo test --release fuzz_vector_validation -- --nocapture

use serde_json::Value;
use std::fs;

fn dp(s: &str) -> String {
    crate::fuzz::default_process(s)
}

/// Nama scorer JSON -> fungsi Rust (10 scorer vektor).
fn scorer_fn(name: &str) -> fn(&str, &str, f64) -> f64 {
    match name {
        "ratio" => crate::fuzz::ratio,
        "partial_ratio" => crate::fuzz::partial_ratio,
        "token_sort_ratio" => crate::fuzz::token_sort_ratio,
        "token_set_ratio" => crate::fuzz::token_set_ratio,
        "token_ratio" => crate::fuzz::token_ratio,
        "partial_token_sort_ratio" => crate::fuzz::partial_token_sort_ratio,
        "partial_token_set_ratio" => crate::fuzz::partial_token_set_ratio,
        "partial_token_ratio" => crate::fuzz::partial_token_ratio,
        "WRatio" => crate::fuzz::w_ratio,
        "QRatio" => crate::fuzz::q_ratio,
        other => panic!("scorer tidak dikenali: {other}"),
    }
}

const SCORERS: [&str; 10] = [
    "ratio",
    "partial_ratio",
    "token_sort_ratio",
    "token_set_ratio",
    "token_ratio",
    "partial_token_sort_ratio",
    "partial_token_set_ratio",
    "partial_token_ratio",
    "WRatio",
    "QRatio",
];

#[test]
fn fuzz_vector_validation() {
    let path =
        std::env::var("FUZZ_VECTORS").unwrap_or_else(|_| "/tmp/w2vec/fuzz_vectors.json".to_string());
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    let v: Value = serde_json::from_str(&raw).expect("vectors: bad json");

    // ---- Pasangan (B2-B5): 3061 pasang x 10 scorer; kedua-dua input di-dp ----
    let pairs = v["pairs"].as_array().expect("pairs array");
    let mut n = [0usize; 10];
    let mut mism = [0usize; 10];
    let mut max_diff = [0f64; 10];
    let mut dumps: Vec<String> = Vec::new();

    for p in pairs {
        let a_raw = p["a"].as_str().unwrap_or("");
        let b_raw = p["b"].as_str().unwrap_or("");
        let a = dp(a_raw);
        let b = dp(b_raw);
        let cutoff = 0.0f64; // genvec.py: score_cutoff None -> 0

        for (i, name) in SCORERS.iter().enumerate() {
            let expected = p["scores"][*name].as_f64().unwrap_or(f64::NAN);
            let actual = scorer_fn(name)(&a, &b, cutoff);
            n[i] += 1;
            let d = (actual - expected).abs();
            if d > max_diff[i] {
                max_diff[i] = d;
            }
            if !(d <= 1e-6) {
                mism[i] += 1;
                if dumps.len() < 12 {
                    dumps.push(format!(
                        "[{name}] a={a_raw:?}->{a:?} b={b_raw:?}->{b:?} cutoff={cutoff:?} expected={expected:?} actual={actual:?} diff={d:e}"
                    ));
                }
            }
        }
    }

    // ---- default_process (B3): kesamaan string tepat untuk semua entri ----
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

    // ---- extract (B6): 140 kes; padanan PERSIS choice + skor + indeks + urutan ----
    let extract = v["extract"].as_array().expect("extract array");
    let mut ex_cases = 0usize;
    let mut ex_rows = 0usize;
    let mut ex_mism = 0usize;
    let mut ex_dumps: Vec<String> = Vec::new();

    for (ci, e) in extract.iter().enumerate() {
        let q_raw = e["q"].as_str().unwrap_or("");
        let name = e["scorer"].as_str().unwrap_or("");
        let cutoff = e["score_cutoff"].as_f64().unwrap_or(0.0);
        let limit = e["limit"].as_u64().map(|l| l as usize);

        let q = dp(q_raw);
        let choice_arr = e["choices"].as_array().expect("choices array");
        let candidates: Vec<crate::process::Candidate<'_>> = choice_arr
            .iter()
            .map(|c| {
                let orig = c.as_str().unwrap_or("");
                crate::process::Candidate {
                    original: orig,
                    processed: dp(orig),
                }
            })
            .collect();

        let got = crate::process::extract(&q, &candidates, scorer_fn(name), cutoff, limit);
        let expected = e["res"].as_array().expect("res array");

        ex_cases += 1;
        if got.len() != expected.len() {
            ex_mism += 1;
            if ex_dumps.len() < 12 {
                ex_dumps.push(format!(
                    "[extract #{ci} {name}] q={q_raw:?} cutoff={cutoff:?} limit={limit:?} expected_rows={} actual_rows={}",
                    expected.len(),
                    got.len()
                ));
            }
        }

        for (ri, row) in expected.iter().enumerate() {
            let exp_choice = row[0].as_str().unwrap_or("");
            let exp_score = row[1].as_f64().unwrap_or(f64::NAN);
            let exp_idx = row[2].as_u64().unwrap_or(u64::MAX) as usize;
            ex_rows += 1;
            let ok = match got.get(ri) {
                Some(g) => {
                    g.choice == exp_choice
                        && g.index == exp_idx
                        && (g.score - exp_score).abs() <= 1e-9
                }
                None => false,
            };
            if !ok {
                ex_mism += 1;
                if ex_dumps.len() < 12 {
                    let g = got.get(ri);
                    ex_dumps.push(format!(
                        "[extract #{ci} {name} row {ri}] q={q_raw:?} cutoff={cutoff:?} limit={limit:?} expected={:?} actual={:?}",
                        (exp_choice, exp_score, exp_idx),
                        g.map(|g| (g.choice.as_str(), g.score, g.index))
                    ));
                }
            }
        }
    }

    // ---- Pin pasangan kosong (spesifikasi vektor) + sampel literal 1/7 ----
    let all10: [(&str, fn(&str, &str, f64) -> f64); 10] = [
        ("ratio", crate::fuzz::ratio),
        ("partial_ratio", crate::fuzz::partial_ratio),
        ("token_sort_ratio", crate::fuzz::token_sort_ratio),
        ("token_set_ratio", crate::fuzz::token_set_ratio),
        ("token_ratio", crate::fuzz::token_ratio),
        ("partial_token_sort_ratio", crate::fuzz::partial_token_sort_ratio),
        ("partial_token_set_ratio", crate::fuzz::partial_token_set_ratio),
        ("partial_token_ratio", crate::fuzz::partial_token_ratio),
        ("WRatio", crate::fuzz::w_ratio),
        ("QRatio", crate::fuzz::q_ratio),
    ];
    let both_empty_100 = [
        "ratio",
        "partial_ratio",
        "token_sort_ratio",
        "token_ratio",
        "partial_token_sort_ratio",
        "partial_token_ratio",
    ];
    for (name, f) in all10 {
        assert_eq!(f("", "x", 0.0), 0.0, "satu-sisi-kosong {name}");
        assert_eq!(f("x", "", 0.0), 0.0, "satu-sisi-kosong(rev) {name}");
        let expect = if both_empty_100.contains(&name) {
            100.0
        } else {
            0.0
        };
        assert_eq!(f("", "", 0.0), expect, "dua-sisi-kosong {name}");
    }
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
    for (i, name) in SCORERS.iter().enumerate() {
        println!(
            "{name}: cases={} mismatches={} max_abs_diff={:e}",
            n[i], mism[i], max_diff[i]
        );
    }
    println!("default_process probes: cases={dp_n} mismatches={dp_mism}");
    println!("extract: cases={ex_cases} rows={ex_rows} mismatches={ex_mism}");
    for d in &dumps {
        println!("MISMATCH {d}");
    }
    for d in &dp_dumps {
        println!("DP MISMATCH {d}");
    }
    for d in &ex_dumps {
        println!("EXTRACT MISMATCH {d}");
    }

    for (i, name) in SCORERS.iter().enumerate() {
        assert_eq!(mism[i], 0, "{name} mismatches");
    }
    assert_eq!(dp_mism, 0, "default_process mismatches");
    assert_eq!(ex_mism, 0, "extract mismatches");
    println!("ALL PASS");
}
