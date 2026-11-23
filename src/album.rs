//! Port of swingmusic/api/album.py — all 5 `/album` routes.
//!
//! Verbatim semantics (live source read 2026-11-18/19):
//! - POST /album: unknown hash → 404 {"error": "Album not found"}; else mutates a
//!   copy with trackcount/duration/check_type, then builds stats/info/extra/
//!   copyright/tracks/more_from/other_versions.
//! - GET /album/<hash>/tracks: sort_by_track_no (disc,track), remove_disc=false→True swap:
//!   serialize_tracks(tracks) default removes disc+track.
//! - POST /album/from-artist: seen_hashes dedup across artisthashes + base_title hash
//!   exclusion + `[:limit]` Python slicing.
//! - POST /album/other-versions: `[]` when unknown; same base_title + artisthash in
//!   albumartists + different og_title.
//! - GET /album/similar: SimilarArtistTable.get_by_hash → random.sample(min(len,limit)).
#![allow(dead_code)]

use std::collections::HashSet;

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::dates::seconds_to_time_string;
use crate::getall::{album_card, artist_card, now_ts, user_id};
use crate::hashing::create_hash;
use crate::models::{Album, Track, UserConfig};
use crate::{AppState, CurrentUser};

fn default_album_limit() -> i64 {
    crate::getall::API_CARD_LIMIT
}

#[derive(Deserialize)]
pub struct GetAlbumInfoBody {
    albumhash: String,
    /// AlbumLimitSchema alias — clients send `albumlimit`.
    #[serde(default = "default_album_limit", alias = "limit")]
    albumlimit: i64,
}

#[derive(Deserialize)]
pub struct GetMoreFromArtistsBody {
    albumartists: Vec<String>,
    #[serde(default = "default_album_limit", alias = "limit")]
    albumlimit: i64,
    #[serde(default)]
    base_title: String,
}

#[derive(Deserialize)]
pub struct GetAlbumVersionsBody {
    albumhash: String,
    #[serde(default)]
    og_album_title: String,
}

#[derive(Deserialize)]
pub struct GetSimilarAlbumsQuery {
    artisthash: String,
    #[serde(default = "default_album_limit", alias = "limit")]
    albumlimit: i64,
}

fn set_value(d: &mut Map<String, Value>, k: &str, v: Value) {
    d.insert(k.to_string(), v);
}

/// `get_track_group_stats(tracks, is_album=True)` — utils/stats.py port.
pub fn get_track_group_stats(tracks: &[Track], is_album: bool) -> Vec<Value> {
    if tracks.is_empty() {
        return Vec::new();
    }
    let played: Vec<&Track> = tracks.iter().filter(|t| t.playcount > 0).collect();
    let unplayed_count = tracks.len() - played.len();

    let played_stat = json!({
        "cssclass": "played",
        "text": "never played",
        "value": format!("{}/{} tracks", unplayed_count, tracks.len()),
        "image": Value::Null,
    });

    let play_duration: i64 = played.iter().map(|t| t.playduration).sum();
    let play_duration_stat = json!({
        "cssclass": "play_duration",
        "text": "listened all time",
        "value": seconds_to_time_string(play_duration),
        "image": Value::Null,
    });

    let top_track = played
        .iter()
        .max_by_key(|t| t.playduration)
        .copied();
    let top_track_stat = match top_track {
        Some(t) => json!({
            "cssclass": "toptrack",
            "text": format!("top track ({} listened)", seconds_to_time_string(t.playduration)),
            "value": t.title,
            "image": t.image,
        }),
        None => json!({
            "cssclass": "toptrack",
            "text": "top track",
            "value": "—",
            "image": Value::Null,
        }),
    };

    let mut stats = vec![play_duration_stat, played_stat, top_track_stat];

    if !is_album {
        // per-albumhash aggregation, sorted by summed playduration desc
        let mut order: Vec<String> = Vec::new();
        let mut map: std::collections::HashMap<String, (i64, i64, String, Value)> =
            std::collections::HashMap::new();
        for t in tracks {
            let e = map.entry(t.albumhash.clone()).or_insert_with(|| {
                order.push(t.albumhash.clone());
                (
                    0,
                    0,
                    t.album.clone(),
                    if t.image.is_empty() {
                        Value::Null
                    } else {
                        json!(t.image)
                    },
                )
            });
            e.0 += 1;
            e.1 += t.playduration;
        }
        let mut albums: Vec<(i64, i64, String, Value)> =
            order.iter().map(|h| map.get(h).cloned().unwrap()).collect();
        albums.sort_by(|a, b| b.1.cmp(&a.1));
        let top = albums.first();
        let top_album_stat = match top {
            Some((_, dur, title, image)) if *dur != 0 => json!({
                "cssclass": "topalbum",
                "text": format!("top album ({} listened)", seconds_to_time_string(*dur)),
                "value": title,
                "image": image,
            }),
            _ => json!({
                "cssclass": "topalbum",
                "text": "top album",
                "value": "—",
                "image": Value::Null,
            }),
        };
        stats.push(top_album_stat);
    }

    if is_album {
        let tracktotal: i64 = tracks
            .iter()
            .map(|t| {
                t.extra
                    .get("track_total")
                    .map(|v| {
                        if v.is_null() {
                            0
                        } else if let Some(s) = v.as_str() {
                            s.parse::<f64>().map(|f| f as i64).unwrap_or(0)
                        } else if let Some(f) = v.as_f64() {
                            f as i64
                        } else if let Some(b) = v.as_bool() {
                            if b { 1 } else { 0 }
                        } else {
                            0
                        }
                    })
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        let completeness_stat = if tracktotal != 0 {
            let percentage = (tracks.len() as f64 / tracktotal as f64) * 100.0;
            let completedness: Value = if percentage <= 100.0 {
                json!(percentage as i64)
            } else {
                json!("?")
            };
            json!({
                "cssclass": "completeness",
                "text": format!("{}/{} tracks available", tracks.len(), tracktotal),
                "value": match completedness {
                    Value::String(s) => json!(format!("{}% complete", s)),
                    Value::Number(n) => json!(format!("{}% complete", n)),
                    _ => json!("% complete"),
                },
                "image": Value::Null,
            })
        } else {
            json!({
                "cssclass": "completeness",
                "text": format!("{}/? tracks available", tracks.len()),
                "value": "?",
                "image": Value::Null,
            })
        };
        stats.push(completeness_stat);
    }

    stats
}

/// POST /album — album info + tracks + more_from + other_versions.
pub async fn post_album(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Json(body): Json<GetAlbumInfoBody>,
) -> Response {
    let userid = user_id(&ext);
    let albumhash = body.albumhash;

    let entry = match st.library.albums.iter().find(|a| a.albumhash == albumhash) {
        Some(a) => a.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Album not found" })),
            )
                .into_response()
        }
    };
    let album = entry;

    let tracks = st.library.get_album_tracks(&albumhash);
    let mut album = album;
    album.trackcount = tracks.len() as i64;
    album.duration = tracks.iter().map(|t| t.duration).sum();
    album.check_type(&tracks, st.settings.from_settings().show_albums_as_singles);

    let mut track_total_set: HashSet<i64> = HashSet::new();
    for t in &tracks {
        let v = t
            .extra
            .get("track_total")
            .map(|v| {
                if v.is_null() {
                    0
                } else if let Some(s) = v.as_str() {
                    s.parse::<f64>().map(|f| f as i64).unwrap_or(0)
                } else if let Some(f) = v.as_f64() {
                    f as i64
                } else if let Some(b) = v.as_bool() {
                    if b { 1 } else { 0 }
                } else {
                    0
                }
            })
            .unwrap_or(1);
        track_total_set.insert(if v == 0 { 1 } else { v });
    }
    let track_total: i64 = track_total_set.iter().sum();
    let avg_bitrate: i64 = {
        let s: i64 = tracks.iter().map(|t| t.bitrate).sum();
        s / (tracks.len() as i64).max(1)
    };

    let more_from = more_from_artist(&st, &album, body.albumlimit);
    let other_versions = album_versions(&st, &albumhash, &album.og_title);

    // info = asdict(album) + is_favorite → to_json(userid) minus trackhashes + _score
    let mut info = album.to_json(userid);
    if let Some(m) = info.as_object_mut() {
        m.remove("trackhashes");
        set_value(m, "_score", json!(0));
    }

    let stats = get_track_group_stats(&tracks, true);
    let serialized: Vec<Value> = tracks.iter().map(|t| t.to_json(userid, false)).collect();

    Json(json!({
        "stats": stats,
        "info": info,
        "extra": { "track_total": track_total, "avg_bitrate": avg_bitrate },
        "copyright": tracks.first().map(|t| t.copyright.clone()).unwrap_or_default(),
        "tracks": serialized,
        "more_from": more_from,
        "other_versions": other_versions,
    }))
    .into_response()
}

/// GET /album/<albumhash>/tracks
pub async fn get_album_tracks(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Path(albumhash): Path<String>,
) -> Response {
    let userid = user_id(&ext);
    let mut tracks = st.library.get_album_tracks(&albumhash);
    crate::store::sort_by_track_no(&mut tracks);
    let out: Vec<Value> = tracks.iter().map(|t| t.to_json(userid, true)).collect();
    Json(out).into_response()
}

/// Python `lst[:limit]` (negative limit trims from the end).
fn py_limit<T>(items: Vec<T>, limit: i64) -> Vec<T> {
    let n = items.len() as i64;
    if limit < 0 {
        let keep = (n + limit).max(0);
        items.into_iter().take(keep as usize).collect()
    } else {
        items.into_iter().take(limit.min(n) as usize).collect()
    }
}

/// `get_more_from_artist` — returns {artisthash: [album cards]}.
pub fn more_from_artist(st: &AppState, album: &Album, limit: i64) -> Value {
    let albumartists: Vec<String> = album.albumartists.iter().map(|a| a.artisthash.clone()).collect();
    let base_title = album.base_title.clone();
    let base_hash = create_hash(&[base_title.as_str()], false);

    let mut out = Map::new();
    let mut seen_hashes: HashSet<String> = HashSet::new();

    for artisthash in &albumartists {
        let albums = st.library.get_albums_by_artisthash(artisthash);

        // INFO: filter out albums added to other artists / same artist-set / same base title
        let filtered: Vec<Album> = albums
            .into_iter()
            .filter(|a| {
                !seen_hashes.contains(&a.albumhash)
                    && a.artisthashes.contains(artisthash)
                    && create_hash(&[a.base_title.as_str()], false) != base_hash
            })
            .collect();

        let page = py_limit(filtered.clone(), limit);
        let cards: Vec<Value> = page.iter().map(album_card).collect();
        seen_hashes.extend(page.iter().map(|a| a.albumhash.clone()));
        out.insert(artisthash.clone(), Value::Array(cards));
    }

    Value::Object(out)
}

/// `get_album_versions` — [] when unknown; same base_title + artisthash + different og_title.
pub fn album_versions(st: &AppState, albumhash: &str, og_title: &str) -> Value {
    let album = match st.library.albums.iter().find(|a| a.albumhash == albumhash) {
        Some(a) => a.clone(),
        None => return json!([]),
    };
    let artisthash = match album.artisthashes.first() {
        Some(h) => h.clone(),
        None => return json!([]),
    };
    let albums = st.library.get_albums_by_artisthash(&artisthash);
    let basetitle = album.base_title.clone();
    let og = og_title.to_string();

    let filtered: Vec<Album> = albums
        .into_iter()
        .filter(|a| {
            a.og_title != og
                && a.base_title == basetitle
                && a.albumartists.iter().any(|x| x.artisthash == artisthash)
        })
        .collect();

    Value::Array(filtered.iter().map(album_card).collect())
}

/// POST /album/from-artist
pub async fn post_from_artist(
    State(st): State<AppState>,
    Json(body): Json<GetMoreFromArtistsBody>,
) -> Response {
    let limit = body.albumlimit;
    let base_hash = create_hash(&[body.base_title.as_str()], false);

    let mut all: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut seen_hashes: HashSet<String> = HashSet::new();

    for artisthash in &body.albumartists {
        let albums = st.library.get_albums_by_artisthash(artisthash);
        let filtered: Vec<Album> = albums
            .into_iter()
            .filter(|a| {
                !seen_hashes.contains(&a.albumhash)
                    && a.artisthashes.contains(artisthash)
                    && create_hash(&[a.base_title.as_str()], false) != base_hash
            })
            .collect();
        let page = py_limit(filtered, limit);
        let cards: Vec<Value> = page.iter().map(album_card).collect();
        seen_hashes.extend(page.iter().map(|a| a.albumhash.clone()));
        all.insert(artisthash.clone(), Value::Array(cards));
    }

    Json(Value::Object(all)).into_response()
}

/// POST /album/other-versions
pub async fn post_other_versions(
    State(st): State<AppState>,
    Json(body): Json<GetAlbumVersionsBody>,
) -> Response {
    Json(album_versions(&st, &body.albumhash, &body.og_album_title)).into_response()
}

/// GET /album/similar — random.sample over similar artists' albums.
pub async fn get_similar(
    State(st): State<AppState>,
    Query(q): Query<GetSimilarAlbumsQuery>,
) -> Response {
    let hashes = match st.library.similar_artist_hashes(&q.artisthash) {
        Some(h) => h,
        None => return Json(json!([])).into_response(),
    };
    let artists = st.library.get_artists_by_hashes(&hashes);
    let artisthashes: Vec<String> = artists.iter().map(|a| a.artisthash.clone()).collect();
    let albums = st.library.get_albums_by_artisthashes(&artisthashes);

    let n = albums.len().min(q.albumlimit.max(0) as usize);
    let mut rng = rand::thread_rng();
    let sample: Vec<Album> = rand::seq::SliceRandom::choose_multiple(albums.as_slice(), &mut rng, n)
        .into_iter()
        .cloned()
        .collect();
    let cards: Vec<Value> = sample.iter().take(q.albumlimit.max(0) as usize).map(album_card).collect();
    Json(Value::Array(cards)).into_response()
}
