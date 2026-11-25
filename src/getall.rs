//! Port of swingmusic/api/getall/__init__.py — `/getall/songs` + `/getall/<itemtype>`.
//!
//! Verbatim semantics carried over (live source read 2026-11-18/19):
//! - `total` = len(items) BEFORE paging; `items[start:start+limit]`, `limit<0` => `items[start:]`.
//! - `natsorted(items, key=casefold)` with `except AttributeError: sorted(items, key=sortfunc)`
//!   — numeric/date keys fall through that AttributeError to a plain stable sort.
//! - help_text branches per sort key ("Never played" for lastplayed+zero playduration;
//!   int year for albums `date`; track/play/album count text; seconds_to_time_string).
//! - albums-only keys: title, albumartists, date; artists-only: name, albumcount.
//! - serialize_for_card removal sets are exact copies of serializers/{album,artist}.py.
#![allow(dead_code)]

use std::cmp::Ordering;

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::dates::{seconds_to_time_string, timestamp_to_time_passed};
use crate::models::{Album, Artist, Track, UserConfig};
use crate::natsort::{casefold, cmp_keys, nat_key};
use crate::{AppState, CurrentUser};

/// Defaults.API_CARD_LIMIT from livesrc/settings.py (read 2026-11-19).
pub const API_CARD_LIMIT: i64 = 6;

#[derive(Deserialize)]
pub struct GetAllItemsQuery {
    #[serde(default)]
    start: i64,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default = "default_sortby")]
    sortby: String,
    #[serde(default = "default_reverse")]
    reverse: String,
}

fn default_limit() -> i64 {
    API_CARD_LIMIT
}
fn default_sortby() -> String {
    "created_date".to_string()
}
fn default_reverse() -> String {
    "1".to_string()
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current user id from the identity payload.
///
/// The JWT `sub` (and therefore `CurrentUser.0`) is the *identity dict*
/// (`user.todict()` — `{"id":1,...}`), NOT a bare int, so `as_i64()` always
/// yields 0. Read the `id` field instead, mirroring Python's
/// `get_current_userid()` (utils/auth.py).
pub fn user_id(ext: &Option<Extension<CurrentUser>>) -> i64 {
    ext.as_ref()
        .and_then(|Extension(cu)| cu.0.get("id"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
}

/// Python list slicing: `items[start:start+limit]` / `items[start:]` when limit<0.
fn py_slice<T>(items: Vec<T>, start: i64, limit: i64) -> Vec<T> {
    let n = items.len() as i64;
    if limit < 0 {
        let s = if start < 0 { (n + start).max(0) } else { start.min(n) };
        items.into_iter().skip(s as usize).collect()
    } else {
        let s = if start < 0 { (n + start).max(0) } else { start.min(n) };
        let stop = start.saturating_add(limit);
        let e = if stop < 0 { 0 } else { stop.min(n) };
        let e = e.max(s);
        items
            .into_iter()
            .skip(s as usize)
            .take((e - s) as usize)
            .collect()
    }
}

fn sort_nat_by<T, F: Fn(&T) -> String>(items: &mut [T], key: F, reverse: bool) {
    items.sort_by(|a, b| {
        let o = cmp_keys(&nat_key(&key(a)), &nat_key(&key(b)));
        if reverse {
            o.reverse()
        } else {
            o
        }
    });
}

fn sort_plain_by<T, K: Ord, F: Fn(&T) -> K>(items: &mut [T], key: F, reverse: bool) {
    items.sort_by(|a, b| {
        let o = key(a).cmp(&key(b));
        if reverse {
            o.reverse()
        } else {
            o
        }
    });
}

fn sort_f64_by<T, F: Fn(&T) -> f64>(items: &mut [T], key: F, reverse: bool) {
    items.sort_by(|a, b| {
        let o = key(a).partial_cmp(&key(b)).unwrap_or(Ordering::Equal);
        if reverse {
            o.reverse()
        } else {
            o
        }
    });
}

/// `format_number()` = locale.format_string("%d", n, grouping=True); the live
/// hosts run the C locale (no thousands separators) — plain int is byte-equal.
fn format_number(n: i64) -> String {
    n.to_string()
}

fn track_count_text(n: i64) -> String {
    format!("{} track{}", format_number(n), if n == 1 { "" } else { "s" })
}
fn album_count_text(n: i64) -> String {
    format!("{} album{}", format_number(n), if n == 1 { "" } else { "s" })
}
fn play_count_text(n: i64) -> String {
    format!("{} play{}", format_number(n), if n == 1 { "" } else { "s" })
}

/// `datetime.fromtimestamp(ts).year` — UTC civil year (parity host TZ=UTC).
pub fn year_of(ts: i64) -> i64 {
    let days = ts.div_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y + 1
    } else {
        y
    }
}

/// serialize_for_card(artist) — exact `props_to_remove` from serializers/artist.py
/// (plus every `is_*` key), then `type = "artist"`.
pub fn artist_card(a: &Artist) -> Value {
    let mut d = a.to_json(0);
    if let Some(m) = d.as_object_mut() {
        for k in [
            "is_favorite",
            "trackcount",
            "duration",
            "albumcount",
            "playcount",
            "playduration",
            "lastplayed",
            "id",
            "genres",
            "genrehashes",
            "extra",
            "created_date",
            "date",
            "fav_userids",
            "_score",
        ] {
            m.remove(k);
        }
        m.insert("type".to_string(), json!("artist"));
    }
    d
}

/// serialize_for_card(album) — `album_serializer` drops every `is_*` key first,
/// then the exact `props_to_remove` set from serializers/album.py.
pub fn album_card(a: &Album) -> Value {
    let mut d = a.to_json(0);
    if let Some(m) = d.as_object_mut() {
        for k in [
            "is_favorite",
            "duration",
            "count",
            "artisthashes",
            "albumartists_hashes",
            "created_date",
            "og_title",
            "base_title",
            "genres",
            "playcount",
            "trackcount",
            "type",
            "playduration",
            "genrehashes",
            "fav_userids",
            "extra",
            "id",
            "lastplayed",
            "weakhash",
        ] {
            m.remove(k);
        }
        m.insert("type".to_string(), json!("album"));
    }
    d
}

/// get_sort_name(name, articles) + the casefold branch of the two sortfuncs.
/// The article-aware flag is off by default (UserConfig) — raw-key fallback.
fn artist_sort_key(name: &str, article_aware: bool) -> String {
    if !article_aware {
        return casefold(name);
    }
    if !name.contains(' ') {
        return name.to_string();
    }
    let mut parts = name.splitn(2, ' ');
    let first = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    if ["the", "a", "an"].contains(&casefold(first).as_str()) {
        rest.to_string()
    } else {
        name.to_string()
    }
}

fn sort_albums(items: &mut [Album], sortkey: &str, reverse: bool, article_aware: bool) {
    match sortkey {
        "trackcount" => sort_plain_by(items, |a| a.trackcount, reverse),
        "duration" => sort_plain_by(items, |a| a.duration, reverse),
        "created_date" => sort_plain_by(items, |a| a.created_date, reverse),
        "playcount" => sort_plain_by(items, |a| a.playcount, reverse),
        "playduration" => sort_plain_by(items, |a| a.playduration, reverse),
        "lastplayed" => sort_plain_by(items, |a| a.lastplayed, reverse),
        "date" => sort_plain_by(items, |a| a.date, reverse),
        "albumartists" => {
            // natsort+casefold raises AttributeError on the list attribute upstream
            // → live falls back to plain sorted() with the name key.
            sort_plain_by(
                items,
                |a| {
                    artist_sort_key(
                        a.albumartists.first().map(|x| x.name.as_str()).unwrap_or(""),
                        article_aware,
                    )
                },
                reverse,
            )
        }
        "title" => sort_nat_by(items, |a| casefold(&a.title), reverse),
        _ => {}
    }
}

fn sort_artists(items: &mut [Artist], sortkey: &str, reverse: bool, article_aware: bool) {
    match sortkey {
        "trackcount" => sort_plain_by(items, |a| a.trackcount, reverse),
        "albumcount" => sort_plain_by(items, |a| a.albumcount, reverse),
        "duration" => sort_plain_by(items, |a| a.duration, reverse),
        "created_date" => sort_plain_by(items, |a| a.created_date, reverse),
        "playcount" => sort_plain_by(items, |a| a.playcount, reverse),
        "playduration" => sort_plain_by(items, |a| a.playduration, reverse),
        "lastplayed" => sort_plain_by(items, |a| a.lastplayed, reverse),
        "name" => sort_nat_by(items, |a| artist_sort_key(&a.name, article_aware), reverse),
        _ => {}
    }
}

fn set_help(d: &mut Value, v: Value) {
    if let Some(m) = d.as_object_mut() {
        m.insert("help_text".to_string(), v);
    }
}

/// GET /getall/songs — flat track list (`serialize_tracks` default: no disc/track).
pub async fn get_all_songs(
    State(st): State<AppState>,
    Query(q): Query<GetAllItemsQuery>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let userid = user_id(&ext);
    let mut items: Vec<Track> = st.library.tracks.clone();
    let total = items.len() as i64;
    let reverse = q.reverse == "1";

    match q.sortby.as_str() {
        "created_date" => sort_f64_by(&mut items, |t| t.last_mod, reverse),
        "album" => sort_nat_by(&mut items, |t| casefold(&t.album), reverse),
        "artist" => sort_nat_by(
            &mut items,
            |t| casefold(t.artists.first().map(|a| a.name.as_str()).unwrap_or("")),
            reverse,
        ),
        "duration" => sort_plain_by(&mut items, |t| t.duration, reverse),
        _ => sort_nat_by(&mut items, |t| casefold(&t.title), reverse),
    }

    let page = py_slice(items, q.start, q.limit);
    let out: Vec<Value> = page.iter().map(|t| t.to_json(userid, true)).collect();
    Json(json!({ "items": out, "total": total })).into_response()
}

/// GET /getall/<itemtype> — albums | artists only (upstream crashes otherwise).
pub async fn get_all_items(
    Path(itemtype): Path<String>,
    State(st): State<AppState>,
    Query(q): Query<GetAllItemsQuery>,
    _ext: Option<Extension<CurrentUser>>,
) -> Response {
    let is_albums = itemtype == "albums";
    let is_artists = itemtype == "artists";
    let article_aware = st.settings.from_settings().artist_article_aware_sorting;
    let sortkey = q.sortby.as_str();
    let reverse = q.reverse == "1";
    let now = now_ts();

    if is_albums {
        let mut items: Vec<Album> = st.library.albums.clone();
        let total = items.len() as i64;
        sort_albums(&mut items, sortkey, reverse, article_aware);
        let page = py_slice(items, q.start, q.limit);
        let mut out: Vec<Value> = Vec::new();
        for item in page.iter() {
            let mut d = album_card(item);
            match sortkey {
                "date" => set_help(&mut d, json!(year_of(item.date))),
                "created_date" => set_help(
                    &mut d,
                    json!(timestamp_to_time_passed(item.created_date, now)),
                ),
                "trackcount" => set_help(&mut d, json!(track_count_text(item.trackcount))),
                "duration" => set_help(&mut d, json!(seconds_to_time_string(item.duration))),
                "playcount" => set_help(&mut d, json!(play_count_text(item.playcount))),
                "lastplayed" => {
                    if item.playduration == 0 {
                        set_help(&mut d, json!("Never played"));
                    } else {
                        set_help(&mut d, json!(timestamp_to_time_passed(item.lastplayed, now)));
                    }
                }
                "playduration" => {
                    set_help(&mut d, json!(seconds_to_time_string(item.playduration)))
                }
                _ => {}
            }
            out.push(d);
        }
        return Json(json!({ "items": out, "total": total })).into_response();
    }

    if is_artists {
        let mut items: Vec<Artist> = st.library.artists.clone();
        let total = items.len() as i64;
        sort_artists(&mut items, sortkey, reverse, article_aware);
        let page = py_slice(items, q.start, q.limit);
        let mut out: Vec<Value> = Vec::new();
        for item in page.iter() {
            let mut d = artist_card(item);
            match sortkey {
                "created_date" => set_help(
                    &mut d,
                    json!(timestamp_to_time_passed(item.created_date, now)),
                ),
                "trackcount" => set_help(&mut d, json!(track_count_text(item.trackcount))),
                "albumcount" => set_help(&mut d, json!(album_count_text(item.albumcount))),
                "duration" => set_help(&mut d, json!(seconds_to_time_string(item.duration))),
                "playcount" => set_help(&mut d, json!(play_count_text(item.playcount))),
                "lastplayed" => {
                    if item.playduration == 0 {
                        set_help(&mut d, json!("Never played"));
                    } else {
                        set_help(&mut d, json!(timestamp_to_time_passed(item.lastplayed, now)));
                    }
                }
                "playduration" => {
                    set_help(&mut d, json!(seconds_to_time_string(item.playduration)))
                }
                _ => {}
            }
            out.push(d);
        }
        return Json(json!({ "items": out, "total": total })).into_response();
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "msg": "Internal Server Error" })),
    )
        .into_response()
}
