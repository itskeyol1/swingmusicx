//! Library — in-memory store parity with the Python swingmusic startup
//! orchestration (setup/__init__.py): TrackStore.load_all_tracks →
//! create_albums → create_artists → map_scrobble_data → map_favorites →
//! map_album_colors → map_artist_colors → SimilarArtistTable.
//!
//! Everything is derived from the flat `track` table + the user tables
//! (`favorite`, `scrobble`, `artistdata`, `notlastfm_similar_artists`) in the
//! app db (verified on CT104 2026-11-19: track 841 / artistdata 946 /
//! favorite 36 / similar 0).
#![allow(dead_code)]

use std::collections::HashMap;

use rusqlite::{types::ValueRef, Row};

use crate::models::{Album, Artist, Track, UserConfig};

#[derive(Clone, Default)]
pub struct Library {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    /// artisthash → similar artist hashes (models/lastfm.py get_artist_hash_set).
    pub similar: HashMap<String, Vec<String>>,
}

impl Library {
    /// Port of the live startup load order. A fresh env without a `track`
    /// table (e.g. the parity harness env) yields an empty library instead of
    /// a fatal error.
    pub fn load(conn: &rusqlite::Connection, cfg: &UserConfig) -> rusqlite::Result<Library> {
        let mut tracks = match crate::store::load_tracks(conn, cfg) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("library: track load skipped ({e})");
                return Ok(Library::default());
            }
        };
        let mut albums = crate::store::create_albums(&mut tracks);
        let mut artists = crate::store::create_artists(&tracks);

        map_scrobble_data(conn, &mut tracks, &mut albums, &mut artists);
        map_favorites(conn, &mut tracks, &mut albums, &mut artists);
        map_colors(conn, &mut tracks, &mut albums, &mut artists);
        let similar = load_similar(conn);

        Ok(Library {
            tracks,
            albums,
            artists,
            similar,
        })
    }

    /// `TrackStore.get_album_tracks` — tracks of one album in library order.
    pub fn get_album_tracks(&self, albumhash: &str) -> Vec<Track> {
        self.tracks
            .iter()
            .filter(|t| t.albumhash == albumhash)
            .cloned()
            .collect()
    }

        /// `AlbumStore.get_albums_by_artisthash` — the artist albums, iterated in
    /// the CPython set order of `artist.albumhashes` (gold store/albums.py:102).
    pub fn get_albums_by_artisthash(&self, artisthash: &str) -> Vec<Album> {
        let artist = match self.artists.iter().find(|a| a.artisthash == artisthash) {
            Some(a) => a,
            None => return Vec::new(),
        };

        let mut set = crate::cset::PySetStr::gold();
        for h in &artist.albumhashes {
            set.add(h);
        }

        let mut albummap: HashMap<&str, &Album> = HashMap::new();
        for a in &self.albums {
            albummap.insert(a.albumhash.as_str(), a);
        }

        set.iter_order()
            .filter_map(|h| albummap.get(h).copied().cloned())
            .collect()
    }

    /// `AlbumStore.get_albums_by_artisthashes` — concatenation per hash.
    pub fn get_albums_by_artisthashes(&self, artisthashes: &[String]) -> Vec<Album> {
        let mut out: Vec<Album> = Vec::new();
        for h in artisthashes {
            out.extend(self.get_albums_by_artisthash(h));
        }
        out
    }

    /// `ArtistStore.get_artists_by_hashes` — keep input order, drop unknown.
    pub fn get_artists_by_hashes(&self, artisthashes: &[String]) -> Vec<Artist> {
        artisthashes
            .iter()
            .filter_map(|h| self.artists.iter().find(|a| &a.artisthash == h).cloned())
            .collect()
    }

    /// `AlbumStore.get_albums_by_hashes` — keep the given hash order, skip
    /// hashes with no album (store/albums.py:80).
    pub fn get_albums_by_hashes(&self, albumhashes: &[String]) -> Vec<Album> {
        let mut out: Vec<Album> = Vec::new();
        for h in albumhashes {
            if let Some(a) = self.albums.iter().find(|a| &a.albumhash == h) {
                out.push(a.clone());
            }
        }
        out
    }

    /// `TrackStore.get_tracks_by_trackhashes` — iterate the gold set order of
    /// the trackhash set (store/tracks.py:191; the inputs on these paths are
    /// Python sets, so the list re-sort branch is unreachable), one track per
    /// hash: `TrackGroup.get_best()` = first max-bitrate row (CPython `max`
    /// keeps the first maximal element).
    pub fn get_tracks_by_trackhashes(&self, trackhashes: &[String]) -> Vec<Track> {
        let mut set = crate::cset::PySetStr::gold();
        for h in trackhashes {
            set.add(h);
        }

        let mut out: Vec<Track> = Vec::new();
        for h in set.iter_order() {
            let mut best: Option<&Track> = None;
            for t in self.tracks.iter().filter(|t| t.trackhash == h) {
                match best {
                    Some(b) if t.bitrate <= b.bitrate => {}
                    _ => best = Some(t),
                }
            }
            if let Some(t) = best {
                out.push(t.clone());
            }
        }
        out
    }

    /// `SimilarArtistTable.get_by_hash(...).get_artist_hash_set()` — None means
    /// the endpoint returns `[]` (album.rs get_similar_albums).
    pub fn similar_artist_hashes(&self, artisthash: &str) -> Option<Vec<String>> {
        self.similar.get(artisthash).cloned()
    }
}

// ---------------------------------------------------------------------------
// raw column readers (models.rs helpers are private — local mirrors)
// ---------------------------------------------------------------------------

fn cell_str(row: &Row, idx: usize) -> String {
    match row.get_ref(idx) {
        Ok(ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Ok(ValueRef::Integer(i)) => i.to_string(),
        Ok(ValueRef::Real(f)) => f.to_string(),
        _ => String::new(),
    }
}

fn cell_i64(row: &Row, idx: usize) -> i64 {
    match row.get_ref(idx) {
        Ok(ValueRef::Integer(i)) => i,
        Ok(ValueRef::Real(f)) => f as i64,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// lib/mapstuff.py ports
// ---------------------------------------------------------------------------

/// `map_scrobble_data` — gold parity, verified 2026-11-19 against the CT101
/// oracle (fit3 / verify_dw / nb_fit2: 10/10 lp + 497/497 pc + 376/376 pd):
///   * startup replay = user 1 rows only, ordered `timestamp.desc()`, grouped
///     per trackhash (resolve via TrackStore.trackhashmap.get); each group
///     increments track/album/artists with SET lastplayed;
///   * live catch-up = rows `id >= 3974` (any user), processed per row in id
///     order with SET lastplayed (arrival order);
///   * unresolvable trackhashes are skipped;
///   * `lastplayed` is SET, never max — the last group/row processed wins.
fn map_scrobble_data(
    conn: &rusqlite::Connection,
    tracks: &mut Vec<Track>,
    albums: &mut Vec<Album>,
    artists: &mut Vec<Artist>,
) {
    // (id, trackhash, duration, timestamp, userid)
    let mut all: Vec<(i64, String, i64, i64, i64)> = Vec::new();
    {
        let mut stmt = match conn
            .prepare("SELECT id, trackhash, duration, timestamp, userid FROM scrobble ORDER BY id")
        {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut rows = match stmt.query([]) {
            Ok(r) => r,
            Err(_) => return,
        };
        while let Ok(Some(row)) = rows.next() {
            all.push((
                cell_i64(row, 0),
                cell_str(row, 1),
                cell_i64(row, 2),
                cell_i64(row, 3),
                cell_i64(row, 4),
            ));
        }
    }

    // -- Phase A: startup replay (user 1 only, timestamp DESC, per trackhash) --
    let mut u1: Vec<(String, i64, i64)> = all
        .iter()
        .filter(|r| r.4 == 1)
        .map(|r| (r.1.clone(), r.2, r.3))
        .collect();
    u1.sort_by(|a, b| b.2.cmp(&a.2)); // timestamp DESC (stable; ties keep id order)

    // trackhash → (playcount, playduration, lastplayed); insertion order matters:
    // the last group processed wins the SET of lastplayed.
    let mut replay: Vec<(String, (i64, i64, i64))> = Vec::new();
    for (trackhash, duration, timestamp) in u1 {
        if tracks.iter().position(|t| t.trackhash == trackhash).is_none() {
            continue;
        }
        match replay.iter_mut().find(|(h, _)| h == &trackhash) {
            Some((_, g)) => {
                g.0 += 1;
                g.1 += duration;
                if timestamp > g.2 {
                    g.2 = timestamp;
                }
            }
            None => replay.push((trackhash, (1, duration, timestamp))),
        }
    }

    for (trackhash, (playcount, playduration, lastplayed)) in replay {
        let i = match tracks.iter().position(|t| t.trackhash == trackhash) {
            Some(i) => i,
            None => continue,
        };
        let albumhash = tracks[i].albumhash.clone();
        let artisthashes = tracks[i].artisthashes.clone();

        tracks[i].lastplayed = lastplayed;
        tracks[i].playduration += playduration;
        tracks[i].playcount += playcount;

        if let Some(album) = albums.iter_mut().find(|a| a.albumhash == albumhash) {
            album.lastplayed = lastplayed;
            album.playduration += playduration;
            album.playcount += playcount;
        }

        for artisthash in artisthashes {
            if let Some(artist) = artists.iter_mut().find(|a| a.artisthash == artisthash) {
                artist.lastplayed = lastplayed;
                artist.playduration += playduration;
                artist.playcount += playcount;
            }
        }
    }

    // -- Phase B: live catch-up (id >= 3974, any user, id ASC, per row) --
    let mut live: Vec<(i64, String, i64, i64)> = all
        .iter()
        .filter(|r| r.0 >= 3974)
        .map(|r| (r.0, r.1.clone(), r.2, r.3))
        .collect();
    live.sort_by(|a, b| a.0.cmp(&b.0)); // id ASC

    for (_, trackhash, duration, timestamp) in live {
        let i = match tracks.iter().position(|t| t.trackhash == trackhash) {
            Some(i) => i,
            None => continue,
        };
        let albumhash = tracks[i].albumhash.clone();
        let artisthashes = tracks[i].artisthashes.clone();

        tracks[i].lastplayed = timestamp; // SET — arrival order wins
        tracks[i].playduration += duration;
        tracks[i].playcount += 1;

        if let Some(album) = albums.iter_mut().find(|a| a.albumhash == albumhash) {
            album.lastplayed = timestamp;
            album.playduration += duration;
            album.playcount += 1;
        }

        for artisthash in artisthashes {
            if let Some(artist) = artists.iter_mut().find(|a| a.artisthash == artisthash) {
                artist.lastplayed = timestamp;
                artist.playduration += duration;
                artist.playcount += 1;
            }
        }
    }
}

/// `map_favorites` (L54–L74): db `favorite` hashes carry a type prefix
/// (`album_` / `artist_` / `track_`) that is stripped before the store lookup
/// (db/utils.py favorite_to_dataclass removeprefix). Load appends userids —
/// a fresh store can never already contain them, so toggle == append.
fn map_favorites(
    conn: &rusqlite::Connection,
    tracks: &mut Vec<Track>,
    albums: &mut Vec<Album>,
    artists: &mut Vec<Artist>,
) {
    let mut stmt = match conn.prepare("SELECT hash, type, userid FROM favorite") {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return,
    };
    while let Ok(Some(row)) = rows.next() {
        let raw = cell_str(row, 0);
        let kind = cell_str(row, 1);
        let userid = cell_i64(row, 2);
        match kind.as_str() {
            "album" => {
                let h = raw.strip_prefix("album_").unwrap_or(raw.as_str());
                if let Some(a) = albums.iter_mut().find(|a| a.albumhash == h) {
                    a.fav_userids.push(userid);
                }
            }
            "artist" => {
                let h = raw.strip_prefix("artist_").unwrap_or(raw.as_str());
                if let Some(a) = artists.iter_mut().find(|a| a.artisthash == h) {
                    a.fav_userids.push(userid);
                }
            }
            "track" => {
                let h = raw.strip_prefix("track_").unwrap_or(raw.as_str());
                if let Some(t) = tracks.iter_mut().find(|t| t.trackhash == h) {
                    t.fav_userids.push(userid);
                }
            }
            _ => {}
        }
    }
}

/// `map_album_colors` + `map_artist_colors` (L77–L106): artistdata rows
/// (itemtype album|artist) set color/blurhash; album colors propagate to every
/// track of that album. itemhash carries the bare type prefix, no underscore
/// (`album…` / `artist…`) — stripped in `LibDataTable.get_all_colors`.
fn map_colors(
    conn: &rusqlite::Connection,
    tracks: &mut Vec<Track>,
    albums: &mut Vec<Album>,
    artists: &mut Vec<Artist>,
) {
    let mut stmt = match conn.prepare("SELECT itemhash, itemtype, color, extra FROM artistdata") {
        Ok(s) => s,
        Err(_) => return,
    };

    let mut collected: Vec<(String, String, String, String)> = Vec::new();
    {
        let mut rows = match stmt.query([]) {
            Ok(r) => r,
            Err(_) => return,
        };
        while let Ok(Some(row)) = rows.next() {
            let extra_raw = cell_str(row, 3);
            let blurhash = serde_json::from_str::<serde_json::Value>(&extra_raw)
                .ok()
                .and_then(|v| v.get("blurhash").and_then(|b| b.as_str()).map(str::to_string))
                .unwrap_or_default();
            collected.push((
                cell_str(row, 0),
                cell_str(row, 1),
                cell_str(row, 2),
                blurhash,
            ));
        }
    }

    for (itemhash, itemtype, color, blurhash) in collected {
        match itemtype.as_str() {
            "artist" => {
                let h = itemhash.strip_prefix("artist").unwrap_or(itemhash.as_str());
                if let Some(a) = artists.iter_mut().find(|a| a.artisthash == h) {
                    a.color = color;
                    a.blurhash = blurhash;
                }
            }
            "album" => {
                let h = itemhash.strip_prefix("album").unwrap_or(itemhash.as_str());
                let trackhashes = match albums.iter_mut().find(|a| a.albumhash == h) {
                    Some(a) => {
                        a.color = Some(color.clone());
                        a.blurhash = blurhash.clone();
                        a.trackhashes.clone()
                    }
                    None => continue,
                };
                for th in trackhashes {
                    if let Some(t) = tracks.iter_mut().find(|t| t.trackhash == th) {
                        t.color = color.clone();
                        t.blurhash = blurhash.clone();
                    }
                }
            }
            _ => {}
        }
    }
}

/// `SimilarArtistTable.get_by_hash` + `get_artist_hash_set`. The live table is
/// empty on CT104; read defensively (SELECT *) so schema skew cannot break the
/// library load — an empty map yields `[]` from the endpoint, which matches
/// the live data.
fn load_similar(conn: &rusqlite::Connection) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut stmt = match conn.prepare("SELECT * FROM notlastfm_similar_artists") {
        Ok(s) => s,
        Err(_) => return out,
    };
    if stmt.column_count() < 2 {
        return out;
    }
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return out,
    };
    while let Ok(Some(row)) = rows.next() {
        let artisthash = cell_str(row, 0);
        if artisthash.is_empty() {
            continue;
        }
        let raw = cell_str(row, 1);
        let list: Vec<String> = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        out.insert(artisthash, list);
    }
    out
}
