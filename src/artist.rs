//! Port of swingmusic/api/artist.py — all 4 `/artist` routes.
//!
//! Faithful to the live fork:
//!  - GET /artist/{artisthash}          get_artist
//!  - GET /artist/{artisthash}/albums   get_artist_albums
//!  - GET /artist/{artisthash}/tracks   get_all_artist_tracks
//!  - GET /artist/{artisthash}/similar  get_similar_artists
//!
//! Query schemas mirror api/apischemas.py aliases + defaults; serializer output
//! mirrors serializers/{artist,album,track}.py; store access goes through the
//! set-order-faithful Library helpers (cset.rs reproduces CPython set order).

use std::collections::HashSet;

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::album::get_track_group_stats;
use crate::cset::PySetStr;
use crate::getall::{album_card, artist_card, user_id};
use crate::models::{Album, Artist, Track};
use crate::natsort::{casefold, natsorted_by};
use crate::{AppState, CurrentUser};

// ---------------------------------------------------------------------------
// query schemas (api/apischemas.py + api/artist.py)
// ---------------------------------------------------------------------------

/// `TrackLimitSchema.limit` (alias `tracklimit`) — live default 5.
fn default_track_limit() -> i64 {
    5
}

/// `GetArtistQuery.albumlimit` — live default 7.
fn default_artist_albumlimit() -> i64 {
    7
}

/// `AlbumLimitSchema` / `ArtistLimitSchema` default = Defaults.API_CARD_LIMIT.
fn default_api_card_limit() -> i64 {
    crate::getall::API_CARD_LIMIT
}

/// pydantic-bool parity for query params ("True"/"true"/"1"/"yes"/"on"/"t").
/// Python `requests` encodes `True` as `True`, so the lenient parse matters.
fn de_py_bool<'de, D>(de: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(de)?;
    Ok(match v {
        Some(Value::Bool(b)) => b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => matches!(
            s.to_lowercase().as_str(),
            "true" | "1" | "yes" | "on" | "t" | "y"
        ),
        _ => false,
    })
}

/// `GetArtistQuery(TrackLimitSchema, GetArtistAlbumsQuery)` — live fields:
/// tracklimit (alias of TrackLimitSchema.limit) default 5, albumlimit default 7
/// (explicit on GetArtistQuery), all default False. (`ignore_albumlimit` from
/// the old stub does not exist in the live source.)
#[derive(Deserialize)]
pub struct GetArtistQuery {
    #[serde(default = "default_track_limit")]
    pub tracklimit: i64,
    #[serde(default = "default_artist_albumlimit")]
    pub albumlimit: i64,
    #[serde(default, deserialize_with = "de_py_bool")]
    pub all: bool,
}

/// `GetArtistAlbumsQuery(AlbumLimitSchema)` — albumlimit (alias of `limit`)
/// default API_CARD_LIMIT, all default False.
#[derive(Deserialize)]
pub struct GetArtistAlbumsQuery {
    #[serde(default = "default_api_card_limit")]
    pub albumlimit: i64,
    #[serde(default, deserialize_with = "de_py_bool")]
    pub all: bool,
}

/// `ArtistLimitSchema` — artistlimit (alias of `limit`), default API_CARD_LIMIT.
#[derive(Deserialize)]
pub struct ArtistLimitSchema {
    #[serde(default = "default_api_card_limit")]
    pub limit: i64,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Python `seq[:k]` end index: k<0 keeps all but the last |k| elements.
fn slice_end(len: usize, k: i64) -> usize {
    let n = len as i64;
    let end = if k < 0 { (n + k).max(0) } else { k.min(n) };
    end as usize
}

fn slice_head<T: Clone>(items: &[T], k: i64) -> Vec<T> {
    items[..slice_end(items.len(), k)].to_vec()
}

/// CPython set-iteration order of a persistent Python set built by these adds
/// (the sets are built by `.add()` in creation order — cset.rs reproduces the
/// gold PYTHONHASHSEED layout).
fn set_order(items: &[String]) -> Vec<String> {
    let mut set = PySetStr::gold();
    for x in items {
        set.add(x);
    }
    set.iter_order().map(str::to_string).collect()
}

/// `sortlib.sort_tracks(tracks, key="playcount", reverse=True)`: stable natural
/// sort on casefolded title, then a stable descending playcount sort.
fn sort_tracks_by_playcount(tracks: &mut Vec<Track>) {
    natsorted_by(tracks, |t| casefold(&t.title));
    tracks.sort_by(|a, b| b.playcount.cmp(&a.playcount));
}

/// serialize_track(track) + the artist-endpoint help_text (raw count, no
/// thousands separator — unlike the getall card helper).
fn track_item(t: &Track, userid: i64) -> Value {
    let mut d = t.to_json(userid, true);
    let help = if t.playcount == 0 {
        "unplayed".to_string()
    } else {
        format!(
            "{} play{}",
            t.playcount,
            if t.playcount == 1 { "" } else { "s" }
        )
    };
    if let Value::Object(ref mut m) = d {
        m.insert("help_text".to_string(), json!(help));
    }
    d
}

/// Body of `get_artist_albums` — shared by the route and the get_artist
/// internal call (which sets `query.limit = query.albumlimit` first).
fn artist_albums_payload(st: &AppState, artist: &Artist, limit: i64, return_all: bool) -> Value {
    let artisthash = artist.artisthash.as_str();

    // AlbumStore.get_albums_by_hashes(entry.albumhashes) — set iteration order.
    let albumhash_order = set_order(&artist.albumhashes);
    let mut albums: Vec<Album> = st.library.get_albums_by_hashes(&albumhash_order);

    // TrackStore.get_tracks_by_trackhashes(entry.trackhashes) — set order.
    let tracks: Vec<Track> = st.library.get_tracks_by_trackhashes(&artist.trackhashes);

    // missing_albumhashes = {t.albumhash for t in tracks if hash not in albums}
    let missing_order: Vec<String> = {
        let have: HashSet<&str> = albums.iter().map(|a| a.albumhash.as_str()).collect();
        let mut missing = PySetStr::gold();
        for t in &tracks {
            if !have.contains(t.albumhash.as_str()) {
                missing.add(&t.albumhash);
            }
        }
        missing.iter_order().map(str::to_string).collect()
    };
    for a in st.library.get_albums_by_hashes(&missing_order) {
        albums.push(a);
    }

    // albumdict = {a.albumhash: a for a in albums} — first-occurrence order.
    let mut albumdict: Vec<Album> = Vec::new();
    for a in albums {
        if !albumdict.iter().any(|x| x.albumhash == a.albumhash) {
            albumdict.push(a);
        }
    }

    // groupby(tracks, key=albumhash) — CONSECUTIVE runs only; check_type
    // mutates the album (a hash spanning several runs sees several calls, the
    // last run wins) — same side effects as the live dataclass.
    let show_singles = st.settings.from_settings().show_albums_as_singles;
    let mut i = 0usize;
    while i < tracks.len() {
        let start = i;
        while i < tracks.len() && tracks[i].albumhash == tracks[start].albumhash {
            i += 1;
        }
        let h = tracks[start].albumhash.clone();
        let run: Vec<Track> = tracks[start..i].to_vec();
        if let Some(alb) = albumdict.iter_mut().find(|x| x.albumhash == h) {
            alb.check_type(&run, show_singles);
        }
    }

    // all_albums = sorted(dict.values(), key=date, reverse=True) — stable.
    let mut all_albums: Vec<Album> = albumdict;
    all_albums.sort_by(|a, b| b.date.cmp(&a.date));

    let limit = if return_all {
        all_albums.len() as i64
    } else {
        limit
    };

    // Buckets in live check order: singles_and_eps / compilations /
    // appearances / albums.
    let missing_hashes: HashSet<&str> = missing_order.iter().map(|s| s.as_str()).collect();
    let mut bucket_albums: Vec<&Album> = Vec::new();
    let mut appearances: Vec<&Album> = Vec::new();
    let mut compilations: Vec<&Album> = Vec::new();
    let mut singles_and_eps: Vec<&Album> = Vec::new();
    for a in &all_albums {
        if a.r#type == "single" || a.r#type == "ep" {
            singles_and_eps.push(a);
        } else if a.r#type == "compilation" {
            compilations.push(a);
        } else if missing_hashes.contains(a.albumhash.as_str())
            || !a.artisthashes.iter().any(|x| x == artisthash)
        {
            appearances.push(a);
        } else {
            bucket_albums.push(a);
        }
    }

    let cards = |items: &[&Album]| -> Vec<Value> {
        slice_head(items, limit)
            .iter()
            .map(|a| album_card(*a))
            .collect()
    };

    json!({
        "albums": cards(&bucket_albums),
        "appearances": cards(&appearances),
        "compilations": cards(&compilations),
        "singles_and_eps": cards(&singles_and_eps),
        "artistname": artist.name,
    })
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// GET /artist/{artisthash} — artist info + tracks + albums + stats.
pub async fn get_artist(
    State(st): State<AppState>,
    Path(artisthash): Path<String>,
    Query(q): Query<GetArtistQuery>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let userid = user_id(&ext);

    let artist = match st.library.artists.iter().find(|a| a.artisthash == artisthash) {
        Some(a) => a.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Artist not found" })),
            )
                .into_response()
        }
    };

    let mut tracks = st.library.get_tracks_by_trackhashes(&artist.trackhashes);
    sort_tracks_by_playcount(&mut tracks);
    let tcount = tracks.len() as i64;

    let mut limit = q.tracklimit;
    if artist.albumcount == 0 && tcount < 10 {
        limit = tcount;
    }

    // datetime.fromtimestamp(artist.date).year — out-of-datetime-range → 0
    // (the live `except ValueError: year = 0`).
    let year = {
        let y = crate::getall::year_of(artist.date);
        if (1..=9999).contains(&y) {
            y
        } else {
            0
        }
    };

    let mut genres: Vec<Value> = artist.genres.iter().map(|g| g.to_value()).collect();
    if year != 0 {
        let decade = year.div_euclid(10) * 10;
        let ds = decade.to_string();
        let decade = format!("{}s", ds.get(2..).unwrap_or(""));
        genres.insert(0, json!({ "name": decade, "genrehash": decade }));
    }

    let stats = get_track_group_stats(&tracks, false);
    let duration: i64 = tracks.iter().map(|t| t.duration).sum();

    let shown = if limit != 0 && limit != -1 {
        slice_head(&tracks, limit)
    } else {
        tracks.clone()
    };

    let track_items: Vec<Value> = shown.iter().map(|t| track_item(t, userid)).collect();

    // query.limit = query.albumlimit; albums = get_artist_albums(path, query)
    let albums = artist_albums_payload(&st, &artist, q.albumlimit, q.all);

    let mut artist_payload = artist_card(&artist);
    if let Value::Object(ref mut m) = artist_payload {
        m.insert("duration".to_string(), json!(duration));
        m.insert("trackcount".to_string(), json!(tcount));
        m.insert("albumcount".to_string(), json!(artist.albumcount));
        m.insert("genres".to_string(), json!(genres));
        m.insert(
            "is_favorite".to_string(),
            json!(artist.fav_userids.contains(&userid)),
        );
    }

    Json(json!({
        "artist": artist_payload,
        "tracks": track_items,
        "albums": albums,
        "stats": stats,
    }))
    .into_response()
}

/// GET /artist/{artisthash}/albums — bucketed album cards.
pub async fn get_artist_albums(
    State(st): State<AppState>,
    Path(artisthash): Path<String>,
    Query(q): Query<GetArtistAlbumsQuery>,
) -> Response {
    let artist = match st.library.artists.iter().find(|a| a.artisthash == artisthash) {
        Some(a) => a.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Artist not found" })),
            )
                .into_response()
        }
    };

    Json(artist_albums_payload(&st, &artist, q.albumlimit, q.all)).into_response()
}

/// GET /artist/{artisthash}/tracks — bare list; unknown artist → [].
pub async fn get_all_artist_tracks(
    State(st): State<AppState>,
    Path(artisthash): Path<String>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let userid = user_id(&ext);

    let mut tracks = match st.library.artists.iter().find(|a| a.artisthash == artisthash) {
        Some(a) => st.library.get_tracks_by_trackhashes(&a.trackhashes),
        None => Vec::new(),
    };
    sort_tracks_by_playcount(&mut tracks);

    let items: Vec<Value> = tracks.iter().map(|t| track_item(t, userid)).collect();
    Json(Value::Array(items)).into_response()
}

/// GET /artist/{artisthash}/similar — artist cards; unknown hash → [].
pub async fn get_similar_artists(
    State(st): State<AppState>,
    Path(artisthash): Path<String>,
    Query(q): Query<ArtistLimitSchema>,
) -> Response {
    let hashes = match st.library.similar_artist_hashes(&artisthash) {
        Some(h) => h,
        None => return Json(json!([])).into_response(),
    };

    // SimilarArtist.get_artist_hash_set() — set order of the stored list.
    let hash_order = set_order(&hashes);
    let similar = st.library.get_artists_by_hashes(&hash_order);

    let limit = q.limit;
    let similar: Vec<Artist> = if similar.len() as i64 > limit {
        let n = (limit.max(0) as usize).min(similar.len());
        let mut rng = rand::thread_rng();
        rand::seq::SliceRandom::choose_multiple(similar.as_slice(), &mut rng, n)
            .into_iter()
            .cloned()
            .collect()
    } else {
        similar
    };

    let cards: Vec<Value> = slice_head(&similar, limit)
        .iter()
        .map(|a| artist_card(a))
        .collect();
    Json(Value::Array(cards)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    fn q(uri: &str) -> GetArtistQuery {
        let uri: Uri = uri.parse().unwrap();
        Query::<GetArtistQuery>::try_from_uri(&uri).unwrap().0
    }

    /// Live defaults: tracklimit 5, albumlimit 7, all False.
    #[test]
    fn get_artist_query_defaults() {
        let q = q("/artist/abc");
        assert_eq!(q.tracklimit, 5);
        assert_eq!(q.albumlimit, 7);
        assert!(!q.all);
    }

    /// Python `requests` sends `True`; pydantic accepts {true,1,yes,on,t,y}.
    #[test]
    fn get_artist_query_py_bools() {
        for s in ["True", "true", "1", "yes", "on", "t", "y"] {
            assert!(q(&format!("/artist/abc?all={s}")).all, "all={s}");
        }
        for s in ["False", "false", "0", "no", "off", ""] {
            assert!(!q(&format!("/artist/abc?all={s}")).all, "all={s}");
        }
    }

    #[test]
    fn get_artist_query_values() {
        let q = q("/artist/abc?tracklimit=13&albumlimit=3&all=True");
        assert_eq!(q.tracklimit, 13);
        assert_eq!(q.albumlimit, 3);
        assert!(q.all);
    }

    #[test]
    fn albums_query_defaults() {
        let uri: Uri = "/artist/abc/albums".parse().unwrap();
        let q = Query::<GetArtistAlbumsQuery>::try_from_uri(&uri).unwrap().0;
        assert_eq!(q.albumlimit, crate::getall::API_CARD_LIMIT);
        assert!(!q.all);
    }

    #[test]
    fn artist_limit_default() {
        let uri: Uri = "/artist/abc/similar".parse().unwrap();
        let q = Query::<ArtistLimitSchema>::try_from_uri(&uri).unwrap().0;
        assert_eq!(q.limit, crate::getall::API_CARD_LIMIT);
    }
}
