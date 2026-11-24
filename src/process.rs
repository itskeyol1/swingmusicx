//! process.rs — B6: process.extract (RapidFuzz 3.14.5, backend C++) — port setia.
//!
//! Rujukan: src/rapidfuzz/process_cpp.hpp (extract_list_impl / ExtractComparison /
//! ListMatchElem) + pembalut Python process_cpp_impl.pyx.
//!
//! Modul ini sengaja TIDAK menjalankan sebarang processor: caller mesti
//! pra-memproses input (contoh: fuzz::default_process) dan menyerahkan hasilnya
//! dalam `Candidate::processed`; `Candidate::original` dipulangkan dalam hasil.

/// Calon carian: teks ASAL (untuk hasil) + teks pra-proses (untuk skor).
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    /// Teks asal — dikembalikan apa adanya sebagai `Extraction::choice`.
    pub original: &'a str,
    /// Teks selepas processor caller (mis. default_process) — dinilai oleh scorer.
    pub processed: String,
}

/// Satu padanan — rangkap tiga Python (choice, score, index):
/// `choice` = teks ASAL, `index` = indeks ASAL dalam senarai calon.
#[derive(Debug, Clone)]
pub struct Extraction {
    pub choice: String,
    pub score: f64,
    pub index: usize,
}

/// Padanan `process.extract` (Python, RapidFuzz 3.14.5):
///
/// - `query` mesti SUDAH diproses oleh caller (tiada processor di sini).
/// - setiap calon dinilai `scorer(query, processed, score_cutoff)`.
/// - calon disimpan jika `score >= score_cutoff` (lowest_score_worst;
///   process_cpp.hpp membenarkan skor 0.0 kekal bila cutoff == 0).
/// - susunan: skor menurun, seri → indeks menaik (ExtractComparison::is_first).
/// - `limit`: `None` = semua calon yang lulus; `Some(n)` = n teratas
///   (`Some(0)` → hasil kosong).
pub fn extract<F>(
    query: &str,
    candidates: &[Candidate<'_>],
    scorer: F,
    score_cutoff: f64,
    limit: Option<usize>,
) -> Vec<Extraction>
where
    F: Fn(&str, &str, f64) -> f64,
{
    let mut results: Vec<Extraction> = Vec::new();

    for (index, candidate) in candidates.iter().enumerate() {
        let score = scorer(query, &candidate.processed, score_cutoff);
        if score >= score_cutoff {
            results.push(Extraction {
                choice: candidate.original.to_string(),
                score,
                index,
            });
        }
    }

    results.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.index.cmp(&b.index))
    });

    if let Some(limit) = limit {
        results.truncate(limit);
    }

    results
}
