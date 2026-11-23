//! cset.rs — Faithful port of CPython 3.12.0 set iteration for string keys.
//!
//! Why: gold swingmusic (livesrc) stores `artist.albumhashes` as a Python set
//! and iterates it when listing albums per artist (store/albums.py:101,
//! `[albummap[h].album for h in artist.albumhashes]`). CPython set order is
//! deterministic given (a) the process hash seed and (b) the insertion order:
//! it is neither insertion nor sorted order. This module reproduces that order
//! byte-exactly so the Rust port matches the gold fixtures.
//!
//! Reference: CPython v3.12.0 Objects/setobject.c — LINEAR_PROBES=9,
//! PERTURB_SHIFT=5, set_add_entry / set_insert_clean / set_table_resize;
//! Python/pyhash.c — siphash13 over compact-unicode bytes for str, empty
//! string hashes to 0, raw hash u64::MAX is remapped to u64::MAX-1.
//! Key constants = CPython lcg_urandom(seed) for PYTHONHASHSEED=42, verified
//! against real CPython to reproduce the gold capture order for the fixtures.
//!
//! Scope: albumhash strings are ASCII hex, so the str-hash == siphash13 of the
//! UTF-8 bytes (CPython's ASCII fast path). No deletes, no dummy tombstones.
//! NOT for general use — only for gold-order parity.

/// siphash13 keys equivalent to CPython PYTHONHASHSEED=42 (lcg_urandom(42)).
pub const GOLD_K0: u64 = 0xdc504fd368cd90af;
pub const GOLD_K1: u64 = 0xb920bb9ffe99e9c1;

pub struct PySetStr {
    k0: u64,
    k1: u64,
    table: Vec<Option<(u64, String)>>,
    mask: usize,
    fill: usize,
    used: usize,
}

impl PySetStr {
    const LINEAR_PROBES: usize = 9;
    const PERTURB_SHIFT: u32 = 5;
    const MINSIZE: usize = 8;

    pub fn new(k0: u64, k1: u64) -> Self {
        PySetStr {
            k0,
            k1,
            table: vec![None; Self::MINSIZE],
            mask: Self::MINSIZE - 1,
            fill: 0,
            used: 0,
        }
    }

    /// Keys used by the gold capture environment (PYTHONHASHSEED=42-equivalent).
    pub fn gold() -> Self {
        Self::new(GOLD_K0, GOLD_K1)
    }

    pub fn hash_str(&self, s: &str) -> u64 {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            return 0;
        }
        let x = siphash13(self.k0, self.k1, bytes);
        if x == u64::MAX {
            u64::MAX - 1
        } else {
            x
        }
    }

    fn insert_at(&mut self, idx: usize, h: u64, key: &str) {
        self.table[idx] = Some((h, key.to_string()));
        self.fill += 1;
        self.used += 1;
    }

    fn insert_clean(&mut self, h: u64, key: &str) {
        let mask = self.mask;
        let mut i = (h as usize) & mask;
        let mut perturb = h;
        loop {
            if self.table[i].is_none() {
                self.insert_at(i, h, key);
                return;
            }
            if i + Self::LINEAR_PROBES <= mask {
                for j in (i + 1)..=(i + Self::LINEAR_PROBES) {
                    if self.table[j].is_none() {
                        self.insert_at(j, h, key);
                        return;
                    }
                }
            }
            perturb >>= Self::PERTURB_SHIFT;
            i = (i * 5 + 1 + perturb as usize) & mask;
        }
    }

    fn after_insert(&mut self, mask_before: usize) {
        if self.fill * 5 < mask_before * 3 {
            return;
        }
        let minused = if self.used > 50000 { self.used * 2 } else { self.used * 4 };
        let mut newsize = Self::MINSIZE;
        while newsize <= minused {
            newsize <<= 1;
        }
        let old = std::mem::replace(&mut self.table, vec![None; newsize]);
        self.mask = newsize - 1;
        self.fill = 0;
        self.used = 0;
        for (h, k) in old.into_iter().flatten() {
            self.insert_clean(h, &k);
        }
    }

    pub fn add(&mut self, key: &str) {
        let h = self.hash_str(key);
        let mask = self.mask;
        let mut i = (h as usize) & mask;
        let mut perturb = h;
        loop {
            match &self.table[i] {
                None => {
                    self.insert_at(i, h, key);
                    self.after_insert(mask);
                    return;
                }
                Some((eh, ek)) => {
                    if *eh == h && ek == key {
                        return;
                    }
                }
            }
            if i + Self::LINEAR_PROBES <= mask {
                let mut found: Option<usize> = None;
                for j in (i + 1)..=(i + Self::LINEAR_PROBES) {
                    match &self.table[j] {
                        None => {
                            found = Some(j);
                            break;
                        }
                        Some((eh, ek)) => {
                            if *eh == h && ek == key {
                                return;
                            }
                        }
                    }
                }
                if let Some(idx) = found {
                    self.insert_at(idx, h, key);
                    self.after_insert(mask);
                    return;
                }
            }
            perturb >>= Self::PERTURB_SHIFT;
            i = (i * 5 + 1 + perturb as usize) & mask;
        }
    }

    /// Iterate in CPython set order.
    pub fn iter_order(&self) -> impl Iterator<Item = &str> {
        self.table.iter().flatten().map(|(_, k)| k.as_str())
    }

    pub fn len(&self) -> usize {
        self.used
    }
}

#[inline]
fn rotl(x: u64, b: u32) -> u64 {
    x.rotate_left(b)
}

/// siphash-1-3, ported from CPython Python/pyhash.c.
pub fn siphash13(k0: u64, k1: u64, data: &[u8]) -> u64 {
    let mut v0 = k0 ^ 0x736f6d6570736575;
    let mut v1 = k1 ^ 0x646f72616e646f6d;
    let mut v2 = k0 ^ 0x6c7967656e657261;
    let mut v3 = k1 ^ 0x7465646279746573;

    #[inline]
    fn rnd(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
        *v0 = v0.wrapping_add(*v1);
        *v1 = rotl(*v1, 13);
        *v1 ^= *v0;
        *v0 = rotl(*v0, 32);
        *v2 = v2.wrapping_add(*v3);
        *v3 = rotl(*v3, 16);
        *v3 ^= *v2;
        *v2 = v2.wrapping_add(*v1);
        *v1 = rotl(*v1, 17);
        *v1 ^= *v2;
        *v2 = rotl(*v2, 32);
        *v0 = v0.wrapping_add(*v3);
        *v3 = rotl(*v3, 21);
        *v3 ^= *v0;
    }

    let len = data.len();
    let mut i = 0usize;
    while len - i >= 8 {
        let mi = u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
        v3 ^= mi;
        rnd(&mut v0, &mut v1, &mut v2, &mut v3);
        v0 ^= mi;
        i += 8;
    }
    let mut b: u64 = ((len & 0xff) as u64) << 56;
    let mut t: u64 = 0;
    let mut sh = 0u32;
    while i < len {
        t |= (data[i] as u64) << sh;
        sh += 8;
        i += 1;
    }
    b |= t;
    v3 ^= b;
    rnd(&mut v0, &mut v1, &mut v2, &mut v3);
    v0 ^= b;
    v2 ^= 0xff;
    rnd(&mut v0, &mut v1, &mut v2, &mut v3);
    rnd(&mut v0, &mut v1, &mut v2, &mut v3);
    rnd(&mut v0, &mut v1, &mut v2, &mut v3);
    v0 ^ v1 ^ v2 ^ v3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_order_gold() {
        // gold insertion order: F1 (07d4...) added before THINK LATER (22df...);
        // CPython set iteration yields THINK LATER first — the fixture order.
        let mut s = PySetStr::gold();
        s.add("07d4f4630f96743b");
        s.add("22df59f4861103a0");
        let v: Vec<&str> = s.iter_order().collect();
        assert_eq!(v, vec!["22df59f4861103a0", "07d4f4630f96743b"]);
    }

    #[test]
    fn empty_str_hashes_zero() {
        let s = PySetStr::gold();
        assert_eq!(s.hash_str(""), 0);
    }
}
