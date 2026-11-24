// fuzz.rs — port bit-exact RapidFuzz 3.14.5 (C++ backend) untuk SwingMusix.
// Ground truth: vektor oracle fuzz_vectors.json (dijana oleh C++ AVX2 build).
// Semua formula mengikut op-order C++ f64 (~ ms f32). Jangan ubah tanpa regen vektor.
//
// Bahagian fail (ditulis berperingkat):
//   B1: utils asas — is_space, sorted_split, SplittedSentenceView (Rust: Vec<&str> — per code point)
//   B2: LCSseq (bitvector Hyyrö) + Indel distance/similarity/norm
//   B3: default_process (jadual unicodetype)
//   B4: ratio + partial_ratio
//   B5: token_sort / token_set / token_ratio / partial_token_* / WRatio / QRatio
//   B6: extract (generic + f64 + shim)

use crate::fuzz_tables;
use std::collections::HashSet;

// ===========================================================================
// B1 — split & space semantics (details/common_impl.hpp + SplittedSentenceView.hpp)
// ===========================================================================

/// CharT 32-bit: is_space = senarai tetap (common_impl.hpp is_space_impl tag 0).
#[inline]
pub fn is_space_cp(cp: u32) -> bool {
    matches!(
        cp,
        0x0009
            | 0x000A
            | 0x000B
            | 0x000C
            | 0x000D
            | 0x001C
            | 0x001D
            | 0x001E
            | 0x001F
            | 0x0020
            | 0x0085
            | 0x00A0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
    )
}

/// split mengikut code point (bukan byte); padan std::find_if(is_space<CharT>) C++.
/// Melangkau token kosong; bukan memangkas ' ' sahaja.
pub fn split_by_space(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if is_space_cp(c as u32) {
            if let Some(st) = start.take() {
                out.push(&s[st..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

// ===========================================================================
// B2 — LCSseq + Indel (LCSseq_impl.hpp / Indel_impl.hpp)
// Plain-DP cores; hanya peraturan RETURN-VALUE cutoff C++ direplikasi (panjang
// LCS adalah unik — algoritma betul mana pun memberi skor identik).
// ===========================================================================

/// Vektor code point (semantik unicode: Rust char == code point C++).
fn cps(s: &str) -> Vec<u32> {
    s.chars().map(|c| c as u32).collect()
}

/// LCS length — DP dua baris.
fn lcs_length(a: &[u32], b: &[u32]) -> usize {
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 {
        return 0;
    }
    let mut prev = vec![0usize; m + 1];
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        for j in 1..=m {
            cur[j] = if a[i - 1] == b[j - 1] {
                prev[j - 1] + 1
            } else {
                prev[j].max(cur[j - 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
        for v in cur.iter_mut() {
            *v = 0;
        }
    }
    prev[m]
}

/// LCSseq similarity (code point) dengan guard cut-off C++ yang tepat:
/// if (score_cutoff > len1 || score_cutoff > len2) return 0;
/// return (sim >= score_cutoff) ? sim : 0;
fn lcs_seq_similarity_cp(a: &[u32], b: &[u32], score_cutoff: usize) -> usize {
    if score_cutoff > a.len() || score_cutoff > b.len() {
        return 0;
    }
    let sim = lcs_length(a, b);
    if sim >= score_cutoff {
        sim
    } else {
        0
    }
}

/// LCSseq similarity (code point) — exact (cutoff 0).
pub fn lcs_seq_similarity(s1: &str, s2: &str) -> usize {
    lcs_seq_similarity_cp(&cps(s1), &cps(s2), 0)
}

/// Indel distance (Indel_impl.hpp): dist = maximum - 2*lcs_sim dengan
/// lcs_cutoff = (maximum/2 >= score_cutoff) ? maximum/2 - score_cutoff : 0;
/// return dist <= score_cutoff ? dist : score_cutoff + 1.
fn indel_distance_cp(a: &[u32], b: &[u32], score_cutoff: usize) -> usize {
    let maximum = a.len() + b.len();
    let lcs_cutoff = if maximum / 2 >= score_cutoff {
        maximum / 2 - score_cutoff
    } else {
        0
    };
    let lcs_sim = lcs_seq_similarity_cp(a, b, lcs_cutoff);
    let dist = maximum - 2 * lcs_sim;
    if dist <= score_cutoff {
        dist
    } else {
        score_cutoff + 1
    }
}

/// Indel distance (string).
pub fn indel_distance(s1: &str, s2: &str, score_cutoff: usize) -> usize {
    indel_distance_cp(&cps(s1), &cps(s2), score_cutoff)
}

/// NormSim_to_NormDist (common.hpp L50-53): min(1.0, 1.0 - x + 0.00001).
#[inline]
pub fn norm_sim_to_norm_dist(x: f64) -> f64 {
    (1.0 - x + 0.00001).min(1.0)
}

/// Indel normalized distance: cutoff_distance = ceil(maximum * score_cutoff);
/// norm_dist = dist/maximum (0 apabila maximum==0);
/// return norm_dist <= score_cutoff ? norm_dist : 1.0.
fn indel_normalized_distance_cp(a: &[u32], b: &[u32], score_cutoff: f64) -> f64 {
    let maximum = a.len() + b.len();
    let cutoff_distance = (maximum as f64 * score_cutoff).ceil() as usize;
    let dist = indel_distance_cp(a, b, cutoff_distance);
    let norm_dist = if maximum > 0 {
        dist as f64 / maximum as f64
    } else {
        0.0
    };
    if norm_dist <= score_cutoff {
        norm_dist
    } else {
        1.0
    }
}

/// Indel normalized distance (string).
pub fn indel_normalized_distance(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    indel_normalized_distance_cp(&cps(s1), &cps(s2), score_cutoff)
}

/// Indel normalized similarity: cutoff via NormSim_to_NormDist; 1.0 - norm_dist
/// (di bawah cutoff norm_dist == 1.0 → 0.0).
fn indel_normalized_similarity_cp(a: &[u32], b: &[u32], score_cutoff: f64) -> f64 {
    let cutoff = norm_sim_to_norm_dist(score_cutoff);
    1.0 - indel_normalized_distance_cp(a, b, cutoff)
}

/// Indel normalized similarity (string).
pub fn indel_normalized_similarity(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    indel_normalized_similarity_cp(&cps(s1), &cps(s2), score_cutoff)
}

// ===========================================================================
// B3 — default_process (utils.cpp UnicodeDefaultProcess + wrapper)
// ===========================================================================

// utils.cpp L3-17: flag masks (CPython unicodetype_db.h).
#[allow(dead_code)]
mod flag {
    pub const ALPHA: u16 = 0x01;
    pub const DECIMAL: u16 = 0x02;
    pub const DIGIT: u16 = 0x04;
    pub const LOWER: u16 = 0x08;
    pub const LINEBREAK: u16 = 0x10;
    pub const SPACE: u16 = 0x20;
    pub const TITLE: u16 = 0x40;
    pub const UPPER: u16 = 0x80;
    pub const XID_START: u16 = 0x100;
    pub const XID_CONTINUE: u16 = 0x200;
    pub const NUMERIC: u16 = 0x800;
    pub const EXTENDED_CASE_MASK: u16 = 0x4000;
}
use flag::*;

/// utils.cpp: is_alnum = ALPHA | DECIMAL | DIGIT | NUMERIC.
#[inline]
fn is_alnum_flags(flags: u16) -> bool {
    flags & (ALPHA | DECIMAL | DIGIT | NUMERIC) != 0
}

/// unicodetype_db gettyperecord: cp >= 0x110000 -> index 0; jika tidak
/// INDEX1[cp >> SHIFT] -> INDEX2[(i1 << SHIFT) + (cp & mask)].
#[inline]
fn get_typerecord(cp: u32) -> &'static fuzz_tables::TypeRec {
    let shift = fuzz_tables::SHIFT;
    let index: usize = if cp >= 0x110000 {
        0
    } else {
        let i1 = fuzz_tables::INDEX1[(cp >> shift) as usize] as usize;
        let lo = (cp & ((1u32 << shift) - 1)) as usize;
        fuzz_tables::INDEX2[(i1 << shift) + lo] as usize
    };
    &fuzz_tables::TYPE_RECORDS[index]
}

/// utils.cpp UnicodeDefaultProcess: non-alnum -> ' '; EXTENDED_CASE -> jadual
/// extended; selain itu lower-mapped (ch + record.lower, aritmetik wrap seperti C++).
fn unicode_default_process(ch: u32) -> u32 {
    let ctype = get_typerecord(ch);
    if !is_alnum_flags(ctype.flags) {
        return 0x20;
    }
    if ctype.flags & EXTENDED_CASE_MASK != 0 {
        let index = (ctype.lower & 0xFFFF) as usize;
        return fuzz_tables::EXTENDED_CASE[index];
    }
    ch.wrapping_add(ctype.lower as u32)
}

/// Pemetaan per code point: cp < 256 guna jadual ASCII (utils.hpp
/// extended_ascii_mapping); selebihnya UnicodeDefaultProcess.
#[inline]
fn map_cp(cp: u32) -> u32 {
    if cp < 256 {
        fuzz_tables::ASCII_MAP[cp as usize] as u32
    } else {
        unicode_default_process(cp)
    }
}

/// default_process wrapper: petakan setiap code point, kemudian trim ' ' di
/// kedua-dua hujung (non-alnum jadi ruang, jadi "—" -> "").
pub fn default_process(s: &str) -> String {
    let mapped: Vec<u32> = s.chars().map(|c| map_cp(c as u32)).collect();
    let mut start = 0usize;
    let mut end = mapped.len();
    while end > start && mapped[end - 1] == 0x20 {
        end -= 1;
    }
    while start < end && mapped[start] == 0x20 {
        start += 1;
    }
    mapped[start..end]
        .iter()
        .map(|&cp| char::from_u32(cp).unwrap_or('\u{FFFD}'))
        .collect()
}

// ===========================================================================
// B4 — ratio + partial_ratio (fuzz.hpp / fuzz_impl.hpp) + cached wrappers
// partial_ratio menyalin partial_ratio_impl + partial_ratio_alignment VERBATIM
// (window pruning + guard char-set prefix/suffix); JANGAN ganti dengan
// penghampiran — dua model lebih murah telah diprobe dan DITOLAK.
// ===========================================================================

/// fuzz_impl.hpp L29-31:
/// ratio = indel_normalized_similarity(s1, s2, score_cutoff/100) * 100.
pub fn ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    indel_normalized_similarity(s1, s2, score_cutoff / 100.0) * 100.0
}

/// CachedIndel (fuzz.hpp): distance(subseq) = indel_distance(s1, subseq) TANPA
/// cutoff mengehadkan (jarak tepat); normalized_similarity(subseq, cutoff) guna
/// laluan Indel normalized generik. Cermin pembalut C++ untuk B5.
pub struct CachedIndel {
    s1_cp: Vec<u32>,
}

impl CachedIndel {
    pub fn new(s1: &str) -> Self {
        CachedIndel { s1_cp: cps(s1) }
    }

    /// Jarak indel tepat (tanpa cutoff) terhadap string tercache.
    pub fn distance(&self, subseq: &str) -> usize {
        let b = cps(subseq);
        indel_distance_cp(&self.s1_cp, &b, usize::MAX)
    }

    /// Normalized similarity (cutoff 0..1 via NormSim_to_NormDist);
    /// bawah cutoff -> 0.0.
    pub fn normalized_similarity(&self, subseq: &str, score_cutoff: f64) -> f64 {
        let b = cps(subseq);
        indel_normalized_similarity_cp(&self.s1_cp, &b, score_cutoff)
    }
}

/// CachedRatio (fuzz.hpp): similarity(sub2, cutoff_0_100) =
/// cached_indel.normalized_similarity(sub2, cutoff/100) * 100.
pub struct CachedRatio {
    cached_indel: CachedIndel,
}

impl CachedRatio {
    pub fn new(s1: &str) -> Self {
        CachedRatio {
            cached_indel: CachedIndel::new(s1),
        }
    }

    /// Ratio 0..100 terhadap string tercache; cutoff inklusif.
    pub fn similarity(&self, subseq: &str, score_cutoff: f64) -> f64 {
        let b = cps(subseq);
        cached_ratio_similarity_cp(&self.cached_indel.s1_cp, &b, score_cutoff)
    }
}

#[inline]
fn cached_ratio_similarity_cp(s1_cp: &[u32], subseq_cp: &[u32], score_cutoff: f64) -> f64 {
    indel_normalized_similarity_cp(s1_cp, subseq_cp, score_cutoff / 100.0) * 100.0
}

/// partial_ratio_alignment (fuzz_impl.hpp L196-229) bentuk skor sahaja:
/// swap supaya len1 <= len2, cutoff > 100 -> 0, kosong -> (kedua kosong)*100,
/// kemudian partial_ratio_impl; pass kedua apabila score != 100 && len1 == len2.
pub fn partial_ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    let a = cps(s1);
    let b = cps(s2);
    partial_ratio_score_cp(&a, &b, score_cutoff)
}

fn partial_ratio_score_cp(a: &[u32], b: &[u32], score_cutoff: f64) -> f64 {
    let (len1, len2) = (a.len(), b.len());
    if len1 > len2 {
        return partial_ratio_score_cp(b, a, score_cutoff);
    }
    if score_cutoff > 100.0 {
        return 0.0;
    }
    if len1 == 0 || len2 == 0 {
        return if len1 == len2 { 100.0 } else { 0.0 };
    }
    let alignment = partial_ratio_impl(a, b, score_cutoff);
    if alignment.score != 100.0 && len1 == len2 {
        let cutoff2 = if score_cutoff > alignment.score {
            score_cutoff
        } else {
            alignment.score
        };
        let alignment2 = partial_ratio_impl(b, a, cutoff2);
        if alignment2.score > alignment.score {
            return alignment2.score;
        }
    }
    alignment.score
}

/// Skor + alignment destinasi yang dipulangkan partial_ratio_impl.
#[allow(dead_code)]
struct RawAlignment {
    score: f64,
    dest_start: usize,
    dest_end: usize,
}

/// partial_ratio_impl (fuzz_impl.hpp L68-193) — disalin tepat:
/// - head: res src 0..len1, dest 0..len1; apabila len2 > len1:
///   maximum = len1*2; norm_cutoff_sim = NormSim_to_NormDist(cutoff/100);
///   cutoff_dist = ceil(maximum * norm_cutoff_sim); best_dist = MAX;
///   scores(len2-len1, MAX); windows = {(0, len2-len1-1)};
/// - gelung window mengira jarak cached-indel tepat di hujung window,
///   merekod pembaikan ketat, pulang awal 100 pada jarak 0, dan memecah
///   window selagi `min_score < cutoff_dist` (L130-137 verbatim);
/// - skor akhir: (1.0 - best_dist/maximum) * 100; `>= cutoff` -> komit;
/// - gelung prefix i=1..len1 atas s2[0..i] dengan guard subseq.back() dalam set s1;
/// - gelung suffix i=len2-len1..len2 atas s2[i..] dengan guard subseq.front();
/// - pulang awal 100 dalam kedua-dua gelung.
fn partial_ratio_impl(s1: &[u32], s2: &[u32], score_cutoff_in: f64) -> RawAlignment {
    let len1 = s1.len();
    let len2 = s2.len();
    let mut res = RawAlignment {
        score: 0.0,
        dest_start: 0,
        dest_end: len1,
    };
    let mut score_cutoff = score_cutoff_in;
    let s1_char_set: HashSet<u32> = s1.iter().copied().collect();

    if len2 > len1 {
        let maximum = len1 * 2;
        let norm_cutoff_sim = norm_sim_to_norm_dist(score_cutoff / 100.0);
        let mut cutoff_dist = (maximum as f64 * norm_cutoff_sim).ceil() as usize;
        let mut best_dist = usize::MAX;
        let mut scores: Vec<usize> = vec![usize::MAX; len2 - len1];
        let mut windows: Vec<(usize, usize)> = vec![(0, len2 - len1 - 1)];

        while !windows.is_empty() {
            let mut new_windows: Vec<(usize, usize)> = Vec::new();
            for &(wfirst, wsecond) in &windows {
                if scores[wfirst] == usize::MAX {
                    scores[wfirst] = indel_distance_cp(s1, &s2[wfirst..wfirst + len1], usize::MAX);
                }
                if scores[wfirst] < cutoff_dist {
                    cutoff_dist = scores[wfirst];
                    best_dist = scores[wfirst];
                    res.dest_start = wfirst;
                    res.dest_end = wfirst + len1;
                    if best_dist == 0 {
                        res.score = 100.0;
                        return res;
                    }
                }
                if scores[wsecond] == usize::MAX {
                    scores[wsecond] =
                        indel_distance_cp(s1, &s2[wsecond..wsecond + len1], usize::MAX);
                }
                if scores[wsecond] < cutoff_dist {
                    cutoff_dist = scores[wsecond];
                    best_dist = scores[wsecond];
                    res.dest_start = wsecond;
                    res.dest_end = wsecond + len1;
                    if best_dist == 0 {
                        res.score = 100.0;
                        return res;
                    }
                }
                let cell_diff = wsecond - wfirst;
                if cell_diff == 1 {
                    continue;
                }
                let known_edits = if scores[wfirst] > scores[wsecond] {
                    scores[wfirst] - scores[wsecond]
                } else {
                    scores[wsecond] - scores[wfirst]
                };
                let max_score_improvement = (cell_diff - known_edits / 2) / 2 * 2;
                let min_score = scores[wfirst].min(scores[wsecond]) as i64
                    - max_score_improvement as i64;
                if min_score < cutoff_dist as i64 {
                    let center = cell_diff / 2;
                    new_windows.push((wfirst, wfirst + center));
                    new_windows.push((wfirst + center, wsecond));
                }
            }
            windows = new_windows;
        }

        let mut score = 1.0 - (best_dist as f64 / maximum as f64);
        score *= 100.0;
        if score >= score_cutoff {
            score_cutoff = score;
            res.score = score;
        }
    }

    for i in 1..len1 {
        let subseq = &s2[0..i];
        if !s1_char_set.contains(&subseq[i - 1]) {
            continue;
        }
        let ls_ratio = cached_ratio_similarity_cp(s1, subseq, score_cutoff);
        if ls_ratio > res.score {
            score_cutoff = ls_ratio;
            res.score = ls_ratio;
            res.dest_start = 0;
            res.dest_end = i;
            if res.score == 100.0 {
                return res;
            }
        }
    }
    for i in (len2 - len1)..len2 {
        let subseq = &s2[i..len2];
        if !s1_char_set.contains(&subseq[0]) {
            continue;
        }
        let ls_ratio = cached_ratio_similarity_cp(s1, subseq, score_cutoff);
        if ls_ratio > res.score {
            score_cutoff = ls_ratio;
            res.score = ls_ratio;
            res.dest_start = i;
            res.dest_end = len2;
            if res.score == 100.0 {
                return res;
            }
        }
    }
    res
}

// ======== B5 token scorers (next block) ========
// ======== B5 token scorers ========

/// fuzz_impl.hpp L55-61 (fuzz_detail::norm_distance):
/// score = lensum>0 ? 100.0 - 100.0*dist/lensum : 100.0; return score >= cutoff ? score : 0.
fn norm_distance(dist: usize, lensum: usize, score_cutoff: f64) -> f64 {
    let score = if lensum > 0 {
        100.0 - 100.0 * dist as f64 / lensum as f64
    } else {
        100.0
    };
    if score >= score_cutoff {
        score
    } else {
        0.0
    }
}

/// fuzz_impl.hpp L63-66 (fuzz_detail::score_cutoff_to_distance):
/// (size_t)ceil((double)lensum * (1.0 - score_cutoff / 100)).
fn score_cutoff_to_distance(score_cutoff: f64, lensum: usize) -> usize {
    (lensum as f64 * (1.0 - score_cutoff / 100.0)).ceil() as usize
}

/// ratio (fuzz_impl.hpp L29-31) atas vektor code point:
/// indel_normalized_similarity(s1, s2, score_cutoff/100) * 100.
fn ratio_cp(a: &[u32], b: &[u32], score_cutoff: f64) -> f64 {
    indel_normalized_similarity_cp(a, b, score_cutoff / 100.0) * 100.0
}

/// sorted_split (common_impl.hpp L161-181): pecah ikut is_space (32-bit), susun
/// menaik (lexicographic code point == byte order untuk UTF-8), TIDAK dedupe.
fn sorted_split(s: &str) -> Vec<&str> {
    let mut splitted = split_by_space(s);
    splitted.sort();
    splitted
}

/// SplittedSentenceView::join (SplittedSentenceView.hpp L70-85): cantum perkataan
/// asal dengan satu 0x20 antara setiap pasangan; senarai kosong -> kosong.
fn join_words(words: &[&str]) -> Vec<u32> {
    if words.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<u32> = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push(0x20);
        }
        out.extend(w.chars().map(|c| c as u32));
    }
    out
}

/// SplittedSentenceView::size/length (L57-68): 0 jika kosong; (n-1) + jumlah
/// panjang perkataan (dalam code point).
fn words_length(words: &[&str]) -> usize {
    if words.is_empty() {
        return 0;
    }
    let mut result = words.len() - 1;
    for w in words {
        result += w.chars().count();
    }
    result
}

/// set_decomposition (common_impl.hpp L12-35) — diambil mengikut nilai, jadi
/// dedupe tempatan sahaja; intersection mengikut susunan a; padanan pertama
/// dalam b dikeluarkan daripada difference_ba.
struct Decomposition<'a> {
    difference_ab: Vec<&'a str>,
    difference_ba: Vec<&'a str>,
    intersection: Vec<&'a str>,
}

fn set_decomposition<'a>(a: &[&'a str], b: &[&'a str]) -> Decomposition<'a> {
    let mut a_words: Vec<&'a str> = a.to_vec();
    a_words.dedup();
    let mut difference_ba: Vec<&'a str> = b.to_vec();
    difference_ba.dedup();

    let mut intersection: Vec<&'a str> = Vec::new();
    let mut difference_ab: Vec<&'a str> = Vec::new();

    for &current_a in &a_words {
        if let Some(pos) = difference_ba.iter().position(|&x| x == current_a) {
            difference_ba.remove(pos);
            intersection.push(current_a);
        } else {
            difference_ab.push(current_a);
        }
    }

    Decomposition {
        difference_ab,
        difference_ba,
        intersection,
    }
}

/// fuzz_impl.hpp L294-311: token_sort_ratio = ratio(sorted_split(s1).join(),
/// sorted_split(s2).join(), score_cutoff); cutoff > 100 -> 0.
pub fn token_sort_ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    if score_cutoff > 100.0 {
        return 0.0;
    }
    let j1 = join_words(&sorted_split(s1));
    let j2 = join_words(&sorted_split(s2));
    ratio_cp(&j1, &j2, score_cutoff)
}

/// fuzz_impl.hpp L330-349: partial_token_sort_ratio (varian partial_ratio).
pub fn partial_token_sort_ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    if score_cutoff > 100.0 {
        return 0.0;
    }
    let j1 = join_words(&sorted_split(s1));
    let j2 = join_words(&sorted_split(s2));
    partial_ratio_score_cp(&j1, &j2, score_cutoff)
}

/// fuzz_impl.hpp L368-420 (fuzz_detail + wrapper L423-437): token_set_ratio.
pub fn token_set_ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    if score_cutoff > 100.0 {
        return 0.0;
    }
    let ta = sorted_split(s1);
    let tb = sorted_split(s2);
    token_set_ratio_tokens(&ta, &tb, score_cutoff)
}

/// Inti token_set_ratio (L372-420) atas token yang telah disusun.
fn token_set_ratio_tokens(tokens_a: &[&str], tokens_b: &[&str], score_cutoff: f64) -> f64 {
    // FuzzyWuzzy mengembalikan 0 bagi input kosong.
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return 0.0;
    }

    let decomposition = set_decomposition(tokens_a, tokens_b);
    let intersect = &decomposition.intersection;
    let diff_ab = &decomposition.difference_ab;
    let diff_ba = &decomposition.difference_ba;

    // satu ayat terkandung dalam yang lain
    if !intersect.is_empty() && (diff_ab.is_empty() || diff_ba.is_empty()) {
        return 100.0;
    }

    let diff_ab_joined = join_words(diff_ab);
    let diff_ba_joined = join_words(diff_ba);

    let ab_len = diff_ab_joined.len();
    let ba_len = diff_ba_joined.len();
    let sect_len = words_length(intersect);

    // panjang rentetan sect+ab <-> sect dan sect+ba <-> sect
    let sect_ab_len = sect_len + (sect_len > 0) as usize + ab_len;
    let sect_ba_len = sect_len + (sect_len > 0) as usize + ba_len;

    let mut result = 0.0;
    let cutoff_distance = score_cutoff_to_distance(score_cutoff, sect_ab_len + sect_ba_len);
    let dist = indel_distance_cp(&diff_ab_joined, &diff_ba_joined, cutoff_distance);
    if dist <= cutoff_distance {
        result = norm_distance(dist, sect_ab_len + sect_ba_len, score_cutoff);
    }

    // keluar awal kerana nisbah lain adalah 0
    if sect_len == 0 {
        return result;
    }

    // jarak levenshtein sect+ab <-> sect dan sect+ba <-> sect; hanya sect serupa,
    // jadi jarak boleh dikira daripada beza panjang
    let sect_ab_dist = (sect_len > 0) as usize + ab_len;
    let sect_ab_ratio = norm_distance(sect_ab_dist, sect_len + sect_ab_len, score_cutoff);

    let sect_ba_dist = (sect_len > 0) as usize + ba_len;
    let sect_ba_ratio = norm_distance(sect_ba_dist, sect_len + sect_ba_len, score_cutoff);

    result.max(sect_ab_ratio).max(sect_ba_ratio)
}

/// fuzz_impl.hpp L456-495: partial_token_set_ratio.
pub fn partial_token_set_ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    if score_cutoff > 100.0 {
        return 0.0;
    }
    let ta = sorted_split(s1);
    let tb = sorted_split(s2);
    partial_token_set_ratio_tokens(&ta, &tb, score_cutoff)
}

/// Inti partial_token_set_ratio (L460-478).
fn partial_token_set_ratio_tokens(tokens_a: &[&str], tokens_b: &[&str], score_cutoff: f64) -> f64 {
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return 0.0;
    }

    let decomposition = set_decomposition(tokens_a, tokens_b);

    // keluar awal apabila ada perkataan sama dalam kedua-dua jujukan
    if !decomposition.intersection.is_empty() {
        return 100.0;
    }

    let jab = join_words(&decomposition.difference_ab);
    let jba = join_words(&decomposition.difference_ba);
    partial_ratio_score_cp(&jab, &jba, score_cutoff)
}

/// fuzz_impl.hpp L514-564: token_ratio (versi tidak cache).
pub fn token_ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    if score_cutoff > 100.0 {
        return 0.0;
    }

    let tokens_a = sorted_split(s1);
    let tokens_b = sorted_split(s2);

    let decomposition = set_decomposition(&tokens_a, &tokens_b);
    let intersect = &decomposition.intersection;
    let diff_ab = &decomposition.difference_ab;
    let diff_ba = &decomposition.difference_ba;

    if !intersect.is_empty() && (diff_ab.is_empty() || diff_ba.is_empty()) {
        return 100.0;
    }

    let diff_ab_joined = join_words(diff_ab);
    let diff_ba_joined = join_words(diff_ba);

    let ab_len = diff_ab_joined.len();
    let ba_len = diff_ba_joined.len();
    let sect_len = words_length(intersect);

    // NOTA: dalam C++, tokens_a/tokens_b ASAL (tanpa dedupe) digunakan di sini —
    // set_decomposition menerima salinan mengikut nilai.
    let mut result = ratio_cp(&join_words(&tokens_a), &join_words(&tokens_b), score_cutoff);

    // panjang rentetan sect+ab <-> sect dan sect+ba <-> sect
    let sect_ab_len = sect_len + (sect_len > 0) as usize + ab_len;
    let sect_ba_len = sect_len + (sect_len > 0) as usize + ba_len;

    let cutoff_distance = score_cutoff_to_distance(score_cutoff, sect_ab_len + sect_ba_len);
    let dist = indel_distance_cp(&diff_ab_joined, &diff_ba_joined, cutoff_distance);
    if dist <= cutoff_distance {
        let nd = norm_distance(dist, sect_ab_len + sect_ba_len, score_cutoff);
        if nd > result {
            result = nd;
        }
    }

    // keluar awal kerana nisbah lain adalah 0
    if sect_len == 0 {
        return result;
    }

    // jarak levenshtein sect+ab <-> sect dan sect+ba <-> sect
    let sect_ab_dist = (sect_len > 0) as usize + ab_len;
    let sect_ab_ratio = norm_distance(sect_ab_dist, sect_len + sect_ab_len, score_cutoff);

    let sect_ba_dist = (sect_len > 0) as usize + ba_len;
    let sect_ba_ratio = norm_distance(sect_ba_dist, sect_len + sect_ba_len, score_cutoff);

    result.max(sect_ab_ratio).max(sect_ba_ratio)
}

/// fuzz_impl.hpp L700-737: partial_token_ratio (versi tidak cache).
pub fn partial_token_ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    if score_cutoff > 100.0 {
        return 0.0;
    }

    let tokens_a = sorted_split(s1);
    let tokens_b = sorted_split(s2);

    let decomposition = set_decomposition(&tokens_a, &tokens_b);

    // keluar awal apabila ada perkataan sama dalam kedua-dua jujukan
    if !decomposition.intersection.is_empty() {
        return 100.0;
    }

    let diff_ab = &decomposition.difference_ab;
    let diff_ba = &decomposition.difference_ba;

    // word_count ASAL berbanding word_count dedupe (diff_*) — penting.
    let result = partial_ratio_score_cp(
        &join_words(&tokens_a),
        &join_words(&tokens_b),
        score_cutoff,
    );

    // jangan kira partial_ratio yang sama dua kali
    if tokens_a.len() == diff_ab.len() && tokens_b.len() == diff_ba.len() {
        return result;
    }

    let score_cutoff = if score_cutoff > result { score_cutoff } else { result };
    let second = partial_ratio_score_cp(&join_words(diff_ab), &join_words(diff_ba), score_cutoff);
    result.max(second)
}

/// fuzz_impl.hpp L785-829: WRatio (tanpa cache).
pub fn w_ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    if score_cutoff > 100.0 {
        return 0.0;
    }

    const UNBASE_SCALE: f64 = 0.95;

    let len1 = cps(s1).len();
    let len2 = cps(s2).len();

    // FuzzyWuzzy mengembalikan 0 bagi input kosong.
    if len1 == 0 || len2 == 0 {
        return 0.0;
    }

    let len_ratio = if len1 > len2 {
        len1 as f64 / len2 as f64
    } else {
        len2 as f64 / len1 as f64
    };

    let mut end_ratio = ratio(s1, s2, score_cutoff);

    if len_ratio < 1.5 {
        let score_cutoff = score_cutoff.max(end_ratio) / UNBASE_SCALE;
        return end_ratio.max(token_ratio(s1, s2, score_cutoff) * UNBASE_SCALE);
    }

    let partial_scale = if len_ratio <= 8.0 { 0.9 } else { 0.6 };

    let score_cutoff = score_cutoff.max(end_ratio) / partial_scale;
    end_ratio = end_ratio.max(partial_ratio(s1, s2, score_cutoff) * partial_scale);

    let score_cutoff = score_cutoff.max(end_ratio) / UNBASE_SCALE;
    end_ratio.max(partial_token_ratio(s1, s2, score_cutoff) * UNBASE_SCALE * partial_scale)
}

/// fuzz_impl.hpp L887-909: QRatio — tiada early-out cutoff>100 (berbeza daripada
/// yang lain); hanya semakan panjang kosong, kemudian ratio.
pub fn q_ratio(s1: &str, s2: &str, score_cutoff: f64) -> f64 {
    let len1 = cps(s1).len();
    let len2 = cps(s2).len();

    // FuzzyWuzzy mengembalikan 0 bagi input kosong.
    if len1 == 0 || len2 == 0 {
        return 0.0;
    }

    ratio(s1, s2, score_cutoff)
}
