//! SwingMusix — port of swingmusic/models/{track,album,artist,folder}.py (Wave A).
//!
//! Parity notes (verified against live CT101 monoswing source, 2026-11-17/18):
//! - `db/sqlite/utils.py` loads tracks with `Track(*row[1:])` — the `id` column is
//!   dropped and every other column maps positionally. Track::from_row therefore
//!   reads sqlite columns 1..=20 in PRAGMA order:
//!   id, album, albumartists, albumhash, artists, bitrate, copyright, date, disc,
//!   duration, filepath, folder, genres, last_mod, title, track, trackhash,
//!   lastplayed, playcount, playduration, extra
//! - weakhash   = create_hash(og_title, *artist_names)             (decode=false)
//! - pathhash   = create_hash(folder with trailing "/")            (decode=false)
//! - trackhash  = create_hash(title, album, *artist_names)         (recreated on load)
//! - album image    = albumhash + ".webp?pathhash=" + pathhash
//! - album weakhash = create_hash(og_title, *albumartist_names)    (2+ args, no join needed —
//!   rna() strips non-alnum so ",".join and arg-splat are byte-identical hashes)
//! - album.genrehashes is a STRING (" ".join) — upstream bug kept 1:1.
//! - is_single() ends with a hash comparison and has NO explicit false branch
//!   (implicit None = falsy); the Rust port returns bool with a final `false`.
#![allow(dead_code)]

use std::collections::HashSet;

use rusqlite::types::ValueRef;
use rusqlite::Row;
use serde_json::{json, Value};

use crate::hashing::create_hash;
use crate::parsers::{
    clean_title, get_base_title_and_versions, parse_feat_from_title, remove_prod,
    split_artists as split_artists_raw,
};

// ---------------------------------------------------------------------------
// UserConfig — subset of swingmusic/config.py UserConfig with verbatim defaults
// ---------------------------------------------------------------------------

/// settings.json `function` map: title/album/artist substitutions (title.py
/// apply_parenthesis_functions + replace_titles). Empty in every live env seen.
pub type TitleFunctions = std::collections::HashMap<String, String>;

pub struct UserConfig {
    pub extract_featured_artists: bool,
    pub remove_prod_by: bool,
    pub remove_remaster_info: bool,
    pub merge_albums: bool,
    pub clean_album_title: bool,
    pub show_albums_as_singles: bool,
    pub artist_article_aware_sorting: bool,
    pub artist_separators: HashSet<String>,
    pub genre_separators: HashSet<String>,
    pub artist_split_ignore_list: Vec<String>,
}

impl Default for UserConfig {
    fn default() -> Self {
        UserConfig {
            extract_featured_artists: true,
            remove_prod_by: true,
            remove_remaster_info: true,
            merge_albums: false,
            clean_album_title: true,
            show_albums_as_singles: false,
            artist_article_aware_sorting: false,
            artist_separators: [";", "/"].iter().map(|s| s.to_string()).collect(),
            genre_separators: ["/", ";", "&"].iter().map(|s| s.to_string()).collect(),
            artist_split_ignore_list: default_artist_ignore_list(),
        }
    }
}

/// Mirror of data/__init__.py default artist split ignore list (live read 2026-11-18).
pub fn default_artist_ignore_list() -> Vec<String> {
    [
        "AC/DC",
        "Sonny & Cher",
        "Bob marley & the wailers",
        "Crosby, Stills, Nash & Young",
        "Booker T. & the M.G.'s",
        "Maurice Williams & The Zodiacs",
        "Frank DeVol and His Orchestra",
        "Tommy James & the Shondells",
        "Smith & Thell",
        "Peter, Paul & Mary",
        "Simon & Garfunkel",
        "Judy & Mary",
        "Florence & The Machine",
        "Belle & Sebastian",
        "C&C Music Factory",
        "C & C Music Factory",
        "FO&O",
        "The Product G&B",
        "Big Brother & the Holding Company",
        "Tyler, The Creator",
        "Eric B. & Rakim",
        "DJ Jazzy Jeff & The Fresh Prince",
        "Rob Base & DJ E-Z Rock",
        "Nick Cave & the Bad Seeds",
        "For King + Country",
        "For King & Country",
        "Robson & Jerome",
        "Charles & Eddie",
        "Womack & Womack",
        "Pepsi & Shirlie",
        "Mel & Kim",
        "Wendy & Lisa",
        "Tony! Toni! Toné!",
        "Phillips, Craig & Dean",
        "Crosby & Nash",
        "Blood, Sweat & Tears",
        "Mike + The Mechanics",
        "Earth, Wind & Fire",
        "Emerson, Lake & Palmer",
        "Mumford & Sons",
        "Hall & Oates",
        "Tom Petty & The Heartbreakers",
        "Sly & the Family Stone",
        "KC & the Sunshine Band",
        "Huey Lewis & the News",
        "Joan Jett & the Blackhearts",
        "Echo & the Bunnymen",
        "Katrina & the Waves",
        "Gladys Knight & the Pips",
        "Martha & the Vandellas",
        "Martha Reeves and the Vandellas",
        "? and the Mysterians",
        "Josie & the Pussycats",
        "Dan + Shay",
        "Brooks & Dunn",
        "Peaches & Herb",
        "Seals & Crofts",
        "Loggins & Messina",
        "England Dan & John Ford Coley",
        "Ashford & Simpson",
        "Sam & Dave",
        "Ike & Tina Turner",
        "Captain & Tennille",
        "Hootie & the Blowfish",
        "Diana Ross & the Supremes",
        "Herb Alpert & the Tijuana Brass",
        "The Mamas & the Papas",
        "Gerry & the Pacemakers",
        "Chloe x Halle",
        "Durand Jones & The Indications",
        "St. Paul & The Broken Bones",
        "Nathaniel Rateliff & The Night Sweats",
        "Aly & AJ",
        "Maddie & Tae",
        "Nico & Vinz",
        "Yusuf / Cat Stevens",
        "Question Mark & the MysteriansKool & the Gang",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ---------------------------------------------------------------------------
// shared value helpers
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ArtistRef {
    pub name: String,
    pub artisthash: String,
    /// Live marker: `create_artists()` (lib/tagger.py L237-241) mutates the
    /// shared track dicts — albumartists entries that are NOT present in the
    /// same track's `artists` list (exact dict equality) get `in_track=false`.
    /// The marker persists on the store objects, so every response that
    /// serializes the store track/album afterwards carries it. Fresh
    /// TrackTable reads (db path) never carry it. `None` = key absent.
    pub in_track: Option<bool>,
}

impl ArtistRef {
    pub fn to_value(&self) -> Value {
        match self.in_track {
            Some(v) => json!({
                "name": self.name,
                "artisthash": self.artisthash,
                "in_track": v,
            }),
            None => json!({ "name": self.name, "artisthash": self.artisthash }),
        }
    }
}

#[derive(Clone)]
pub struct GenreRef {
    pub name: String,
    pub genrehash: String,
}

impl GenreRef {
    pub fn to_value(&self) -> Value {
        json!({ "name": self.name, "genrehash": self.genrehash })
    }
}

/// Python ints serialize as `0` while f64 `0.0` serializes as `0.0` — keep parity
/// by emitting integral floats as integers (`_score` is int 0 in golden bodies).
fn num(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

fn folder_slash(folder: &str) -> String {
    if folder.ends_with('/') {
        folder.to_string()
    } else {
        format!("{folder}/")
    }
}

/// Port of track.py explicit-tag parsing (L90–L97):
/// list → first item (or "0" when empty); string → lowercase in ("1","yes","true");
/// anything else → Python truthiness (bool()).
fn parse_explicit_tag(v: Option<&Value>) -> bool {
    let fallback = json!("0");
    let tag: &Value = match v {
        Some(Value::Array(a)) => a.first().unwrap_or(&fallback),
        Some(other) => other,
        None => &fallback,
    };
    match tag {
        Value::String(s) => {
            let l = s.to_lowercase();
            l == "1" || l == "yes" || l == "true"
        }
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Null => false,
    }
}

// raw sqlite column readers — live db mixes INTEGER/REAL/NULL, coerce safely
fn as_i64(row: &Row, idx: usize) -> i64 {
    match row.get_ref(idx) {
        Ok(ValueRef::Integer(i)) => i,
        Ok(ValueRef::Real(f)) => f as i64,
        _ => 0,
    }
}

fn as_f64(row: &Row, idx: usize) -> f64 {
    match row.get_ref(idx) {
        Ok(ValueRef::Integer(i)) => i as f64,
        Ok(ValueRef::Real(f)) => f,
        _ => 0.0,
    }
}

fn as_str(row: &Row, idx: usize) -> String {
    match row.get_ref(idx) {
        Ok(ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Ok(ValueRef::Integer(i)) => i.to_string(),
        Ok(ValueRef::Real(f)) => f.to_string(),
        _ => String::new(),
    }
}

fn as_json(row: &Row, idx: usize) -> Value {
    match row.get_ref(idx) {
        Ok(ValueRef::Text(t)) => serde_json::from_slice::<Value>(t).unwrap_or_else(|_| json!({})),
        _ => json!({}),
    }
}

// ---------------------------------------------------------------------------
// album _score registry — live shared-store semantics
// ---------------------------------------------------------------------------

use std::collections::HashMap as StdHashMap;
use std::sync::{Mutex as StdMutex, OnceLock};

/// Process-global albumhash → `_score` map.
///
/// Live Python mutates the SHARED Album store objects in
/// SearchAlbums.__call__ (`album._score = item[1]`), and the album serializer
/// keeps `_score`. The mutation therefore persists: any later response that
/// re-serializes the same album (artist pages, /search/top fill, getall)
/// sees the retained score — verified against the wave-2b gold (search_albums_tl
/// writes 100.0, the later artist_albums/info pages read it back; the other
/// single on the same page keeps 0). Track/artist serializers strip `_score`,
/// so only album serializations consult this registry.
static ALBUM_SCORES: OnceLock<StdMutex<StdHashMap<String, f64>>> = OnceLock::new();

fn album_scores() -> &'static StdMutex<StdHashMap<String, f64>> {
    ALBUM_SCORES.get_or_init(|| StdMutex::new(StdHashMap::new()))
}

/// Write-through for a search result set (live `album._score = item[1]`).
pub fn record_album_scores(entries: &[(String, f64)]) {
    if let Ok(mut m) = album_scores().lock() {
        for (h, s) in entries {
            m.insert(h.clone(), *s);
        }
    }
}

/// Read-side for serializers. `None` = never scored (live default
/// `_score: float = 0` — serialized as int `0`).
pub fn album_score_for(albumhash: &str) -> Option<f64> {
    album_scores()
        .lock()
        .ok()
        .and_then(|m| m.get(albumhash).copied())
}

/// Serializer renderer for `_score` with float parity: absent → int `0`,
/// scored → the float repr (`50.0`, `100.0`, `88.88888888888889`) — Python
/// keeps the rapidfuzz float, never an int.
fn score_value(v: Option<f64>) -> Value {
    match v {
        Some(x) => json!(x),
        None => json!(0),
    }
}

// ---------------------------------------------------------------------------
// Track
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Track {
    pub track_id: i64,
    pub album: String,
    pub albumartists: Vec<ArtistRef>,
    pub albumhash: String,
    pub artists: Vec<ArtistRef>,
    pub bitrate: i64,
    pub copyright: String,
    pub date: i64,
    pub disc: i64,
    pub duration: i64,
    pub filepath: String,
    pub folder: String,
    pub genres: Vec<GenreRef>,
    pub last_mod: f64,
    pub title: String,
    pub track: i64,
    pub trackhash: String,
    pub lastplayed: i64,
    pub playcount: i64,
    pub playduration: i64,
    pub extra: Value,
    // derived / post-init
    pub og_title: String,
    pub og_album: String,
    pub artisthashes: Vec<String>,
    pub genrehashes: Vec<String>,
    pub weakhash: String,
    pub pathhash: String,
    pub explicit: bool,
    // post-init fields — live track.py L44/L47/L54/L57:
    //   image       = albumhash + ".webp?pathhash=" + pathhash
    //   color/blurhash are hydrated from the color cache (lib/mapstuff.py)
    //   fav_userids comes from userdata.db favorites
    pub image: String,
    pub color: String,
    pub blurhash: String,
    pub fav_userids: Vec<i64>,
    // raw db strings kept for reprocessing (not serialized)
    pub artists_raw: String,
    pub albumartists_raw: String,
    pub genres_raw: String,
}

impl Track {
    /// `Track(*row[1:], config=config)` — positional column read, then __post_init__.
    pub fn from_row(
        row: &Row,
        cfg: &UserConfig,
        fns: &TitleFunctions,
    ) -> rusqlite::Result<Self> {
        let mut t = Track {
            track_id: as_i64(row, 0),
            album: as_str(row, 1),
            albumartists_raw: as_str(row, 2),
            albumhash: as_str(row, 3),
            artists_raw: as_str(row, 4),
            bitrate: as_i64(row, 5),
            copyright: as_str(row, 6),
            date: as_i64(row, 7),
            disc: as_i64(row, 8),
            duration: as_i64(row, 9),
            filepath: as_str(row, 10),
            folder: as_str(row, 11),
            genres_raw: as_str(row, 12),
            last_mod: as_f64(row, 13),
            title: as_str(row, 14),
            track: as_i64(row, 15),
            trackhash: as_str(row, 16),
            lastplayed: as_i64(row, 17),
            playcount: as_i64(row, 18),
            playduration: as_i64(row, 19),
            extra: as_json(row, 20),
            albumartists: Vec::new(),
            artists: Vec::new(),
            genres: Vec::new(),
            og_title: String::new(),
            og_album: String::new(),
            artisthashes: Vec::new(),
            genrehashes: Vec::new(),
            weakhash: String::new(),
            pathhash: String::new(),
            explicit: false,
            image: String::new(),
            color: String::new(),
            blurhash: String::new(),
            fav_userids: Vec::new(),
        };
        t.post_init(cfg);
        t.apply_title_functions(fns);
        Ok(t)
    }

    /// Port of lib/title.py application order: parenthesis functions first
    /// (verbatim str.replace on the title), then whole/inner-text replacements
    /// for title (`inner_text` flips the containment rule), album and artists.
    /// Any title change recreates the trackhash (live `__update_trackhash__`).
    pub fn apply_title_functions(&mut self, fns: &TitleFunctions) {
        if fns.is_empty() {
            return;
        }
        let mut title = self.title.clone();

        // 1. parenthesis functions: replace the literal paren chunk
        for (find, replace) in fns {
            if find.starts_with('(') {
                title = title.replace(find.as_str(), replace);
            }
        }

        // 2. title replacements (whole title OR inner text), album, artists
        let mut album = self.album.clone();
        let mut artists_changed = false;
        for (find, replace) in fns {
            if find.starts_with('(') {
                continue;
            }
            match find.as_str() {
                "album_title" => album = album.replace("album_title", replace),
                "album_artist" => {
                    for a in self.albumartists.iter_mut() {
                        if a.name == "album_artist" {
                            a.name = replace.clone();
                            artists_changed = true;
                        }
                    }
                }
                _ => {
                    let inner = fns
                        .get(&format!("{find}_inner_text"))
                        .map(|s| s == "true")
                        .unwrap_or(false);
                    if inner {
                        if title.contains(find.as_str()) {
                            title = title.replace(find.as_str(), replace);
                        }
                    } else if title.trim() == find.trim() {
                        title = replace.clone();
                    }
                }
            }
        }

        if title != self.title {
            self.title = title;
            self.recreate_trackhash();
        }
        if album != self.album {
            self.album = album;
            self.recreate_trackhash();
        }
        if artists_changed {
            self.recreate_trackhash();
        }
    }

    /// Port of models/track.py __post_init__ (verbatim order, L85–L128):
    /// og_title/og_album → folder += "/" → weakhash(raw title, raw artists string)
    /// → explicit tag parse → pathhash/image → split → map → genres → dedup → trackhash.
    pub fn post_init(&mut self, cfg: &UserConfig) {
        self.og_title = self.title.clone();
        self.og_album = self.album.clone();
        // live L87: self.folder = self.folder + "/"
        self.folder = format!("{}/", self.folder);
        // live L88: create_hash(self.title, self.artists) — RAW title + RAW artists
        // string, BEFORE split/map (rna() concatenation makes arg-splat and
        // raw-string byte-identical, but the title must still be unmapped here).
        self.weakhash = create_hash(&[self.title.as_str(), self.artists_raw.as_str()], false);
        self.explicit = parse_explicit_tag(self.extra.get("explicit"));
        // live L99: image = albumhash + ".webp" + "?pathhash=" + pathhash
        // (pathhash = create_hash(folder) with the trailing slash attached above).
        self.pathhash = create_hash(&[self.folder.as_str()], false);
        self.image = format!("{}.webp?pathhash={}", self.albumhash, self.pathhash);

        self.split_artists(cfg);
        self.map_with_config(cfg);
        self.process_genres(cfg);

        // live L110–L125: drop duplicate artist dicts (order-preserving, first wins).
        {
            let mut seen: HashSet<(String, String)> = HashSet::new();
            self.artists
                .retain(|a| seen.insert((a.name.clone(), a.artisthash.clone())));
            let mut seen_aa: HashSet<(String, String)> = HashSet::new();
            self.albumartists
                .retain(|a| seen_aa.insert((a.name.clone(), a.artisthash.clone())));
        }

        self.recreate_trackhash();
    }

    /// Port of Track.split_artists() — hashes use decode=True.
    pub fn split_artists(&mut self, cfg: &UserConfig) {
        fn split(src: &str, cfg: &UserConfig) -> Vec<ArtistRef> {
            split_artists_raw(src, &cfg.artist_separators, &cfg.artist_split_ignore_list)
                .into_iter()
                .map(|a| ArtistRef {
                    artisthash: create_hash(&[a.as_str()], true),
                    name: a,
                    in_track: None,
                })
                .collect()
        }
        let artists_src = self.artists_raw.clone();
        let albumartists_src = self.albumartists_raw.clone();
        self.artists = split(&artists_src, cfg);
        self.albumartists = split(&albumartists_src, cfg);
        self.artisthashes = self.artists.iter().map(|a| a.artisthash.clone()).collect();
    }

    /// Port of Track.map_with_config().
    pub fn map_with_config(&mut self, cfg: &UserConfig) {
        let mut new_title = self.title.clone();

        if cfg.extract_featured_artists {
            let (feat, t) =
                parse_feat_from_title(&self.title, &cfg.artist_separators, &cfg.artist_split_ignore_list);
            new_title = t;

            let snapshot: HashSet<String> = self.artisthashes.iter().cloned().collect();
            let mut new_hashes: Vec<String> = Vec::new();
            for f in feat {
                let h = create_hash(&[f.as_str()], true);
                if !snapshot.contains(&h) {
                    self.artists.push(ArtistRef {
                        name: f,
                        artisthash: h.clone(),
                        in_track: None,
                    });
                    new_hashes.push(h);
                }
            }
            self.artisthashes.extend(new_hashes);

            // singles whose album == title take the cleaned title as album
            if self.og_album == self.og_title {
                self.album = new_title.clone();
            }
        }

        if cfg.remove_prod_by {
            new_title = remove_prod(&new_title);
        }
        if cfg.remove_remaster_info {
            new_title = clean_title(&new_title);
        }
        self.title = new_title;

        if cfg.clean_album_title {
            self.album = get_base_title_and_versions(&self.album, false).0;
        }

        if cfg.merge_albums {
            let mut args: Vec<&str> = vec![self.album.as_str()];
            for a in &self.albumartists {
                args.push(a.name.as_str());
            }
            self.albumhash = create_hash(&args, false);
        }
    }

    /// Port of Track.process_genres() — genrehashes use decode=False, names stripped.
    pub fn process_genres(&mut self, cfg: &UserConfig) {
        if self.genres_raw.is_empty() {
            return;
        }
        let mut src = self.genres_raw.to_lowercase();

        if src.contains("r&b") {
            src = src.replace("r&b", "RnB");
        }
        if src.contains("rock & roll") {
            src = src.replace("rock & roll", "rock");
        }
        for s in &cfg.genre_separators {
            src = src.replace(s.as_str(), ",");
        }

        self.genres = src
            .split(',')
            .map(|g| {
                let name = g.trim();
                GenreRef {
                    name: name.to_string(),
                    genrehash: create_hash(&[name], false),
                }
            })
            .collect();
        self.genrehashes = self.genres.iter().map(|g| g.genrehash.clone()).collect();
    }

    /// Port of Track.recreate_trackhash().
    pub fn recreate_trackhash(&mut self) {
        let mut args: Vec<&str> = vec![self.title.as_str(), self.album.as_str()];
        for a in &self.artists {
            args.push(a.name.as_str());
        }
        self.trackhash = create_hash(&args, false);
    }

    pub fn weakhash_for(title: &str, artists: &[ArtistRef]) -> String {
        let mut args: Vec<&str> = vec![title];
        for a in artists {
            args.push(a.name.as_str());
        }
        create_hash(&args, false)
    }

    /// Port of serializers/track.py serialize_track() — the 18-key golden set
    /// (gold album_tracks_item0) plus `disc`/`track` when remove_disc=false
    /// (album_info track items carry them). Keys removed by the live props set:
    /// copyright, date, last_mod, og_title, og_album, playcount, playduration,
    /// lastplayed, genres, genrehashes, fav_userids, id, created_date, _pos,
    /// _ati, _score, config, pathhash, track_id.
    pub fn to_json(&self, userid: i64, remove_disc: bool) -> Value {
        let mut d = json!({
            "album": self.album,
            "albumartists": self.albumartists.iter().map(|a| a.to_value()).collect::<Vec<_>>(),
            "albumhash": self.albumhash,
            "artists": self.artists.iter().map(|a| a.to_value()).collect::<Vec<_>>(),
            "artisthashes": self.artisthashes,
            "bitrate": self.bitrate,
            "blurhash": self.blurhash,
            "color": self.color,
            "duration": self.duration,
            "explicit": self.explicit,
            "extra": self.extra,
            "filepath": self.filepath,
            "folder": self.folder,
            "image": self.image,
            "is_favorite": self.fav_userids.contains(&userid),
            "title": self.title,
            "trackhash": self.trackhash,
            "weakhash": self.weakhash,
        });
        if !remove_disc {
            if let Value::Object(ref mut m) = d {
                m.insert("disc".to_string(), json!(self.disc));
                m.insert("track".to_string(), json!(self.track));
            }
        }
        d
    }
}

// ---------------------------------------------------------------------------
// Album
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Album {
    pub albumhash: String,
    pub albumartists: Vec<ArtistRef>,
    pub artisthashes: Vec<String>,
    pub base_title: String,
    pub created_date: i64,
    pub date: i64,
    pub duration: i64,
    pub genres: Vec<GenreRef>,
    /// STRING (upstream bug kept 1:1): " ".join(genrehashes)
    pub genrehashes: String,
    pub image: String,
    pub lastplayed: i64,
    pub og_title: String,
    pub pathhash: String,
    pub playcount: i64,
    pub playduration: i64,
    pub title: String,
    pub trackcount: i64,
    pub trackhashes: Vec<String>,
    pub r#type: String,
    pub versions: Vec<String>,
    pub weakhash: String,
    pub color: Option<String>,
    pub blurhash: String,
    pub extra: Value,
    pub id: i64,
    pub fav_userids: Vec<i64>,
    /// search relevance score (upstream `_score`); stays 0 outside search flows.
    pub score: f64,
    /// raw genres string kept for post-hoc grouping (upstream `genres_raw`),
    /// recomputed on load + on the live color-info pass.
    pub genres_raw: String,
}

impl Album {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        albumhash: String,
        albumartists: Vec<ArtistRef>,
        artisthashes: Vec<String>,
        base_title: String,
        created_date: i64,
        date: i64,
        duration: i64,
        genres: Vec<GenreRef>,
        lastplayed: i64,
        og_title: String,
        pathhash: String,
        playcount: i64,
        playduration: i64,
        title: String,
        trackhashes: Vec<String>,
        genres_raw: String,
    ) -> Album {
        let image = format!("{}.webp?pathhash={}", albumhash, pathhash);
        let weakhash = {
            let mut args: Vec<&str> = vec![og_title.as_str()];
            for a in &albumartists {
                args.push(a.name.as_str());
            }
            create_hash(&args, false)
        };
        let mut a = Album {
            albumhash,
            albumartists,
            artisthashes,
            base_title,
            created_date,
            date,
            duration,
            genres,
            genrehashes: String::new(),
            image,
            lastplayed,
            og_title,
            pathhash,
            playcount,
            playduration,
            title,
            trackcount: trackhashes.len() as i64,
            trackhashes,
            r#type: String::new(),
            versions: Vec::new(),
            weakhash,
            color: None,
            blurhash: String::new(),
            extra: json!({}),
            id: -1,
            fav_userids: Vec::new(),
            score: 0.0,
            genres_raw,
        };
        a.populate_versions();
        a
    }

    /// Initial album entry from the first track of the group (albumslib.py).
    pub fn from_track(t: &Track) -> Album {
        let artisthashes = t
            .albumartists
            .iter()
            .map(|a| a.artisthash.clone())
            .collect();
        let base_title = crate::parsers::get_base_album_title(&t.album).0;
        Album::new(
            t.albumhash.clone(),
            t.albumartists.clone(),
            artisthashes,
            base_title,
            t.last_mod as i64,
            t.date,
            t.duration,
            t.genres.clone(),
            t.lastplayed,
            t.og_album.clone(),
            t.pathhash.clone(),
            t.playcount,
            t.playduration,
            t.album.clone(),
            vec![t.trackhash.clone()],
            t.genres_raw.clone(),
        )
    }

    /// Live color-info hydration (lib/mapstuff.py MapStuff.update_album_data):
    /// re-parses `genres_raw` via the configured separators, resolves genre
    /// names (genrelist), then sets color/blurhash from the artistdata row.
    pub fn hydrate_color_info(
        &mut self,
        color: String,
        blurhash: Option<String>,
        genrelist: Option<&std::collections::HashMap<String, String>>,
        separators: &std::collections::HashSet<String>,
    ) {
        if let Some(list) = genrelist {
            if !self.genres_raw.is_empty() {
                let mut parts: Vec<String> =
                    self.genres_raw.split(',').map(|s| s.trim().to_string()).collect();
                for s in separators {
                    parts = parts
                        .iter()
                        .flat_map(|p| p.split(s.as_str()).map(|x| x.trim().to_string()))
                        .collect();
                }
                let mut seen: Vec<String> = Vec::new();
                self.genres = parts
                    .iter()
                    .filter(|g| !g.is_empty())
                    .map(|g| {
                        let name = list.get(g.as_str()).cloned().unwrap_or_else(|| g.clone());
                        crate::models::GenreRef {
                            genrehash: crate::hashing::create_hash(&[g.as_str()], false),
                            name,
                        }
                    })
                    .filter(|gr| {
                        if seen.contains(&gr.genrehash) {
                            false
                        } else {
                            seen.push(gr.genrehash.clone());
                            true
                        }
                    })
                    .collect();
            }
        }
        self.color = Some(color);
        self.blurhash = blurhash.unwrap_or_default();
    }

    /// Port of Album.populate_versions() — idempotent (recomputed from og_title).
    pub fn populate_versions(&mut self) {
        let (_, mut versions) = get_base_title_and_versions(&self.og_title, true);
        if versions.iter().any(|v| v == "super_deluxe") {
            versions.retain(|v| v != "deluxe");
        }
        if versions.iter().any(|v| v == "original") && self.r#type == "soundtrack" {
            versions.retain(|v| v != "original");
        }
        self.versions = versions.iter().map(|v| v.replace('_', " ")).collect();
    }

    /// Port of Album.check_type() — single → soundtrack → live → compilation → ep.
    pub fn check_type(&mut self, tracks: &[Track], single_track_as_single: bool) {
        if self.is_single(tracks, single_track_as_single) {
            self.r#type = "single".to_string();
        } else if self.is_soundtrack() {
            self.r#type = "soundtrack".to_string();
        } else if self.is_live_album() {
            self.r#type = "live album".to_string();
        } else if self.is_compilation() {
            self.r#type = "compilation".to_string();
        } else if self.is_ep() {
            self.r#type = "ep".to_string();
        } else {
            self.r#type = "album".to_string();
        }
        self.populate_versions();
    }

    pub fn is_soundtrack(&self) -> bool {
        let title = self.og_title.to_lowercase();
        for keyword in ["motion picture", "soundtrack"] {
            if title.contains(keyword) {
                return true;
            }
        }
        if title.len() > 10 && title.ends_with("the album") {
            return true;
        }
        false
    }

    pub fn is_compilation(&self) -> bool {
        let artists: String = self
            .albumartists
            .iter()
            .map(|a| a.name.clone())
            .collect::<String>()
            .to_lowercase();
        if artists.contains("various artists") {
            return true;
        }
        let substrings = [
            "the essential",
            "best of",
            "greatest hits",
            "#1 hits",
            "number ones",
            "super hits",
            "collection",
            "anthology",
            "great hits",
            "biggest hits",
            "the hits",
            "the ultimate",
            "compilation",
        ];
        let title = self.title.to_lowercase();
        for substring in substrings {
            if title.contains(substring) {
                return true;
            }
        }
        false
    }

    pub fn is_live_album(&self) -> bool {
        let og = self.og_title.to_lowercase();
        for keyword in ["live from", "live at", "live in", "live on", "mtv unplugged"] {
            if og.contains(keyword) {
                return true;
            }
        }
        false
    }

    pub fn is_ep(&self) -> bool {
        // live verbatim: self.title.strip().endswith(" EP")
        self.title.trim().ends_with(" EP")
    }

    /// Port of Album.is_single() — NO explicit false branch upstream; the hash
    /// comparison result is returned directly and falling off the end is falsy.
    pub fn is_single(&self, tracks: &[Track], single_track_as_single: bool) -> bool {
        for keyword in ["single version", "- single"] {
            if self.og_title.to_lowercase().contains(keyword) {
                return true;
            }
        }
        if single_track_as_single && self.trackcount == 1 {
            return true;
        }
        if tracks.len() == 1 {
            let t = &tracks[0];
            let h = create_hash(&[t.title.as_str()], false);
            return h == create_hash(&[self.title.as_str()], false)
                || h == create_hash(&[self.og_title.as_str()], false);
        }
        false
    }

    pub fn to_json(&self, userid: i64) -> Value {
        json!({
            "albumhash": self.albumhash,
            "albumartists": self.albumartists.iter().map(|a| a.to_value()).collect::<Vec<_>>(),
            "artisthashes": self.artisthashes,
            "base_title": self.base_title,
            "blurhash": self.blurhash,
            "color": self.color,
            "created_date": self.created_date,
            "date": self.date,
            "duration": self.duration,
            "extra": self.extra,
            "fav_userids": self.fav_userids,
            "genrehashes": self.genrehashes,
            "genres": self.genres.iter().map(|g| g.to_value()).collect::<Vec<_>>(),
            "id": self.id,
            "image": self.image,
            "is_favorite": self.fav_userids.contains(&userid),
            "lastplayed": self.lastplayed,
            "og_title": self.og_title,
            "pathhash": self.pathhash,
            "playcount": self.playcount,
            "playduration": self.playduration,
            "title": self.title,
            "trackcount": self.trackcount,
            "type": self.r#type,
            "versions": self.versions,
            "weakhash": self.weakhash,
            "_score": score_value(album_score_for(&self.albumhash)),
        })
    }
}

// ---------------------------------------------------------------------------
// Artist / ArtistMinimal / Folder
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ArtistMinimal {
    pub name: String,
    pub artisthash: String,
    pub image: String,
}

impl ArtistMinimal {
    pub fn new(name: &str) -> ArtistMinimal {
        let artisthash = create_hash(&[name], true);
        let mut name = name.to_string();
        // hack upstream keeps for unreleased files
        if artisthash == "5a37d5315e" {
            name = "Juice WRLD".to_string();
        }
        let image = format!("{}.webp", artisthash);
        ArtistMinimal {
            name,
            artisthash,
            image,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "name": self.name, "artisthash": self.artisthash })
    }
}

#[derive(Clone)]
pub struct Artist {
    pub name: String,
    pub albumcount: i64,
    pub artisthash: String,
    pub created_date: i64,
    pub date: i64,
    pub duration: i64,
    pub genres: Vec<GenreRef>,
    pub genrehashes: Vec<String>,
    pub trackcount: i64,
    pub lastplayed: i64,
    pub playcount: i64,
    pub playduration: i64,
    pub extra: Value,
    pub id: i64,
    pub image: String,
    pub color: String,
    pub blurhash: String,
    pub fav_userids: Vec<i64>,
    pub score: f64,
    // aggregation helpers (not serialized)
    pub names: Vec<String>,
    pub albumhashes: Vec<String>,
    pub trackhashes: Vec<String>,
}

impl Artist {
    pub fn from_track(t: &Track, ar: &ArtistRef) -> Artist {
        Artist {
            name: ar.name.clone(),
            names: vec![ar.name.clone()],
            artisthash: ar.artisthash.clone(),
            albumcount: 0,
            albumhashes: Vec::new(),
            created_date: t.last_mod as i64,
            date: t.date,
            duration: 0,
            genres: Vec::new(),
            genrehashes: Vec::new(),
            trackcount: 0,
            trackhashes: Vec::new(),
            lastplayed: 0,
            playcount: 0,
            playduration: 0,
            extra: json!({}),
            id: -1,
            image: format!("{}.webp", ar.artisthash),
            color: String::new(),
            blurhash: String::new(),
            fav_userids: Vec::new(),
            score: 0.0,
        }
    }

    pub fn to_json(&self, userid: i64) -> Value {
        json!({
            "name": self.name,
            "albumcount": self.albumcount,
            "artisthash": self.artisthash,
            "created_date": self.created_date,
            "date": self.date,
            "duration": self.duration,
            "genres": self.genres.iter().map(|g| g.to_value()).collect::<Vec<_>>(),
            "genrehashes": self.genrehashes,
            "trackcount": self.trackcount,
            "lastplayed": self.lastplayed,
            "playcount": self.playcount,
            "playduration": self.playduration,
            "extra": self.extra,
            "id": self.id,
            "image": self.image,
            "color": self.color,
            "blurhash": self.blurhash,
            "fav_userids": self.fav_userids,
            "is_favorite": self.fav_userids.contains(&userid),
            "_score": num(self.score),
        })
    }
}

#[derive(Clone)]
pub struct Folder {
    pub name: String,
    pub path: String,
    pub is_sym: bool,
    pub trackcount: i64,
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn t_fixture(title: &str, album: &str, artists: &str, folder: &str) -> Track {
        let mut t = Track {
            track_id: -1,
            album: album.to_string(),
            albumartists: Vec::new(),
            albumhash: "albumhash".to_string(),
            artists: Vec::new(),
            bitrate: 0,
            copyright: String::new(),
            date: 0,
            disc: 0,
            duration: 0,
            filepath: String::new(),
            folder: folder.to_string(),
            genres: Vec::new(),
            last_mod: 0.0,
            title: title.to_string(),
            track: 0,
            trackhash: String::new(),
            lastplayed: 0,
            playcount: 0,
            playduration: 0,
            extra: json!({}),
            og_title: String::new(),
            og_album: String::new(),
            artisthashes: Vec::new(),
            genrehashes: Vec::new(),
            weakhash: String::new(),
            pathhash: String::new(),
            explicit: false,
            image: String::new(),
            color: String::new(),
            blurhash: String::new(),
            fav_userids: Vec::new(),
            artists_raw: artists.to_string(),
            albumartists_raw: artists.to_string(),
            genres_raw: String::new(),
        };
        t.post_init(&UserConfig::default());
        t
    }

    #[test]
    fn artist_minimal_tate_vector() {
        // golden artisthash from live /getall/artists
        let a = ArtistMinimal::new("Tate McRae");
        assert_eq!(a.artisthash, "442538f77201bed3");
        assert_eq!(a.image, "442538f77201bed3.webp");
    }

    #[test]
    fn feat_artist_extracted_and_title_cleaned() {
        let t = t_fixture("Song (feat. Guest)", "Album", "Artist", "/music/a");
        // live verbatim: re.sub(r"\((?:feat|ft|featuring|with)\.?\s+(.+?)\)", "", title)
        // removes only the bracketed group — the space before it stays ("Song ").
        // Verified empirically against livesrc/utils/parsers.py.
        assert_eq!(t.title, "Song ");
        assert!(t.artists.iter().any(|a| a.name == "Guest"));
        assert!(t.artisthashes.len() >= 2);
    }

    #[test]
    fn pathhash_equals_folder_hash() {
        let t = t_fixture("Song", "Album", "Artist", "/music/a");
        assert_eq!(t.pathhash, create_hash(&["musica"], false));
    }

    #[test]
    fn genres_split_and_rnb_kept_single() {
        let mut t = t_fixture("Song", "Album", "Artist", "/music/a");
        t.genres_raw = "Pop; Rock & Roll".to_string();
        t.process_genres(&UserConfig::default());
        let names: Vec<&str> = t.genres.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["pop", "rock"]);
    }

    #[test]
    fn album_type_and_ep_detection() {
        let mut t = t_fixture("Song", "Greatest Hits", "Artist", "/music/a");
        t.albumhash = "h".to_string();
        let mut alb = Album::from_track(&t);
        let tracks = vec![t.clone(), t_fixture("Other", "Greatest Hits", "Artist", "/music/a")];
        alb.check_type(&tracks, false);
        assert_eq!(alb.r#type, "compilation");
        assert!(alb.image.contains("?pathhash="));
        assert!(!alb.weakhash.is_empty());
    }

    #[test]
    fn is_ep_suffix() {
        let mut t = t_fixture("Song", "My EP", "Artist", "/music/a");
        t.album = "My EP".to_string();
        let alb = Album::from_track(&t);
        assert!(alb.is_ep());
    }

    #[test]
    fn to_json_matches_golden_18_keys() {
        // gold: /tmp/goldtxt/album_tracks_item0.txt — 18 keys, remove_disc=true
        let t = t_fixture("Run for the Hills", "THINK LATER", "Tate McRae", "/music/music");
        let v = t.to_json(1, true);
        let mut got: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        got.sort();
        let want = vec![
            "album", "albumartists", "albumhash", "artisthashes", "artists", "bitrate",
            "blurhash", "color", "duration", "explicit", "extra", "filepath", "folder",
            "image", "is_favorite", "title", "trackhash", "weakhash",
        ];
        assert_eq!(got, want);
        // folder carries the trailing slash (post_init mutation)
        assert_eq!(v["folder"], "/music/music/");
        // image = albumhash + ".webp?pathhash=" + pathhash
        assert_eq!(
            v["image"],
            format!("{}.webp?pathhash={}", t.albumhash, t.pathhash)
        );
        // remove_disc=false adds disc+track (album_info track items)
        let v2 = t.to_json(1, false);
        assert!(v2.get("disc").is_some());
        assert!(v2.get("track").is_some());
        assert_eq!(v2.as_object().unwrap().len(), 20);
    }
}
