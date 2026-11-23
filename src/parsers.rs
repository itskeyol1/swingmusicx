//! Port of swingmusic/utils/parsers.py + enums/album_versions.py.
//! All regexes mirrored from python (pattern + flags + non-greedy semantics).
//! Vectors pinned 2026-11-17 against live code.

use crate::natsort::casefold;
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

/// enums/album_versions.py — (member_name.lower(), keywords) in definition order.
pub const ALBUM_VERSIONS: &[(&str, &[&str])] = &[
    ("explicit", &["explicit"]),
    ("_360_audio", &["360 audio"]),
    ("anniversary_edition", &["anniversary"]),
    ("diamond_edition", &["diamond"]),
    ("centennial_edition", &["centennial"]),
    ("golden_edition", &["gold"]),
    ("platinum_edition", &["platinum"]),
    ("silver_edition", &["silver"]),
    ("ultimate_edition", &["ultimate"]),
    ("expanded", &["expanded"]),
    ("extended", &["extended"]),
    ("deluxe", &["deluxe"]),
    ("super_deluxe", &["super deluxe"]),
    ("complete", &["complete"]),
    ("legacy_edition", &["legacy"]),
    ("special_edition", &["special"]),
    ("collectors_edition", &["collector"]),
    ("archive_edition", &["archive"]),
    ("acoustic", &["acoustic"]),
    ("instrumental", &["instrumental"]),
    ("double_disc", &["double disc", "double disk"]),
    ("unplugged", &["unplugged"]),
    ("summer_edition", &["summer"]),
    ("winter_edition", &["winter"]),
    ("spring_edition", &["spring"]),
    ("fall_edition", &["fall"]),
    ("bonus_edition", &["bonus"]),
    ("bonus_track", &["bonus track"]),
    ("original", &["original", " og ", "og "]),
    ("intl_version", &["international"]),
    ("uk_version", &["uk version"]),
    ("us_version", &["us version"]),
    ("parental_advisory", &["PA version"]),
    ("limited_edition", &["limited"]),
    ("mono", &["mono"]),
    ("stereo", &["stereo"]),
    ("hi_res", &["Hi-Res"]),
    ("re_mix", &["re-mix"]),
    ("re_recorded", &["re-recorded", "rerecorded"]),
    ("reissue", &["reissue"]),
    ("remastered", &["remaster"]),
];

/// get_all_keywords(): "|".join("|".join(value) for member) — same order as python.
fn all_keywords() -> String {
    ALBUM_VERSIONS
        .iter()
        .map(|(_, kws)| kws.join("|"))
        .collect::<Vec<_>>()
        .join("|")
}

fn base_title_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?i)\s*(\(|\[)[^)\]]*?({})[^)\]]*?(\)|\])$",
            all_keywords()
        ))
        .unwrap()
    })
}

fn feat_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\((?:feat|ft|featuring|with)\.?\s+(.+?)\)").unwrap())
}

fn feat_sqr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\[(?:feat|ft|featuring|with)\.?\s+(.+?)\]").unwrap())
}

fn prod_bracket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\s?(\(|\[)prod\..*?(\)|\])\s?").unwrap())
}

fn prod_bare_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\s?\bprod\.\s*\S+").unwrap())
}

fn remaster_bracket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\s*[\[(][^)\]]*remaster[^)\]]*[)\]]\s*").unwrap())
}

fn remaster_hyphen_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\s-\s*[^-]*\bremaster[^-]*\s*").unwrap())
}

/// split_artists(src, config): ignore-list match (case-insensitive, first entry
/// wins) → kept whole; separator char → flush; comma always splits on flush.
/// NOTE: python set iteration order is arbitrary; we keep list order for
/// determinism. Separators are single-char in practice (default {";", "/"}).
pub fn split_artists(src: &str, separators: &HashSet<String>, ignore_list: &[String]) -> Vec<String> {
    let chars: Vec<char> = src.chars().collect();
    let mut result: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut i = 0usize;

    fn flush(result: &mut Vec<String>, current: &mut String) {
        if !current.trim().is_empty() {
            for a in current.split(',') {
                let a = a.trim();
                if !a.is_empty() {
                    result.push(a.to_string());
                }
            }
            current.clear();
        }
    }

    while i < chars.len() {
        let mut mlen = 0usize;
        for ig in ignore_list {
            let plen = ig.chars().count();
            if i + plen <= chars.len() {
                let slice: String = chars[i..i + plen].iter().collect();
                if slice.to_lowercase() == ig.to_lowercase() {
                    mlen = plen;
                    break;
                }
            }
        }

        if mlen > 0 {
            flush(&mut result, &mut current);
            // python appends the slice from src (original case)
            let slice: String = chars[i..i + mlen].iter().collect();
            result.push(slice);
            i += mlen;
        } else if separators.contains(&chars[i].to_string()) {
            flush(&mut result, &mut current);
            i += 1;
        } else {
            current.push(chars[i]);
            i += 1;
        }
    }

    flush(&mut result, &mut current);
    result
}

/// remove_prod(title).
pub fn remove_prod(title: &str) -> String {
    if !title.to_lowercase().contains("prod.") {
        return title.to_string();
    }
    let re = if title.chars().any(|c| "()[]".contains(c)) {
        prod_bracket_re()
    } else {
        prod_bare_re()
    };
    re.replace_all(title, "").trim().to_string()
}

/// parse_feat_from_title(title, config) -> (artists, new_title) — title NOT stripped.
pub fn parse_feat_from_title(
    title: &str,
    separators: &HashSet<String>,
    ignore_list: &[String],
) -> (Vec<String>, String) {
    let (re, m) = match feat_re().captures(title) {
        Some(m) => (feat_re(), m),
        None => match feat_sqr_re().captures(title) {
            Some(m) => (feat_sqr_re(), m),
            None => return (vec![], title.to_string()),
        },
    };

    let artists = split_artists(m.get(1).unwrap().as_str(), separators, ignore_list);
    let new_title = re.replace_all(title, "").to_string();
    (artists, new_title)
}

/// get_base_album_title(string) -> (title, Option<removed_block>).
pub fn get_base_album_title(s: &str) -> (String, Option<String>) {
    match base_title_re().find(s) {
        Some(m) => {
            let removed = m.as_str();
            (
                s.replace(removed, "").trim().to_string(),
                Some(removed.trim().to_string()),
            )
        }
        None => (s.to_string(), None),
    }
}

/// get_album_info(bracket_text) -> version names in enum order.
/// NOTE: python's `if "anniversary" in versions: get_anniversary(...)` branch is
/// unreachable (member name lowercases to "anniversary_edition"), not ported.
pub fn get_album_info(bracket_text: Option<&str>) -> Vec<String> {
    let Some(bt) = bracket_text else {
        return vec![];
    };
    let cleaned: String = bt
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace())
        .collect();
    let lower = cleaned.to_lowercase();

    let mut versions = Vec::new();
    for (name, kws) in ALBUM_VERSIONS {
        if kws.iter().any(|k| lower.contains(&k.to_lowercase())) {
            versions.push(name.to_string());
        }
    }
    versions
}

/// get_base_title_and_versions(original, get_versions=True).
pub fn get_base_title_and_versions(original: &str, get_versions: bool) -> (String, Vec<String>) {
    let (album_title, version_block) = get_base_album_title(original);
    let Some(block) = version_block else {
        return (original.to_string(), vec![]);
    };
    if !get_versions {
        return (album_title, vec![]);
    }
    let versions = get_album_info(Some(&block));
    if versions.is_empty() {
        return (original.to_string(), versions);
    }
    (album_title, versions)
}

/// remove_bracketed_remaster.
pub fn remove_bracketed_remaster(text: &str) -> String {
    remaster_bracket_re().replace_all(text, "").trim().to_string()
}

/// remove_hyphen_remasters.
pub fn remove_hyphen_remasters(text: &str) -> String {
    remaster_hyphen_re().replace_all(text, "").trim().to_string()
}

/// clean_title — keeps whichever removal is LONGER (python: rem_1 if len(rem_2)
/// > len(rem_1) else rem_2).
pub fn clean_title(title: &str) -> String {
    if !title.to_lowercase().contains("remaster") {
        return title.to_string();
    }
    let rem_1 = remove_bracketed_remaster(title);
    let rem_2 = remove_hyphen_remasters(title);
    if rem_2.chars().count() > rem_1.chars().count() {
        rem_1
    } else {
        rem_2
    }
}

/// get_sort_name(name, articles) — strips leading article for sorting.
/// NOTE: python raises ValueError on names that split into one token
/// (e.g. " A"); we return the name unchanged instead.
pub fn get_sort_name(name: &str, articles: &HashSet<String>) -> String {
    if !name.contains(' ') {
        return name.to_string();
    }
    if let Some((article, rest)) = name.split_once(char::is_whitespace) {
        if articles.contains(&casefold(article)) {
            return rest.trim_start().to_string();
        }
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seps() -> HashSet<String> {
        [";", "/"].iter().map(|s| s.to_string()).collect()
    }

    fn igs() -> Vec<String> {
        [
            "AC/DC",
            "Simon & Garfunkel",
            "Crosby, Stills, Nash & Young",
            "Earth, Wind & Fire",
            "Tyler, The Creator",
            "Yusuf / Cat Stevens",
            "Chloe x Halle",
            "? and the Mysterians",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn split_artists_vectors_live() {
        let s = |x: &str| split_artists(x, &seps(), &igs());
        assert_eq!(s("AC/DC"), ["AC/DC"]);
        assert_eq!(s("A/B/C"), ["A", "B", "C"]);
        assert_eq!(s("A & B"), ["A & B"]);
        assert_eq!(s("AC/DC; Beyonce"), ["AC/DC", "Beyonce"]);
        assert_eq!(s("Yusuf / Cat Stevens"), ["Yusuf / Cat Stevens"]);
        assert_eq!(s("Chloe x Halle"), ["Chloe x Halle"]);
        assert_eq!(s("Tate McRae"), ["Tate McRae"]);
        assert_eq!(s("AC/DC /Simon & Garfunkel"), ["AC/DC", "Simon & Garfunkel"]);
        assert!(s(";").is_empty());
        assert_eq!(s("a/b"), ["a", "b"]);
        assert_eq!(
            s("Crosby, Stills, Nash & Young; X"),
            ["Crosby, Stills, Nash & Young", "X"]
        );
    }

    #[test]
    fn remove_prod_vectors_live() {
        assert_eq!(remove_prod("Song (prod. X)"), "Song");
        assert_eq!(remove_prod("Song [prod. Y]"), "Song");
        assert_eq!(remove_prod("Song prod. Z"), "Song");
        assert_eq!(remove_prod("Song"), "Song");
        assert_eq!(remove_prod("Song (PROD. Q)"), "Song");
        assert_eq!(remove_prod("Song (prod. A B) tail"), "Songtail");
    }

    #[test]
    fn parse_feat_vectors_live() {
        let p = |x: &str| parse_feat_from_title(x, &seps(), &igs());
        assert_eq!(p("Title (feat. A)"), (vec!["A".to_string()], "Title ".to_string()));
        assert_eq!(p("Title [ft. B & C]"), (vec!["B & C".to_string()], "Title ".to_string()));
        assert_eq!(p("Title (featuring D)"), (vec!["D".to_string()], "Title ".to_string()));
        assert_eq!(p("Title"), (vec![], "Title".to_string()));
        assert_eq!(p("Title (with E)"), (vec!["E".to_string()], "Title ".to_string()));
        assert_eq!(p("T (feat. AC/DC)"), (vec!["AC/DC".to_string()], "T ".to_string()));
        assert_eq!(
            p("T (ft. A, B)"),
            (vec!["A".to_string(), "B".to_string()], "T ".to_string())
        );
    }

    #[test]
    fn base_title_vectors_live() {
        let b = |x: &str| get_base_title_and_versions(x, true);
        assert_eq!(b("Album (Deluxe Edition)"), ("Album".to_string(), vec!["deluxe".to_string()]));
        assert_eq!(b("Album [Special Edition]"), ("Album".to_string(), vec!["special_edition".to_string()]));
        assert_eq!(b("Album"), ("Album".to_string(), vec![]));
        assert_eq!(b("Album (2024 Remaster)"), ("Album".to_string(), vec!["remastered".to_string()]));
        assert_eq!(
            b("Album (Bonus Track Version)"),
            ("Album".to_string(), vec!["bonus_edition".to_string(), "bonus_track".to_string()])
        );
        assert_eq!(b("Album (EP)"), ("Album (EP)".to_string(), vec![]));
        assert_eq!(b("Album - Single"), ("Album - Single".to_string(), vec![]));
        assert_eq!(b("Album (10th Anniversary Edition)"), ("Album".to_string(), vec!["anniversary_edition".to_string()]));
        assert_eq!(b("Album (Deluxe) x"), ("Album (Deluxe) x".to_string(), vec![]));
        assert_eq!(
            b("Album (Super Deluxe)"),
            ("Album".to_string(), vec!["deluxe".to_string(), "super_deluxe".to_string()])
        );
        assert_eq!(b("Album (OG)"), ("Album (OG)".to_string(), vec![]));
        assert_eq!(b("Album (UK Version)"), ("Album".to_string(), vec!["uk_version".to_string()]));
        // get_versions=False drops versions but keeps stripped title
        assert_eq!(
            get_base_title_and_versions("Album (Deluxe Edition)", false),
            ("Album".to_string(), vec![])
        );
    }

    #[test]
    fn clean_title_vectors_live() {
        assert_eq!(clean_title("Song (Remastered 2011)"), "Song");
        assert_eq!(clean_title("Song - Remaster"), "Song");
        assert_eq!(clean_title("Song (Live)"), "Song (Live)");
        assert_eq!(clean_title("Song"), "Song");
        assert_eq!(clean_title("Song [Remaster] - x"), "Song- x");
        assert_eq!(clean_title("Song - 2011 Remastered Version"), "Song");
    }

    #[test]
    fn album_info_vectors_live() {
        assert_eq!(get_album_info(Some("Bonus Track Version")), ["bonus_edition", "bonus_track"]);
        assert_eq!(get_album_info(Some("Super Deluxe")), ["deluxe", "super_deluxe"]);
        assert_eq!(get_album_info(Some("Deluxe")), ["deluxe"]);
    }

    #[test]
    fn sort_name_vectors_live() {
        let arts: HashSet<String> = ["the", "a", "an"].iter().map(|s| s.to_string()).collect();
        assert_eq!(get_sort_name("The Weeknd", &arts), "Weeknd");
        assert_eq!(get_sort_name("A Perfect Circle", &arts), "Perfect Circle");
        assert_eq!(get_sort_name("Radiohead", &arts), "Radiohead");
        assert_eq!(get_sort_name("An Artist", &arts), "Artist");
    }
}
