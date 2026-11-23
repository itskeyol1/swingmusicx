//! Port of swingmusic/utils/hashing.py — xxh3_64 hexdigest over rna()-normalized args.
//! Parity vectors verified 2026-11-17 against live CT101 (monoswing fork).

use xxhash_rust::xxh3::xxh3_64;

/// remove_non_alnum(): lowercase → trim → drop spaces → keep alnum chars.
/// Falls back to the (lowered, de-spaced) token when nothing alnum remains.
fn rna(token: &str) -> String {
    let token = token.to_lowercase().trim().replace(' ', "");
    let t: String = token.chars().filter(|c| c.is_alphanumeric()).collect();
    if t.is_empty() {
        token
    } else {
        t
    }
}

/// create_hash(*args, decode=False): concat rna(arg) for each arg, optionally
/// unidecode, then xxh3_64 hexdigest (lowercase, 16 chars).
pub fn create_hash(args: &[&str], decode: bool) -> String {
    let mut s: String = args.iter().map(|a| rna(a)).collect();
    if decode {
        // Python unidecode maps U+30FC (katakana-hiragana prolonged sound
        // mark) to '-'; the Rust crate drops it. Pre-substitute to keep
        // byte parity (gold vector: Southern All Stars → 0514151603b25392).
        s = unidecode::unidecode(&s.replace('\u{30FC}', "-"));
    }
    format!("{:016x}", xxh3_64(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rna_matches_python() {
        assert_eq!(rna("Tate McRae"), "tatemcrae");
        assert_eq!(rna("  A$AP Rocky! "), "aaprocky");
        assert_eq!(rna("..."), "..."); // no alnum → fallback to de-spaced token
    }

    #[test]
    fn xxh3_empty_vector() {
        assert_eq!(create_hash(&[""], false), "2d06800538d394c2");
    }

    #[test]
    fn create_hash_vectors_live() {
        // verified against live python create_hash() 2026-11-17
        assert_eq!(create_hash(&["AR劉夫陽"], true), "5eaa63b3e10104f9");
        assert_eq!(create_hash(&["Tate McRae"], true), "442538f77201bed3");
        assert_eq!(create_hash(&["THINK LATER", "Tate McRae"], false), "22df59f4861103a0");
        assert_eq!(create_hash(&["a"], true), "e6c632b61e964e1f");
        assert_eq!(create_hash(&["The   Weeknd"], false), "e159c3baaab9a1fb");
        assert_eq!(create_hash(&["A$AP Rocky!"], false), "175cb5ac500d6369");
        assert_eq!(create_hash(&["a", "b", "c"], false), "78af5f94892f3950");
        assert_eq!(create_hash(&["Beyoncé"], false), "3191c36a29b65358");
        assert_eq!(create_hash(&["...And Justice"], false), "3eec0d191087a07e");
        assert_eq!(create_hash(&["X JAPAN"], true), "b6c3ba2e2e113d2d");
        // U+30FC katakana dash: Python unidecode → '-', Rust crate drops it;
        // pre-substitution keeps this byte-identical to live CT101 gold.
        assert_eq!(
            create_hash(&["Southern All Stars - サザンオールスターズ"], true),
            "0514151603b25392"
        );
        // NOTE: golden albumhash 422dd2d6ff4397c5 comes from the scan-time call
        // site (lib/taglib.py) — needs its own vector once args are pinned.
    }
}
