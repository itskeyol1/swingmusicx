//! Port of swingmusic/api/search.py — `/search` + `/search/top` routes.
//!
//! Verbatim semantics (live source read 2026-11-24; api/search.py + lib/searchlib.py):
//! - SEARCH_COUNT = 30; `/search/top` limit defaults to Defaults.API_CARD_LIMIT (6);
//!   `q` is unidecode()d then default_process()ed before every extraction
//!   (process::extract runs no processor — caller pre-processes, per its docs).
//! - Scorers: tracks + artists use fuzz.WRatio, albums use fuzz.token_sort_ratio;
//!   score floors Cutoff.* = 50; engine caps Limit.* = 150.
//! - SearchTracks removes duplicates (max bitrate per trackhash, first-seen order);
//!   SearchArtists / SearchAlbums do not.
//! - TopResults.search: tracks_limit = 150 on tracks_only else 4; albums_limit =
//!   150 on albums_only else limit; artists_limit = limit (None => all passing).
//!   all_results = artists + tracks + albums, STABLE sort by int(_score) DESC.
//!   Empty result => {top_result: null, tracks: [], artists: [], albums: []}.
//! - top tracks: per top item (Album -> by albumhash, Artist -> by artisthash),
//!   `[:tracks_limit]` then stable playduration DESC; shortfall filled from the
//!   scored search tracks by trackhash (found-set SNAPSHOT before the loop, kept
//!   1:1); serialized with remove_disc=true. tracks_only returns that bare list.
//! - top albums: Artist -> albums `[:albums_limit]`; shortfall filled likewise;
//!   serialize_for_card_many. albums_only returns that bare list.
//! - top_result: Track -> serialize_track + type "track"; Album -> card + "album";
//!   Artist -> card keeping {albumcount, trackcount} + "artist".
//! - Response keys: top_result, tracks, artists, albums.
//! - /search/top: empty q -> {"error": "No query provided"}, 400.
//! - /search/: itemtype tracks|albums|artists, else {"error": "Invalid item type.
//!   Valid types are 'tracks', 'albums' and 'artists'"}, 400. Response:
//!   {"results": results[start:start+limit], "more": len(results) > start+limit}.
//! - /search/ itemtype artists runs SearchArtists with its default limit (150);
//!   tracks/albums route through TopResults with tracks_only/albums_only (limit None).
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::fuzz::{default_process, token_sort_ratio, w_ratio};
use crate::getall::{album_card, artist_card, user_id, API_CARD_LIMIT};
use crate::models::{record_album_scores, Album, Artist, Track};
use crate::process::{extract, Candidate};
use crate::unidecode::unidecode;
use crate::{AppState, CurrentUser};

/// api/search.py SEARCH_COUNT.
const SEARCH_COUNT: i64 = 30;
/// searchlib.Cutoff.tracks / .albums / .artists — all 50.
const CUTOFF: f64 = 50.0;
/// searchlib.Limit.tracks / .albums / .artists — all 150.
const LIMIT: i64 = 150;

fn default_limit() -> i64 {
    API_CARD_LIMIT
}

fn default_search_limit() -> i64 {
    SEARCH_COUNT
}

#[derive(Deserialize)]
pub struct TopResultsQuery {
    #[serde(default)]
    pub q: String,
    /// TopResultsQuery.limit — Defaults.API_CARD_LIMIT (live default 6).
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Deserialize)]
pub struct SearchLoadMoreQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub itemtype: String,
    #[serde(default)]
    pub start: i64,
    #[serde(default = "default_search_limit")]
    pub limit: i64,
}

/// TrackStore.get_flat_list(): chain trackhashmap groups in first-seen trackhash
/// order (dict insertion order), each group's tracks in load order.
pub(crate) fn flat_tracks(tracks: &[Track]) -> Vec<Track> {
    let mut order: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for t in tracks {
        if seen.insert(t.trackhash.as_str()) {
            order.push(t.trackhash.as_str());
        }
    }
    let mut out: Vec<Track> = Vec::with_capacity(tracks.len());
    for h in order {
        for t in tracks {
            if t.trackhash == h {
                out.push(t.clone());
            }
        }
    }
    out
}

/// Python `lst[:limit]` (negative limit trims from the end).
pub(crate) fn py_limit<T>(items: Vec<T>, limit: i64) -> Vec<T> {
    let n = items.len() as i64;
    if limit < 0 {
        let keep = (n + limit).max(0);
        items.into_iter().take(keep as usize).collect()
    } else {
        items.into_iter().take(limit.min(n) as usize).collect()
    }
}

/// Python `lst[start:stop]` slice semantics (negative indexes normalised first).
fn py_slice<T: Clone>(items: &[T], start: i64, stop: i64) -> Vec<T> {
    let n = items.len() as i64;
    let s = if start < 0 { (n + start).max(0) } else { start.min(n) };
    let e = if stop < 0 { (n + stop).max(0) } else { stop.min(n) };
    let e = e.max(s);
    items[s as usize..e as usize].to_vec()
}

/// utils/remove_duplicates.py (is_album_tracks=False): keep the max-bitrate
/// track per trackhash, groups in first-seen order (max() ties keep the first).
fn remove_duplicates(tracks: Vec<Track>) -> Vec<Track> {
    let mut pos: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Track> = Vec::new();
    for t in tracks {
        match pos.get(&t.trackhash) {
            Some(&i) => {
                if t.bitrate > out[i].bitrate {
                    out[i] = t;
                }
            }
            None => {
                pos.insert(t.trackhash.clone(), out.len());
                out.push(t);
            }
        }
    }
    out
}

/// TrackStore.find_tracks_by(albumhash): flat order, then remove_duplicates.
fn tracks_by_albumhash(flat: &[Track], albumhash: &str) -> Vec<Track> {
    let matched: Vec<Track> = flat
        .iter()
        .filter(|t| t.albumhash == albumhash)
        .cloned()
        .collect();
    remove_duplicates(matched)
}

/// TrackStore.get_tracks_by_artisthash: `artisthash in artisthashes` predicate.
fn tracks_by_artisthash(flat: &[Track], artisthash: &str) -> Vec<Track> {
    let matched: Vec<Track> = flat
        .iter()
        .filter(|t| t.artisthashes.iter().any(|h| h == artisthash))
        .cloned()
        .collect();
    remove_duplicates(matched)
}

/// SearchTracks.__call__ — WRatio over unidecode(title).lower(), deduped.
/// Returns (track, _score) pairs; `limit` is the extract cap BEFORE dedup.
fn search_tracks(flat: &[Track], query_proc: &str, limit: Option<usize>) -> Vec<(Track, f64)> {
    let candidates: Vec<Candidate<'_>> = flat
        .iter()
        .map(|t| Candidate {
            original: t.title.as_str(),
            processed: default_process(&unidecode(&t.title).to_lowercase()),
        })
        .collect();
    let scored: Vec<(Track, f64)> = extract(query_proc, &candidates, w_ratio, CUTOFF, limit)
        .into_iter()
        .map(|r| (flat[r.index].clone(), r.score))
        .collect();
    // remove_duplicates carries the kept object's _score.
    let mut pos: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<(Track, f64)> = Vec::new();
    for (t, s) in scored {
        match pos.get(&t.trackhash) {
            Some(&i) => {
                if t.bitrate > out[i].0.bitrate {
                    out[i] = (t, s);
                }
            }
            None => {
                pos.insert(t.trackhash.clone(), out.len());
                out.push((t, s));
            }
        }
    }
    out
}

/// SearchAlbums.__call__ — token_sort_ratio over unidecode(title).lower().
fn search_albums(st: &AppState, query_proc: &str, limit: Option<usize>) -> Vec<(Album, f64)> {
    let candidates: Vec<Candidate<'_>> = st
        .library
        .albums
        .iter()
        .map(|a| Candidate {
            original: a.title.as_str(),
            processed: default_process(&unidecode(&a.title).to_lowercase()),
        })
        .collect();
    extract(query_proc, &candidates, token_sort_ratio, CUTOFF, limit)
        .into_iter()
        .map(|r| (st.library.albums[r.index].clone(), r.score))
        .collect()
}

/// SearchArtists.__call__ — WRatio over unidecode(name).lower().
fn search_artists(st: &AppState, query_proc: &str, limit: Option<usize>) -> Vec<(Artist, f64)> {
    let candidates: Vec<Candidate<'_>> = st
        .library
        .artists
        .iter()
        .map(|a| Candidate {
            original: a.name.as_str(),
            processed: default_process(&unidecode(&a.name).to_lowercase()),
        })
        .collect();
    extract(query_proc, &candidates, w_ratio, CUTOFF, limit)
        .into_iter()
        .map(|r| (st.library.artists[r.index].clone(), r.score))
        .collect()
}

/// Limit cast for process::extract: negative values fall back to "no cap"
/// (rapidfuzz caps are size_t; live clients send ints well above 0).
fn extract_cap(limit: Option<i64>) -> Option<usize> {
    match limit {
        Some(v) if v >= 0 => Some(v as usize),
        _ => None,
    }
}

/// searchlib.TopResults.search (live shape: limit None or i64 from the query).
fn top_results(
    st: &AppState,
    userid: i64,
    query: &str,
    limit: Option<i64>,
    albums_only: bool,
    tracks_only: bool,
) -> Value {
    let query_proc = default_process(&unidecode(query));

    let tracks_limit: i64 = if tracks_only { LIMIT } else { 4 };
    let albums_limit: Option<i64> = if albums_only { Some(LIMIT) } else { limit };
    let artists_limit: Option<i64> = limit;

    let flat = flat_tracks(&st.library.tracks);
    let tracks_scored: Vec<(Track, f64)> = if !albums_only {
        search_tracks(&flat, &query_proc, extract_cap(Some(tracks_limit)))
    } else {
        Vec::new()
    };
    let albums_scored = search_albums(st, &query_proc, extract_cap(albums_limit));

    // Live SearchAlbums.__call__ mutates the SHARED album dicts in place
    // (`album._score = item[1]`) and the album serializer keeps `_score`:
    // the scores persist for every later response that re-serializes these
    // albums (artist pages, /search/top fill). Write them through.
    record_album_scores(
        &albums_scored
            .iter()
            .map(|(a, s)| (a.albumhash.clone(), *s))
            .collect::<Vec<_>>(),
    );

    let artists_scored = search_artists(st, &query_proc, extract_cap(artists_limit));

    // all_results = artists + tracks + albums, stable sort by int(_score) DESC.
    let mut all: Vec<(i64, u8, usize)> = Vec::new();
    for (i, (_, s)) in artists_scored.iter().enumerate() {
        all.push((*s as i64, 0, i));
    }
    for (i, (_, s)) in tracks_scored.iter().enumerate() {
        all.push((*s as i64, 1, i));
    }
    for (i, (_, s)) in albums_scored.iter().enumerate() {
        all.push((*s as i64, 2, i));
    }
    all.sort_by(|a, b| b.0.cmp(&a.0));

    if all.is_empty() {
        return json!({ "top_result": null, "tracks": [], "artists": [], "albums": [] });
    }

    let (_, top_kind, top_idx) = all[0];

    // ---- top tracks ------------------------------------------------------
    let mut top_tracks_v: Vec<Value> = Vec::new();
    if !albums_only {
        let mut top_tracks: Vec<Track> = match top_kind {
            1 => Vec::new(),
            2 => {
                let ah = albums_scored[top_idx].0.albumhash.clone();
                let mut v = py_limit(tracks_by_albumhash(&flat, &ah), tracks_limit);
                v.sort_by(|a, b| b.playduration.cmp(&a.playduration));
                v
            }
            _ => {
                let ah = artists_scored[top_idx].0.artisthash.clone();
                let mut v = py_limit(tracks_by_artisthash(&flat, &ah), tracks_limit);
                v.sort_by(|a, b| b.playduration.cmp(&a.playduration));
                v
            }
        };

        // Fill shortfall from the search results; found-set is a SNAPSHOT
        // (never updated in the loop) — kept 1:1 with the live Python.
        if (top_tracks.len() as i64) < tracks_limit {
            let found: HashSet<String> =
                top_tracks.iter().map(|t| t.trackhash.clone()).collect();
            for (t, _s) in tracks_scored.iter() {
                if !found.contains(&t.trackhash) {
                    top_tracks.push(t.clone());
                }
                if (top_tracks.len() as i64) >= tracks_limit {
                    break;
                }
            }
        }

        top_tracks_v = top_tracks.iter().map(|t| t.to_json(userid, true)).collect();

        if tracks_only {
            return Value::Array(top_tracks_v);
        }
    }

    // ---- top albums ------------------------------------------------------
    let mut top_albums_v: Vec<Value> = Vec::new();
    {
        let mut top_albums: Vec<Album> = match top_kind {
            0 => {
                let ah = artists_scored[top_idx].0.artisthash.clone();
                let v = st.library.get_albums_by_artisthash(&ah);
                match albums_limit {
                    Some(al) => py_limit(v, al),
                    None => v,
                }
            }
            _ => Vec::new(),
        };

        if let Some(al) = albums_limit {
            if (top_albums.len() as i64) < al {
                let found: HashSet<String> =
                    top_albums.iter().map(|a| a.albumhash.clone()).collect();
                for (a, _s) in albums_scored.iter() {
                    if !found.contains(&a.albumhash) {
                        top_albums.push(a.clone());
                    }
                    if (top_albums.len() as i64) >= al {
                        break;
                    }
                }
            }
        }

        top_albums_v = top_albums.iter().map(album_card).collect();

        if albums_only {
            return Value::Array(top_albums_v);
        }
    }

    // ---- top result ------------------------------------------------------
    let top_result_v: Value = match top_kind {
        1 => {
            let mut d = tracks_scored[top_idx].0.to_json(userid, true);
            if let Some(m) = d.as_object_mut() {
                m.insert("type".to_string(), json!("track"));
            }
            d
        }
        2 => {
            let mut d = album_card(&albums_scored[top_idx].0);
            if let Some(m) = d.as_object_mut() {
                m.insert("type".to_string(), json!("album"));
            }
            d
        }
        _ => {
            // serialize_for_card(artist, include={"albumcount", "trackcount"})
            let a = &artists_scored[top_idx].0;
            let mut d = artist_card(a);
            if let Some(m) = d.as_object_mut() {
                m.insert("albumcount".to_string(), json!(a.albumcount));
                m.insert("trackcount".to_string(), json!(a.trackcount));
                m.insert("type".to_string(), json!("artist"));
            }
            d
        }
    };

    let artists_v: Vec<Value> = artists_scored
        .iter()
        .map(|(a, _)| artist_card(a))
        .collect();

    json!({
        "top_result": top_result_v,
        "tracks": top_tracks_v,
        "artists": artists_v,
        "albums": top_albums_v,
    })
}

/// GET /search/top — Search(q).get_top_results(limit=query.limit).
pub async fn get_top_results(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Query(q): Query<TopResultsQuery>,
) -> Response {
    if q.q.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "No query provided" })),
        )
            .into_response();
    }

    let userid = user_id(&ext);
    Json(top_results(&st, userid, &q.q, Some(q.limit), false, false)).into_response()
}

/// GET /search/ — load-more dispatch by itemtype.
pub async fn search_items(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Query(q): Query<SearchLoadMoreQuery>,
) -> Response {
    let userid = user_id(&ext);
    let results: Vec<Value> = match q.itemtype.as_str() {
        "tracks" => {
            let v = top_results(&st, userid, &q.q, None, false, true);
            v.as_array().cloned().unwrap_or_default()
        }
        "albums" => {
            let v = top_results(&st, userid, &q.q, None, true, false);
            v.as_array().cloned().unwrap_or_default()
        }
        "artists" => {
            // Search(query.q).search_artists(): SearchArtists default limit = 150.
            let query_proc = default_process(&unidecode(&q.q));
            search_artists(&st, &query_proc, Some(LIMIT as usize))
                .iter()
                .map(|(a, _)| artist_card(a))
                .collect()
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "Invalid item type. Valid types are 'tracks', 'albums' and 'artists'"
                })),
            )
                .into_response();
        }
    };

    let total = results.len() as i64;
    let stop = q.start.saturating_add(q.limit);
    let sliced = py_slice(&results, q.start, stop);
    let more = total > stop;

    Json(json!({ "results": sliced, "more": more })).into_response()
}
