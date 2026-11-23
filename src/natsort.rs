//! Minimal port of natsort 8.4 `natsorted` default key (numbers as int, strings
//! as str chunks, leading empty str chunk before a leading number).
//! natsort normalizes input to NFD before parsing (é → e + combining acute),
//! so 'E' < 'é' < 'The Weeknd'. Vectors pinned from live natsort 8.4.0 (2026-11-17).

use std::cmp::Ordering;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Chunk {
    Str(String),
    Num(u128),
}

/// natsort default key: "1.5" → ["", 1, ".", 5]; "Track 9" → ["Track ", 9]; "a" → ["a"].
pub fn nat_key(s: &str) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    let mut cur = String::new();
    let mut digits: Option<String> = None;

    for ch in s.nfd() {
        if ch.is_ascii_digit() {
            if digits.is_none() {
                // number starts: flush pending string (even empty, python keeps it)
                out.push(Chunk::Str(std::mem::take(&mut cur)));
                digits = Some(String::new());
            }
            digits.as_mut().unwrap().push(ch);
        } else {
            if let Some(d) = digits.take() {
                out.push(num_chunk(&d));
            }
            cur.push(ch);
        }
    }
    if let Some(d) = digits.take() {
        out.push(num_chunk(&d));
    }
    if !cur.is_empty() {
        out.push(Chunk::Str(cur));
    }
    out
}

fn num_chunk(d: &str) -> Chunk {
    let t = d.trim_start_matches('0');
    if t.is_empty() {
        return Chunk::Num(0);
    }
    // ponytail: u128 covers any real-world title; >39 digits falls back to str
    t.parse::<u128>().map(Chunk::Num).unwrap_or_else(|_| Chunk::Str(d.to_string()))
}

fn cmp_chunk(a: &Chunk, b: &Chunk) -> Ordering {
    match (a, b) {
        (Chunk::Str(x), Chunk::Str(y)) => x.cmp(y),
        (Chunk::Num(x), Chunk::Num(y)) => x.cmp(y),
        // python tuple compare can't hit mixed types at same index; keep it total
        (Chunk::Str(_), Chunk::Num(_)) => Ordering::Less,
        (Chunk::Num(_), Chunk::Str(_)) => Ordering::Greater,
    }
}

pub fn cmp_keys(a: &[Chunk], b: &[Chunk]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let o = cmp_chunk(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Stable sort by natural key of a derived string (apply casefold first when the
/// python call-site passes `key=str.casefold`).
pub fn natsorted_by<T, F: Fn(&T) -> String>(items: &mut [T], key: F) {
    items.sort_by(|a, b| cmp_keys(&nat_key(&key(a)), &nat_key(&key(b))));
}

pub fn natsorted(items: &mut Vec<String>) {
    items.sort_by(|a, b| cmp_keys(&nat_key(a), &nat_key(b)));
}

/// Python str.casefold() approximation (ß → ss; rest = to_lowercase).
pub fn casefold(s: &str) -> String {
    s.replace('ß', "ss").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Vec<Chunk> {
        nat_key(s)
    }

    #[test]
    fn vectors_live() {
        assert_eq!(key("1.5"), vec![Chunk::Str("".into()), Chunk::Num(1), Chunk::Str(".".into()), Chunk::Num(5)]);
        assert_eq!(key("1,5"), vec![Chunk::Str("".into()), Chunk::Num(1), Chunk::Str(",".into()), Chunk::Num(5)]);
        assert_eq!(key("v1.02"), key("v1.2"));
        assert_eq!(key("01"), key("1"));
        assert_eq!(key("a01b2"), vec![Chunk::Str("a".into()), Chunk::Num(1), Chunk::Str("b".into()), Chunk::Num(2)]);
        assert_eq!(key("Track 9"), vec![Chunk::Str("Track ".into()), Chunk::Num(9)]);
        assert_eq!(key("09"), vec![Chunk::Str("".into()), Chunk::Num(9)]);
        assert_eq!(key("009"), vec![Chunk::Str("".into()), Chunk::Num(9)]);
        assert_eq!(key("ABC"), vec![Chunk::Str("ABC".into())]);
        assert_eq!(key("é"), vec![Chunk::Str("e\u{301}".into())]); // NFD, python: ('é',)
        assert_eq!(key("-1"), vec![Chunk::Str("-".into()), Chunk::Num(1)]);
        assert_eq!(key("+1"), vec![Chunk::Str("+".into()), Chunk::Num(1)]);
        assert_eq!(key("...And"), vec![Chunk::Str("...And".into())]);
    }

    #[test]
    fn sorted_vectors_live() {
        let mut v: Vec<String> = ["abba","ABBA","a10","a2","a1","01","1","v1.2","v1.02"].iter().map(|s| s.to_string()).collect();
        natsorted(&mut v);
        assert_eq!(v, ["01","1","ABBA","a1","a2","a10","abba","v1.2","v1.02"]);

        let mut v: Vec<String> = ["abba","ABBA","a10","a2","a1","01","1","v1.2","v1.02"].iter().map(|s| s.to_string()).collect();
        natsorted_by(&mut v, |s| casefold(s));
        assert_eq!(v, ["01","1","a1","a2","a10","abba","ABBA","v1.2","v1.02"]);

        let mut v: Vec<String> = ["ABBA","abba","a10","a2","a1","01","1","v1.2","v1.02","The Weeknd","the weeknd","A$AP","2Pac","2pac","50 Cent","50cent","é","E","e"].iter().map(|s| s.to_string()).collect();
        natsorted_by(&mut v, |s| casefold(s));
        assert_eq!(v, ["01","1","2Pac","2pac","50 Cent","50cent","a1","a2","a10","A$AP","ABBA","abba","E","e","é","The Weeknd","the weeknd","v1.2","v1.02"]);
    }
}
