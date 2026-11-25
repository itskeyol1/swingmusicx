//! gel.2c — `/nothome` routes (homepage items) + HomepageStore.
//!
//! Port 1:1 daripada sumber Python:
//!   api/home/__init__.py, store/homepage.py, store/homepageentries.py,
//!   lib/home/{__init__,get_recently_played,recentlyadded,recentlyplayed,
//!             create_items,recover_items}.py,
//!   lib/recipes/recents.py, lib/pagelib.py (recover_page_items).
//!
//! KONTRAK ROUTE (dikunci):
//!
//!   GET /nothome/recents/added?limit=N
//!       → {"items": get_recently_added_items(limit)}   (default API_CARD_LIMIT = 6)
//!   GET /nothome/recents/played?limit=N
//!       → {"items": get_recently_played(limit)}        (default API_CARD_LIMIT = 6)
//!   GET /nothome/?limit=N
//!       → HomepageStore.get_homepage_items(limit)      (default 9, array-of-entries)
use std::collections::HashMap;

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::getall::{now_ts, user_id, API_CARD_LIMIT};
use crate::models::{Artist, Track};
use crate::{AppState, CurrentUser};

/// Default limit untuk `/nothome/` (api/home/__init__.py `HomepageItem.limit`).
const HOMEPAGE_LIMIT: i64 = 9;
/// `RecentlyPlayed.ITEM_LIMIT` / `RecentlyAdded.ITEM_LIMIT` (lib/recipes/recents.py).
const RECENTS_ITEM_LIMIT: i64 = 15;

// ---------------------------------------------------------------------------
// Query schemas
// ---------------------------------------------------------------------------

/// `GenericLimitSchema` — limit int, default `Defaults.API_CARD_LIMIT`.
#[derive(Deserialize)]
pub struct GenericLimitQuery {
    #[serde(default = "default_api_card_limit")]
    pub limit: i64,
}

/// `HomepageItem` — limit int, default 9.
#[derive(Deserialize)]
pub struct HomepageQuery {
    #[serde(default = "default_homepage_limit")]
    pub limit: i64,
}

fn default_api_card_limit() -> i64 {
    API_CARD_LIMIT
}

fn default_homepage_limit() -> i64 {
    HOMEPAGE_LIMIT
}

/// Python `list[:limit]` (limit boleh negatif → slice dari hujung).
fn py_head<T: Clone>(items: &[T], limit: i64) -> Vec<T> {
    if limit < 0 {
        let keep = (items.len() as i64 + limit).max(0) as usize;
        return items[..keep].to_vec();
    }
    let take = (limit as usize).min(items.len());
    items[..take].to_vec()
}

// ---------------------------------------------------------------------------
// TrackLog — models/logger.py (`type`/`type_src` dari prefix `source`)
// ---------------------------------------------------------------------------

struct TrackLog {
    trackhash: String,
    timestamp: i64,
    /// `entry.source` penuh (cth "al:123456", "fo:/music/music/", "metrolist").
    /// Dedup dalam `create_items` berlaku pada medan INI, bukan trackhash.
    source: String,
    /// "track" | "album" | "artist" | "folder" | "playlist" | "favorite" | "mix"
    kind: &'static str,
    type_src: Option<String>,
}

impl TrackLog {
    /// `TrackLog.__post_init__` — prefix_map, padanan pertama menang.
    fn from_row(trackhash: String, timestamp: i64, source: &str) -> TrackLog {
        const PREFIXES: [(&str, &str); 6] = [
            ("mix:", "mix"),
            ("al:", "album"),
            ("ar:", "artist"),
            ("fo:", "folder"),
            ("pl:", "playlist"),
            ("favorite", "favorite"),
        ];
        for (prefix, kind) in PREFIXES {
            if let Some(rest) = source.strip_prefix(prefix) {
                let src = if prefix.ends_with(':') {
                    Some(rest.to_string())
                } else {
                    // "favorite" tanpa ":" → `source.split(":", 1)[1]` → IndexError → None
                    None
                };
                return TrackLog {
                    trackhash,
                    timestamp,
                    source: source.to_string(),
                    kind,
                    type_src: src,
                };
            }
        }
        TrackLog {
            trackhash,
            timestamp,
            source: source.to_string(),
            kind: "track",
            type_src: None,
        }
    }
}

/// `ScrobbleTable.get_all(start, limit, userid)` — satu halaman, `timestamp DESC`.
fn scrobble_page(
    conn: &rusqlite::Connection,
    start: i64,
    limit: i64,
    userid: i64,
) -> Vec<TrackLog> {
    let mut out = Vec::new();
    let mut stmt = match conn.prepare(
        "SELECT trackhash, timestamp, source FROM scrobble \
         WHERE userid = ?1 ORDER BY timestamp DESC LIMIT ?2 OFFSET ?3",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("home: scrobble query skipped ({e})");
            return out;
        }
    };
    let rows = stmt.query_map(rusqlite::params![userid, limit, start], |r| {
        let hash: String = r.get(0)?;
        let ts: i64 = r.get(1)?;
        let src: String = r.get::<_, Option<String>>(2)?.unwrap_or_default();
        Ok((hash, ts, src))
    });
    if let Ok(rows) = rows {
        for (hash, ts, src) in rows.flatten() {
            out.push(TrackLog::from_row(hash, ts, &src));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// RawItem — dict {"type","hash","timestamp", ...} dalam HomepageStore
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct RawItem {
    kind: String,
    hash: String,
    timestamp: f64,
    help_text: Option<String>,
    is_custom: bool,
}

impl RawItem {
    fn new(kind: &str, hash: String, timestamp: f64) -> RawItem {
        RawItem {
            kind: kind.to_string(),
            hash,
            timestamp,
            help_text: None,
            is_custom: false,
        }
    }

    /// Bentuk `/nothome/recents/*` dan payload mentah homepage sebelum
    /// `recover_items`. Kunci diisih (json respons disusun abjad).
    fn to_dict(&self) -> Value {
        let mut m = Map::new();
        m.insert("hash".to_string(), json!(self.hash));
        m.insert("timestamp".to_string(), num_json(self.timestamp));
        m.insert("type".to_string(), json!(self.kind));
        if let Some(h) = &self.help_text {
            if !h.is_empty() {
                m.insert("help_text".to_string(), json!(h));
            }
        }
        if self.is_custom {
            m.insert("is_custom".to_string(), json!(true));
        }
        Value::Object(m)
    }
}

/// Python `json.dumps`: int kekal int, float kekal `.0`.
fn num_json(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

fn contains_item(list: &[Value], item: &Value) -> bool {
    match item {
        Value::Array(xs) => xs.iter().all(|x| list.iter().any(|e| e == x)),
        _ => list.iter().any(|e| e == item),
    }
}

// ---------------------------------------------------------------------------
// Store indexes
// ---------------------------------------------------------------------------

/// `TrackStore.trackhashmap` → hash ke indeks `get_best()` (bitrate tertinggi).
fn track_index(tracks: &[Track]) -> HashMap<String, usize> {
    let mut map: HashMap<String, usize> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        match map.get(&t.trackhash) {
            Some(&j) if tracks[j].bitrate >= t.bitrate => {}
            _ => {
                map.insert(t.trackhash.clone(), i);
            }
        }
    }
    map
}

fn album_known(st: &AppState, albumhash: &str) -> bool {
    st.library.albums.iter().any(|a| a.albumhash == albumhash)
}

fn artist_known(st: &AppState, artisthash: &str) -> bool {
    st.library.artists.iter().any(|a| a.artisthash == artisthash)
}

// ---------------------------------------------------------------------------
// lib/home/get_recently_played.py → create_items()
// ---------------------------------------------------------------------------

fn create_items(
    entries: &[TrackLog],
    limit: i64,
    st: &AppState,
    idx: &HashMap<String, usize>,
) -> Vec<RawItem> {
    let mut items: Vec<RawItem> = Vec::new();
    // Python: `added = set()` — dedup pada `entry.source` (bukan trackhash).
    // Satu source boleh muncul berkali-kali (contoh "metrolist" 41×), jadi set
    // ini yang mengehadkan output; tanpanya item bertindih akan terlepas masuk.
    let mut added: Vec<&str> = Vec::new();

    for entry in entries {
        if items.len() as i64 >= limit {
            break;
        }
        if added.contains(&entry.source.as_str()) {
            continue;
        }
        added.push(&entry.source);

        match entry.kind {
            "track" => {
                if idx.contains_key(&entry.trackhash) {
                    items.push(RawItem::new(
                        "track",
                        entry.trackhash.clone(),
                        entry.timestamp as f64,
                    ));
                }
            }
            "album" => {
                if let Some(src) = &entry.type_src {
                    if album_known(st, src) {
                        items.push(RawItem::new("album", src.clone(), entry.timestamp as f64));
                    }
                }
            }
            "artist" => {
                if let Some(src) = &entry.type_src {
                    if artist_known(st, src) {
                        items.push(RawItem::new("artist", src.clone(), entry.timestamp as f64));
                    }
                }
            }
            "folder" => {
                if let Some(mut folder) = entry.type_src.clone() {
                    if !folder.is_empty() {
                        if !folder.ends_with('/') {
                            folder.push('/');
                        }
                        if std::path::Path::new(&folder).exists() {
                            items.push(RawItem::new("folder", folder, entry.timestamp as f64));
                        }
                    }
                }
            }
            "favorite" => {
                items.push(RawItem::new(
                    "favorite",
                    "favorite".to_string(),
                    entry.timestamp as f64,
                ));
            }
            // playlist/mix: store berkenaan (PlaylistTable / plugin mix) tiada
            // dalam env replay ini, jadi tiada item dihasilkan — sama seperti
            // Python yang `continue` bila lookup gagal.
            _ => {}
        }
    }

    items
}

/// `get_recently_played(limit, userid)` — gelung BATCH_SIZE=200, max 20 iter.
fn build_recently_played(
    conn: &rusqlite::Connection,
    st: &AppState,
    idx: &HashMap<String, usize>,
    userid: i64,
    limit: i64,
) -> Vec<RawItem> {
    const BATCH_SIZE: i64 = 200;
    const MAX_ITERATIONS: usize = 20;

    let mut items_map: Vec<RawItem> = Vec::new();
    let mut current_index = 0i64;
    let mut iterations = 0usize;
    let mut entries = scrobble_page(conn, 0, BATCH_SIZE, userid);

    while (items_map.len() as i64) < limit && iterations < MAX_ITERATIONS {
        let items = create_items(&entries, limit, st, idx);
        if items.is_empty() {
            break;
        }
        for item in items {
            if items_map.iter().any(|e| e.hash == item.hash) {
                continue;
            }
            items_map.push(item);
            if items_map.len() as i64 >= limit {
                return sorted_recent(&items_map);
            }
        }
        current_index += BATCH_SIZE;
        entries = scrobble_page(conn, current_index, BATCH_SIZE, userid);
        iterations += 1;
    }

    sorted_recent(&items_map)
}

/// `sorted(items_map.values(), key=timestamp, reverse=True)` — stabil.
fn sorted_recent(items: &[RawItem]) -> Vec<RawItem> {
    let mut out = items.to_vec();
    out.sort_by(|a, b| {
        b.timestamp
            .partial_cmp(&a.timestamp)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

// ---------------------------------------------------------------------------
// lib/home/recentlyadded.py
// ---------------------------------------------------------------------------

/// `calc_based_on_percent` — (>=70%, most common, count).
fn calc_based_on_percent(items: &[String], total: usize) -> (bool, String, usize) {
    if items.is_empty() || total == 0 {
        return (false, String::new(), 0);
    }
    let mut most_common = items[0].clone();
    let mut most_count = 0usize;
    for cand in items {
        let c = items.iter().filter(|x| *x == cand).count();
        if c > most_count {
            most_count = c;
            most_common = cand.clone();
        }
    }
    ((most_count as f64) / (total as f64) >= 0.7, most_common, most_count)
}

fn create_track_item(t: &Track) -> RawItem {
    let mut item = RawItem::new("track", t.trackhash.clone(), t.last_mod);
    item.help_text = Some("NEW TRACK".to_string());
    item
}

/// `check_folder_type(group_)` — satu folder → satu item (atau senarai track).
fn check_folder_type(
    folder: &str,
    tracks: &[&Track],
    time: f64,
    st: &AppState,
) -> Option<Vec<RawItem>> {
    if tracks.len() == 1 {
        let mut entry = create_track_item(tracks[0]);
        entry.timestamp = time;
        return Some(vec![entry]);
    }

    let albumhashes: Vec<String> = tracks.iter().map(|t| t.albumhash.clone()).collect();
    let (is_album, albumhash, _) = calc_based_on_percent(&albumhashes, tracks.len());
    if is_album {
        if !album_known(st, &albumhash) {
            return None;
        }
        let mut item = RawItem::new("album", albumhash, time);
        item.help_text = Some("NEW ALBUM".to_string());
        return Some(vec![item]);
    }

    let artisthashes: Vec<String> = tracks
        .iter()
        .flat_map(|t| t.artisthashes.iter().cloned())
        .collect();
    let (is_artist, artisthash, _) = calc_based_on_percent(&artisthashes, tracks.len());
    if is_artist {
        if !artist_known(st, &artisthash) {
            return None;
        }
        let mut item = RawItem::new("artist", artisthash, time);
        item.help_text = Some("NEW ARTIST".to_string());
        return Some(vec![item]);
    }

    if tracks.len() < 3 {
        return Some(tracks.iter().map(|t| create_track_item(t)).collect());
    }

    let mut item = RawItem::new("folder", folder.to_string(), time);
    item.help_text = Some("NEW MUSIC".to_string());
    Some(vec![item])
}

/// `get_recently_added_items(limit)`.
fn get_recently_added_items(st: &AppState, limit: i64) -> Vec<RawItem> {
    let lib = &st.library;
    if lib.tracks.is_empty() {
        return Vec::new();
    }

    // get_recently_added_tracks(0, None) → `last_mod` DESC (stabil).
    let mut order: Vec<usize> = (0..lib.tracks.len()).collect();
    order.sort_by(|&a, &b| {
        lib.tracks[b]
            .last_mod
            .partial_cmp(&lib.tracks[a].last_mod)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // group_track_by_folders: natsorted(folder) lalu groupby → dict order.
    let mut folders: Vec<String> = Vec::new();
    {
        let mut sorted: Vec<&Track> = order.iter().map(|&i| &lib.tracks[i]).collect();
        sorted.sort_by(|a, b| {
            crate::natsort::cmp_keys(
                &crate::natsort::nat_key(&a.folder),
                &crate::natsort::nat_key(&b.folder),
            )
        });
        for t in sorted {
            if !folders.iter().any(|f| f == &t.folder) {
                folders.push(t.folder.clone());
            }
        }
    }

    let mut all_tracks: HashMap<&str, Vec<&Track>> = HashMap::new();
    for &i in &order {
        all_tracks
            .entry(lib.tracks[i].folder.as_str())
            .or_default()
            .push(&lib.tracks[i]);
    }

    // `pathlib.Path(folder).exists()` + sort track by last_mod DESC +
    // sort groups by time DESC.
    let mut grouplist: Vec<(&str, Vec<&Track>, f64)> = Vec::new();
    for folder in &folders {
        if !std::path::Path::new(folder).exists() {
            continue;
        }
        let mut group = match all_tracks.remove(folder.as_str()) {
            Some(g) => g,
            None => continue,
        };
        group.sort_by(|a, b| {
            b.last_mod
                .partial_cmp(&a.last_mod)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let time = group[0].last_mod;
        grouplist.push((folder.as_str(), group, time));
    }
    grouplist.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut recent_items: Vec<Value> = Vec::new();
    for (folder, group, time) in grouplist {
        let handled: Option<Value> = match check_folder_type(folder, &group, time, st) {
            None => None,
            Some(xs) => {
                let vals: Vec<Value> = xs.iter().map(|x| x.to_dict()).collect();
                if vals.len() == 1 {
                    Some(vals[0].clone())
                } else {
                    Some(Value::Array(vals))
                }
            }
        };
        if let Some(it) = handled {
            if !contains_item(&recent_items, &it) {
                match it {
                    Value::Array(xs) => recent_items.extend(xs),
                    other => recent_items.push(other),
                }
            }
        }
        if recent_items.len() as i64 >= limit {
            break;
        }
    }

    // Layer RawItem di atas dict untuk recover_items.
    recent_items
        .into_iter()
        .filter_map(|v| raw_from_value(&v))
        .collect()
}

fn raw_from_value(v: &Value) -> Option<RawItem> {
    let m = v.as_object()?;
    let kind = m.get("type")?.as_str()?.to_string();
    let hash = m.get("hash")?.as_str()?.to_string();
    let ts = m.get("timestamp").and_then(|t| t.as_f64()).unwrap_or(0.0);
    let help_text = m
        .get("help_text")
        .and_then(|h| h.as_str())
        .map(|s| s.to_string());
    Some(RawItem {
        kind,
        hash,
        timestamp: ts,
        help_text,
        is_custom: false,
    })
}

// ---------------------------------------------------------------------------
// HomepageStore — store/homepage.py + store/homepageentries.py
// ---------------------------------------------------------------------------

struct HomepageStore {
    /// recently_played: userid → items (entry items tidak kosong untuk setiap user)
    recently_played: HashMap<i64, Vec<RawItem>>,
    /// recently_added: bucket 0 (global). `None` = items kosong → ditapis keluar.
    recently_added: Option<Vec<RawItem>>,
}

impl HomepageStore {
    /// Isi store seperti selepas routine `RecentlyPlayed`/`RecentlyAdded`
    /// berjalan dengan `update_only=False` (lib/recipes/recents.py::run).
    fn load(conn: &rusqlite::Connection, st: &AppState) -> HomepageStore {
        let idx = track_index(&st.library.tracks);
        let mut recently_played: HashMap<i64, Vec<RawItem>> = HashMap::new();
        for uid in user_ids(conn) {
            let items = build_recently_played(conn, st, &idx, uid, RECENTS_ITEM_LIMIT);
            recently_played.insert(uid, items);
        }
        let added = get_recently_added_items(st, RECENTS_ITEM_LIMIT);
        HomepageStore {
            recently_played,
            recently_added: if added.is_empty() { None } else { Some(added) },
        }
    }

    /// `get_homepage_items(limit)` — recently_played dahulu, recently_added
    /// terakhir (`recently_added = homedata.pop()`).
    fn get_homepage_items(&self, userid: i64, limit: i64, st: &AppState) -> Vec<Value> {
        let mut homedata: Vec<Value> = Vec::new();

        if !self.recently_played.is_empty() {
            let raw = self
                .recently_played
                .get(&userid)
                .cloned()
                .unwrap_or_default();
            homedata.push(entry_obj(
                "recently_played",
                "Recently played",
                "",
                recover_items(py_head(&raw, limit), st, userid),
            ));
        }

        let mut recently_added: Option<Value> = None;
        if let Some(raw) = &self.recently_added {
            // `RecentlyAddedHomepageEntry.get_items` → `super().get_items(0, limit)`
            recently_added = Some(entry_obj(
                "recently_added",
                "Recently added",
                "New music added to your library",
                recover_items(py_head(raw, limit), st, 0),
            ));
        }

        if let Some(ra) = recently_added {
            homedata.push(ra);
        }
        homedata
    }
}

fn entry_obj(key: &str, title: &str, description: &str, items: Vec<Value>) -> Value {
    let mut e = Map::new();
    e.insert("description".to_string(), json!(description));
    e.insert("items".to_string(), Value::Array(items));
    e.insert("title".to_string(), json!(title));
    let mut g = Map::new();
    g.insert(key.to_string(), Value::Object(e));
    Value::Object(g)
}

fn user_ids(conn: &rusqlite::Connection) -> Vec<i64> {
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id FROM user") {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, i64>(0)) {
            for r in rows.flatten() {
                out.push(r);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// lib/home/recover_items.py
// ---------------------------------------------------------------------------

fn recover_items(items: Vec<RawItem>, st: &AppState, userid: i64) -> Vec<Value> {
    let idx = track_index(&st.library.tracks);
    let now = now_ts();
    let mut recovered: Vec<Value> = Vec::new();

    for item in items {
        let rec: Value = match item.kind.as_str() {
            "album" => match st.library.albums.iter().find(|a| a.albumhash == item.hash) {
                Some(a) => {
                    let mut card = crate::getall::album_card(a);
                    if let Some(m) = card.as_object_mut() {
                        // album_serializer(to_remove={genres,date,count,
                        //   duration,albumartists_hashes,og_title})
                        for k in [
                            "genres",
                            "date",
                            "count",
                            "duration",
                            "albumartists_hashes",
                            "og_title",
                        ] {
                            m.remove(k);
                        }
                    }
                    recovered_json("album", card)
                }
                None => continue,
            },
            "artist" => match st.library.artists.iter().find(|a| a.artisthash == item.hash) {
                Some(a) => recovered_json("artist", Artist::to_json(a, userid)),
                None => continue,
            },
            "folder" => {
                let mut m = Map::new();
                m.insert("count".to_string(), json!(count_tracks_containing_path(st, &item.hash)));
                m.insert("path".to_string(), json!(item.hash));
                recovered_json("folder", Value::Object(m))
            }
            "favorite" => {
                let mut m = Map::new();
                m.insert("count".to_string(), json!(0));
                m.insert("image".to_string(), Value::Null);
                recovered_json("favorite", Value::Object(m))
            }
            "track" => match idx.get(&item.hash) {
                Some(&i) => recovered_json("track", st.library.tracks[i].to_json(userid, true)),
                None => continue,
            },
            // playlist/mix: recovery memerlukan PlaylistTable / plugin mix yang
            // tidak dihasilkan oleh routine recents.
            _ => continue,
        };

        let mut rec = rec;
        let helptext = item
            .help_text
            .clone()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| item.kind.clone());
        let secondary_text = crate::dates::timestamp_to_time_passed(item.timestamp as i64, now);

        if let Some(obj) = rec.get_mut("item").and_then(|v| v.as_object_mut()) {
            if !helptext.is_empty() {
                obj.insert("help_text".to_string(), json!(helptext));
            }
            if !secondary_text.is_empty() {
                obj.insert("time".to_string(), json!(secondary_text));
            }
        }
        recovered.push(rec);
    }

    recovered
}

fn recovered_json(kind: &str, item: Value) -> Value {
    let mut m = Map::new();
    m.insert("item".to_string(), item);
    m.insert("type".to_string(), json!(kind));
    Value::Object(m)
}

/// `FolderStore.count_tracks_containing_paths([p])[0]["trackcount"]` — kiraan
/// filepath yang bermula dengan prefix path yang sama.
fn count_tracks_containing_path(st: &AppState, dirpath: &str) -> i64 {
    st.library
        .tracks
        .iter()
        .filter(|t| t.filepath.starts_with(dirpath))
        .count() as i64
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn db(st: &AppState) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(&st.paths.app_db_path)?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    Ok(conn)
}

fn internal() -> Response {
    let mut m = Map::new();
    m.insert("msg".to_string(), json!("an error occurred"));
    (StatusCode::INTERNAL_SERVER_ERROR, Json(Value::Object(m))).into_response()
}

/// GET /nothome/recents/added — {"items": [...]}.
pub async fn get_recently_added(
    State(st): State<AppState>,
    Query(q): Query<GenericLimitQuery>,
    _ext: Option<Extension<CurrentUser>>,
) -> Response {
    let items = get_recently_added_items(&st, q.limit);
    let out: Vec<Value> = items.iter().map(|i| i.to_dict()).collect();
    Json(json!({ "items": out })).into_response()
}

/// GET /nothome/recents/played — {"items": [...]}.
pub async fn get_recently_played(
    State(st): State<AppState>,
    Query(q): Query<GenericLimitQuery>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let userid = user_id(&ext);
    let conn = match db(&st) {
        Ok(c) => c,
        Err(_) => return internal(),
    };
    let idx = track_index(&st.library.tracks);
    let items = build_recently_played(&conn, &st, &idx, userid, q.limit);
    let out: Vec<Value> = items.iter().map(|i| i.to_dict()).collect();
    Json(json!({ "items": out })).into_response()
}

/// GET /nothome/ — array-of-entries (default limit 9).
pub async fn homepage_items(
    State(st): State<AppState>,
    Query(q): Query<HomepageQuery>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let userid = user_id(&ext);
    let conn = match db(&st) {
        Ok(c) => c,
        Err(_) => return internal(),
    };
    let store = HomepageStore::load(&conn, &st);
    Json(Value::Array(store.get_homepage_items(userid, q.limit, &st))).into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `?a=b` tanpa kebergantungan baharu: deserializer serde dengan
    /// bentuk yang sama seperti `Query` (urlencoded key=value).
    fn q_limit<T: for<'de> Deserialize<'de>>(s: &str) -> T {
        let s = if s.is_empty() {
            "{}".to_string()
        } else {
            let (k, v) = s.split_once('=').unwrap();
            format!("{{\"{k}\":{v}}}")
        };
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn limit_defaults() {
        let q: GenericLimitQuery = q_limit("");
        assert_eq!(q.limit, API_CARD_LIMIT);
        let h: HomepageQuery = q_limit("");
        assert_eq!(h.limit, HOMEPAGE_LIMIT);
        let q3: GenericLimitQuery = q_limit("limit=3");
        assert_eq!(q3.limit, 3);
    }

    #[test]
    fn tracklog_prefix_map() {
        let t = TrackLog::from_row("h".into(), 1, "fo:/music/music/");
        assert_eq!(t.kind, "folder");
        assert_eq!(t.type_src.as_deref(), Some("/music/music/"));
        let t = TrackLog::from_row("h".into(), 1, "metrolist");
        assert_eq!(t.kind, "track");
        assert!(t.type_src.is_none());
        let t = TrackLog::from_row("h".into(), 1, "0f4ca210dba053a9");
        assert_eq!(t.kind, "track");
        let t = TrackLog::from_row("h".into(), 1, "al:abc");
        assert_eq!(t.kind, "album");
        assert_eq!(t.type_src.as_deref(), Some("abc"));
    }

    #[test]
    fn percent_threshold_is_70() {
        let mut all = vec!["a".to_string(); 7];
        all.extend(vec!["b".to_string(); 3]);
        let (ok, mc, n) = calc_based_on_percent(&all, 10);
        assert!(ok);
        assert_eq!(mc, "a");
        assert_eq!(n, 7);
        let (ok6, _, _) = calc_based_on_percent(&vec!["a".to_string(); 6], 10);
        assert!(!ok6);
    }

    #[test]
    fn flat_json_shape() {
        let it = RawItem::new("folder", "/music/music/".into(), 1_789_019_573.0);
        assert_eq!(
            serde_json::to_string(&it.to_dict()).unwrap(),
            r#"{"hash":"/music/music/","timestamp":1789019573,"type":"folder"}"#
        );
    }

    /// Regresi 2026-11-25: dedup mesti pada `entry.source`, BUKAN `trackhash`.
    /// Data sebenar user 1 ada 41 scrobble dengan source sama ("metrolist");
    /// dengan dedup ikut trackhash, `/nothome/recents/played` pulang 6 item
    /// sedangkan gold Python pulang 3 (satu per source unik).
    #[test]
    fn dedup_is_on_source_not_trackhash() {
        let entries = vec![
            TrackLog::from_row("h1".into(), 300, "metrolist"),
            TrackLog::from_row("h2".into(), 200, "metrolist"),
            TrackLog::from_row("h3".into(), 100, "metrolist"),
        ];
        // ketiga-tiga hash berbeza tetapi source sama → hanya SATU yang lulus
        let mut seen: Vec<&str> = Vec::new();
        let kept: Vec<&str> = entries
            .iter()
            .filter(|e| {
                if seen.contains(&e.source.as_str()) {
                    false
                } else {
                    seen.push(&e.source);
                    true
                }
            })
            .map(|e| e.trackhash.as_str())
            .collect();
        assert_eq!(kept, vec!["h1"]);
    }
}
