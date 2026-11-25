//! Port of swingmusic/api/folder.py — all 4 `/folder` routes.
//! 1:1 with livesrc api/folder.py + lib/folderslib.py + lib/sortlib.py
//! (live fork v2.1.14, read 2026-11-24).
#![allow(dead_code)]

use std::cmp::Ordering;
use std::path::{Component, Path, PathBuf};

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::models::{Folder, Track};
use crate::natsort::casefold;
use crate::{AppState, CurrentUser};

// ---------------------------------------------------------------------------
// request payloads — parity with the pydantic models in api/folder.py
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FolderTree {
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default = "default_sorttracksby")]
    pub sorttracksby: String,
    #[serde(default)]
    pub tracksort_reverse: bool,
    #[serde(default = "default_sortfoldersby")]
    pub sortfoldersby: String,
    #[serde(default)]
    pub foldersort_reverse: bool,
    #[serde(default)]
    pub start: i64,
    #[serde(default = "default_page_limit")]
    pub limit: i64,
    #[serde(default)]
    pub tracks_only: bool,
}

fn default_folder() -> String {
    "$home".to_string()
}
fn default_sorttracksby() -> String {
    "default".to_string()
}
fn default_sortfoldersby() -> String {
    "lastmod".to_string()
}
fn default_page_limit() -> i64 {
    50
}

/// `class DirBrowserBody(BaseModel): folder: str = "$home"`
#[derive(Deserialize)]
pub struct DirBrowserBody {
    #[serde(default = "default_folder")]
    pub folder: String,
}

#[derive(Deserialize)]
pub struct ShowInFilesQuery {
    #[serde(default)]
    pub path: String,
}

#[derive(Deserialize)]
pub struct TracksInPathQuery {
    #[serde(default)]
    pub path: String,
}

// ---------------------------------------------------------------------------
// path helpers (python pathlib parity)
// ---------------------------------------------------------------------------

fn as_posix(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `Path(x).as_posix() + "/"` without doubling an existing trailing slash.
fn posix_trim(p: &Path) -> String {
    let s = as_posix(p);
    if s.len() > 1 {
        s.trim_end_matches('/').to_string()
    } else {
        s
    }
}

/// `pathlib.Path(x).resolve()` — non-strict: canonicalize when the path exists,
/// otherwise lexically normalise to an absolute path.
fn py_resolve(p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|d| d.join(&path))
            .unwrap_or(path)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// `settings.Paths().USER_HOME_DIR.as_posix()`
fn user_home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// api/folder.py `is_path_within_root_dirs()` — traversal guard.
fn is_path_within_root_dirs(filepath: &str, root_dirs: &[String]) -> bool {
    let resolved = py_resolve(filepath);

    for rd in root_dirs {
        let root_path = if rd == "$home" {
            match user_home_dir() {
                Some(h) => py_resolve(&as_posix(&h)),
                None => continue,
            }
        } else {
            py_resolve(rd)
        };

        // `resolved_path == root_path or root_path in resolved_path.parents`
        if resolved == root_path || resolved.starts_with(&root_path) {
            return true;
        }
    }

    false
}

/// `get_all_drives()` — psutil.disk_partitions(all=True) mountpoints.
fn get_all_drives(is_win: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    if is_win {
        for c in b'A'..=b'Z' {
            let p = format!("{}:\\", c as char);
            if Path::new(&p).exists() {
                out.push(format!("{}/", c as char));
            }
        }
        return out;
    }

    if let Ok(txt) = std::fs::read_to_string("/proc/self/mounts") {
        for line in txt.lines() {
            let mut it = line.split_whitespace();
            let _dev = it.next();
            if let Some(mp) = it.next() {
                let mp = mp.replace("\\040", " ");
                if !out.contains(&mp) {
                    out.push(mp);
                }
            }
        }
    }

    const REMOVE: [&str; 9] = [
        "/boot", "/tmp", "/snap", "/var", "/sys", "/proc", "/etc", "/run", "/dev",
    ];
    out.retain(|d| !REMOVE.iter().any(|r| d.starts_with(r)));
    out
}

// ---------------------------------------------------------------------------
// small shared helpers
// ---------------------------------------------------------------------------

fn db(st: &AppState) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(&st.paths.app_db_path)?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    Ok(conn)
}

fn internal() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Internal server error" })),
    )
        .into_response()
}

fn is_admin(ext: &Option<Extension<CurrentUser>>) -> bool {
    let Some(Extension(cu)) = ext.as_ref() else {
        return false;
    };
    let Some(roles) = cu.0.get("roles") else {
        return false;
    };
    if let Some(s) = roles.as_str() {
        return s.split(',').any(|r| r.trim() == "admin");
    }
    if let Some(a) = roles.as_array() {
        return a.iter().any(|v| v.as_str() == Some("admin"));
    }
    false
}

/// FolderStore.filepaths — every indexed track filepath.
fn all_filepaths(tracks: &[Track]) -> Vec<String> {
    tracks.iter().map(|t| t.filepath.clone()).collect()
}

/// FolderStore.get_tracks_by_filepaths()
fn tracks_by_filepaths(tracks: &[Track], files: &[String]) -> Vec<Track> {
    let mut out: Vec<Track> = Vec::new();
    for f in files {
        for t in tracks.iter().filter(|t| &t.filepath == f) {
            if !out.iter().any(|x| x.filepath == t.filepath) {
                out.push(t.clone());
            }
        }
    }
    out
}

/// TrackStore.get_tracks_by_trackhashes() — order follows the requested hashes.
fn tracks_by_trackhashes(tracks: &[Track], hashes: &[String]) -> Vec<Track> {
    let mut out: Vec<Track> = Vec::new();
    for h in hashes {
        for t in tracks.iter().filter(|t| &t.trackhash == h) {
            if !out.iter().any(|x| x.filepath == t.filepath) {
                out.push(t.clone());
            }
        }
    }
    out
}

fn ser_tracks(tracks: &[Track], userid: i64) -> Vec<Value> {
    tracks.iter().map(|t| t.to_json(userid, true)).collect()
}

fn folders_json(folders: &[Folder]) -> Vec<Value> {
    folders
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "path": f.path,
                "is_sym": f.is_sym,
                "trackcount": f.trackcount,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// lib/folderslib.py
// ---------------------------------------------------------------------------

/// `create_folder()` — name, posix path + "/", is_sym, trackcount.
fn create_folder(path: &str, trackcount: i64) -> Folder {
    let pb = PathBuf::from(path);
    let name = pb
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let is_sym = std::fs::symlink_metadata(&pb)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

    Folder {
        name,
        path: format!("{}/", posix_trim(&pb)),
        is_sym,
        trackcount,
    }
}

/// `count_filepaths_in_dir()` — count of indexed filepaths under `dirpath`.
fn count_filepaths_in_dir(filepaths: &[String], dirpath: &str) -> i64 {
    filepaths
        .iter()
        .filter(|p| p.starts_with(dirpath))
        .count() as i64
}

/// `get_folders()` — drop folders with no indexed tracks.
fn get_folders(filepaths: &[String], paths: &[String]) -> Vec<Folder> {
    paths
        .iter()
        .filter_map(|p| {
            let count = count_filepaths_in_dir(filepaths, p);
            if count > 0 {
                Some(create_folder(p, count))
            } else {
                None
            }
        })
        .collect()
}

/// utils/filesystem.py SUPPORTED_FILES (with leading dot, as `entry.suffix`).
const SUPPORTED_FILES: [&str; 9] = [
    ".flac", ".mp3", ".wav", ".m4a", ".ogg", ".wma", ".opus", ".alac", ".aiff",
];

fn get_files_and_dirs(
    tracks: &[Track],
    filepaths: &[String],
    path: &Path,
    start: i64,
    limit: i64,
    tracksortby: &str,
    foldersortby: &str,
    tracksort_reverse: bool,
    foldersort_reverse: bool,
    tracks_only: bool,
    userid: i64,
) -> Value {
    get_files_and_dirs_depth(
        tracks,
        filepaths,
        path,
        start,
        limit,
        tracksortby,
        foldersortby,
        tracksort_reverse,
        foldersort_reverse,
        tracks_only,
        userid,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn get_files_and_dirs_depth(
    tracks_lib: &[Track],
    filepaths: &[String],
    path: &Path,
    start: i64,
    limit_in: i64,
    tracksortby: &str,
    foldersortby: &str,
    tracksort_reverse: bool,
    foldersort_reverse: bool,
    tracks_only: bool,
    userid: i64,
    depth: usize,
) -> Value {
    if depth > 128 {
        return json!({
            "path": as_posix(path),
            "tracks": [],
            "folders": [],
            "total": 0,
        });
    }

    // if file or non-existent
    if !path.exists() || !path.is_dir() {
        return json!({
            "path": as_posix(path),
            "tracks": [],
            "folders": [],
            "total": 0,
        });
    }

    let mut limit = limit_in;
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();

    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let md = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            if md.is_dir() {
                // hidden dirs: `not entry.stem.startswith(".")`
                if !name.starts_with('.') {
                    // trailing slash avoids "/Music" matching "/Music Videos"
                    dirs.push(format!("{}/", posix_trim(&p)));
                }
            } else if md.is_file() {
                let ext = match name.rfind('.') {
                    Some(i) if i > 0 => name[i..].to_lowercase(),
                    _ => String::new(),
                };
                if SUPPORTED_FILES.contains(&ext.as_str()) {
                    files.push(as_posix(&p));
                }
            }
        }
    }

    // convert files to tracks
    let mut page: Vec<Track> = Vec::new();
    if !files.is_empty() {
        if limit == -1 {
            limit = files.len() as i64;
        }

        let mut found = tracks_by_filepaths(tracks_lib, &files);
        sort_tracks(&mut found, tracksortby, tracksort_reverse);

        let s = start.max(0) as usize;
        let e = (start + limit).max(0) as usize;
        page = if s < found.len() {
            found[s..e.min(found.len())].to_vec()
        } else {
            Vec::new()
        };
    }

    let mut folders: Vec<Folder> = Vec::new();
    if !tracks_only {
        folders = get_folders(filepaths, &dirs);
        sort_folders(&mut folders, foldersortby, foldersort_reverse);
    }

    if folders.len() == 1 && page.is_empty() {
        // skip through a single empty folder, recursively
        let next = PathBuf::from(&folders[0].path);
        return get_files_and_dirs_depth(
            tracks_lib,
            filepaths,
            &next,
            start,
            limit,
            tracksortby,
            foldersortby,
            tracksort_reverse,
            foldersort_reverse,
            tracks_only,
            userid,
            depth + 1,
        );
    }

    json!({
        "path": as_posix(path),
        "tracks": ser_tracks(&page, userid),
        "folders": folders_json(&folders),
        "total": files.len(),
    })
}

// ---------------------------------------------------------------------------
// lib/sortlib.py
// ---------------------------------------------------------------------------

fn mtime_of(path: &str) -> f64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `sort_tracks(tracks, key, reverse)`
fn sort_tracks(tracks: &mut Vec<Track>, key: &str, reverse: bool) {
    // primary sort
    if key == "default" {
        tracks.sort_by(|a, b| {
            a.last_mod
                .partial_cmp(&b.last_mod)
                .unwrap_or(Ordering::Equal)
        });
    } else {
        tracks.sort_by(|a, b| casefold(&a.title).cmp(&casefold(&b.title)));
    }

    if key == "disc" {
        // group into albums, then sort each group by disc/track no
        tracks.sort_by(|a, b| casefold(&a.album).cmp(&casefold(&b.album)));

        let mut groups: Vec<(String, Vec<Track>)> = Vec::new();
        for t in tracks.drain(..) {
            match groups.last_mut() {
                Some((h, v)) if *h == t.albumhash => v.push(t),
                _ => groups.push((t.albumhash.clone(), vec![t])),
            }
        }
        for (_, mut g) in groups {
            g.sort_by(|a, b| (a.disc, a.track).cmp(&(b.disc, b.track)));
            tracks.extend(g);
        }
        return;
    }

    if key == "default" || key == "last_mod" || key == "title" {
        if reverse {
            tracks.reverse();
        }
        return;
    }

    let keyf = |t: &Track| -> String {
        match key {
            "album" => casefold(&t.album),
            "artists" => casefold(t.artists.first().map(|a| a.name.as_str()).unwrap_or("")),
            "albumartists" => casefold(
                t.albumartists
                    .first()
                    .map(|a| a.name.as_str())
                    .unwrap_or(""),
            ),
            "bitrate" => format!("{:020}", t.bitrate),
            "date" => format!("{:020}", t.date),
            "duration" => format!("{:020}", t.duration),
            "lastplayed" => format!("{:020}", t.lastplayed),
            "playduration" => format!("{:020}", t.playduration),
            "playcount" => format!("{:020}", t.playcount),
            "filepath" => casefold(&t.filepath),
            _ => casefold(&t.title),
        }
    };

    tracks.sort_by(|a, b| keyf(a).cmp(&keyf(b)));
    if reverse {
        tracks.reverse();
    }
}

/// `sort_folders(folders, key, reverse)`
fn sort_folders(folders: &mut Vec<Folder>, key: &str, reverse: bool) {
    match key {
        "default" => return,
        "lastmod" => folders.sort_by(|a, b| {
            mtime_of(&a.path)
                .partial_cmp(&mtime_of(&b.path))
                .unwrap_or(Ordering::Equal)
        }),
        "trackcount" => folders.sort_by(|a, b| a.trackcount.cmp(&b.trackcount)),
        "name" => folders.sort_by(|a, b| casefold(&a.name).cmp(&casefold(&b.name))),
        _ => folders.sort_by(|a, b| casefold(&a.name).cmp(&casefold(&b.name))),
    }

    if reverse {
        folders.reverse();
    }
}

// ---------------------------------------------------------------------------
// userdata: playlists + favorites
// ---------------------------------------------------------------------------

struct PlaylistRow {
    id: i64,
    name: String,
    hashes: Vec<String>,
    last_updated: String,
}

fn parse_hashes(raw: &str) -> Vec<String> {
    if let Ok(v) = serde_json::from_str::<Vec<String>>(raw) {
        return v;
    }
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// PlaylistTable.get_all() — current user's playlists; a missing table yields none.
fn playlist_rows(st: &AppState, userid: i64) -> Vec<PlaylistRow> {
    let mut out: Vec<PlaylistRow> = Vec::new();

    let Ok(conn) = db(st) else {
        return out;
    };
    let Ok(mut stmt) =
        conn.prepare("SELECT id, name, trackhashes, last_updated FROM playlist WHERE userid = ?1")
    else {
        return out;
    };

    let rows = stmt.query_map([userid], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1).unwrap_or_default(),
            r.get::<_, String>(2).unwrap_or_default(),
            r.get::<_, String>(3).unwrap_or_default(),
        ))
    });

    if let Ok(rows) = rows {
        for row in rows.flatten() {
            out.push(PlaylistRow {
                id: row.0,
                name: row.1,
                hashes: parse_hashes(&row.2),
                last_updated: row.3,
            });
        }
    }

    out
}

/// FavoritesTable.get_fav_tracks() — backed by `Track.fav_userids`.
fn fav_tracks(tracks: &[Track], userid: i64) -> Vec<Track> {
    tracks
        .iter()
        .filter(|t| {
            if userid > 0 {
                t.fav_userids.contains(&userid)
            } else {
                !t.fav_userids.is_empty()
            }
        })
        .cloned()
        .collect()
}

/// python slice `[start : start + limit]`, `limit <= 0` => to the end.
fn py_page<T: Clone>(items: &[T], start: i64, limit: i64) -> Vec<T> {
    let s = start.max(0) as usize;
    if s >= items.len() {
        return Vec::new();
    }
    if limit <= 0 {
        return items[s..].to_vec();
    }
    let e = (start + limit).max(0) as usize;
    items[s..e.min(items.len())].to_vec()
}

/// `showinfm.show_in_file_manager()` — host opener.
fn show_in_file_manager(path: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

// ---------------------------------------------------------------------------
// POST /folder
// ---------------------------------------------------------------------------

/// POST /folder — folder tree for a directory / virtual node.
pub async fn get_folder_tree(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Json(body): Json<FolderTree>,
) -> Response {
    let og_req_dir = body.folder.clone();
    let mut req_dir = body.folder.clone();
    let tracks_only = body.tracks_only;
    let userid = crate::getall::user_id(&ext);

    let root_dirs = st.settings.root_dirs();
    let tracks_lib: Vec<Track> = st.library.tracks.clone();
    let filepaths = all_filepaths(&tracks_lib);

    if req_dir == "$home" && root_dirs.iter().any(|r| r == "$home") {
        if let Some(h) = user_home_dir() {
            req_dir = as_posix(&h);
        }
    }

    if req_dir == "$home" {
        let folders = get_folders(&filepaths, &root_dirs);

        // if $home is a single directory, return its contents
        if folders.len() > 1 {
            return Json(json!({
                "folders": folders_json(&folders),
                "tracks": [],
            }))
            .into_response();
        }

        if folders.is_empty() {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "msg": "No home directory found",
                    "folders": [],
                    "tracks": [],
                })),
            )
                .into_response();
        }

        req_dir = folders[0].path.clone();
    }

    if req_dir.starts_with("$playlist") {
        let splits: Vec<&str> = req_dir.split('/').collect();

        if splits.len() == 2 {
            let pid = splits[1].trim();
            let rows = playlist_rows(&st, userid);

            let mut found: Option<PlaylistRow> = None;
            if let Ok(id) = pid.parse::<i64>() {
                found = rows.into_iter().find(|p| p.id == id);
            }
            if found.is_none() {
                let rows = playlist_rows(&st, userid);
                found = rows.into_iter().find(|p| p.name == pid);
            }

            let Some(pl) = found else {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "msg": "Playlist not found",
                        "folders": [],
                        "tracks": [],
                    })),
                )
                    .into_response();
            };

            let hashes = py_page(&pl.hashes, body.start, body.limit);
            let tracks = tracks_by_trackhashes(&tracks_lib, &hashes);

            return Json(json!({
                "path": format!("$playlist/{}", pl.name),
                "folders": [],
                "tracks": ser_tracks(&tracks, userid),
            }))
            .into_response();
        }

        let mut playlists = playlist_rows(&st, userid);
        // datetime desc — the "%Y-%m-%d %H:%M:%S" format sorts lexicographically
        playlists.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));

        let folders: Vec<Value> = playlists
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "path": format!("$playlist/{}", p.id),
                    "trackcount": p.hashes.len() as i64,
                })
            })
            .collect();

        return Json(json!({
            "path": req_dir,
            "folders": folders,
            "tracks": [],
        }))
        .into_response();
    }

    if req_dir == "$favorites" {
        let favs = fav_tracks(&tracks_lib, userid);
        let paged = py_page(&favs, body.start, body.limit);

        return Json(json!({
            "tracks": ser_tracks(&paged, userid),
            "folders": [],
            "path": req_dir,
        }))
        .into_response();
    }

    // Resolve path to prevent directory traversal attacks
    let resolved_path = py_resolve(&req_dir);

    // Validate path is within configured root directories
    if !is_path_within_root_dirs(&as_posix(&resolved_path), &root_dirs) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "folders": [],
                "tracks": [],
                "error": "Path not within allowed directories",
            })),
        )
            .into_response();
    }

    if !resolved_path.exists() || !resolved_path.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "folders": [],
                "tracks": [],
                "error": "Invalid directory",
            })),
        )
            .into_response();
    }

    let mut results = get_files_and_dirs(
        &tracks_lib,
        &filepaths,
        &resolved_path,
        body.start,
        body.limit,
        &body.sorttracksby,
        &body.sortfoldersby,
        body.tracksort_reverse,
        body.foldersort_reverse,
        tracks_only,
        userid,
    );

    if og_req_dir == "$home" && st.settings.show_playlists_in_folder_view() {
        let playlists_total: i64 = playlist_rows(&st, userid)
            .iter()
            .map(|p| p.hashes.len() as i64)
            .sum();
        let favorites_total = fav_tracks(&tracks_lib, userid).len() as i64;

        if let Some(arr) = results.get_mut("folders").and_then(|v| v.as_array_mut()) {
            arr.insert(
                0,
                json!({
                    "name": "Playlists",
                    "path": "$playlists",
                    "trackcount": playlists_total,
                }),
            );
            arr.insert(
                0,
                json!({
                    "name": "Favorites",
                    "path": "$favorites",
                    "trackcount": favorites_total,
                }),
            );
        }
    }

    Json(results).into_response()
}

// ---------------------------------------------------------------------------
// POST /folder/dir-browser
// ---------------------------------------------------------------------------

/// POST /folder/dir-browser — admin directory browser.
pub async fn list_folders(
    State(_st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Json(body): Json<DirBrowserBody>,
) -> Response {
    // `@admin_required()`
    if !is_admin(&ext) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "msg": "Only admins can do that!" })),
        )
            .into_response();
    }

    let mut req_dir = body.folder.trim().to_string();
    let is_win = cfg!(windows);

    if req_dir.is_empty() {
        req_dir = "$home".to_string();
    }

    if req_dir == "$root" {
        let folders: Vec<Value> = get_all_drives(is_win)
            .iter()
            .map(|d| json!({ "name": d, "path": d }))
            .collect();

        return Json(json!({ "folders": folders })).into_response();
    }

    // Resolve path to prevent directory traversal attacks
    if req_dir == "$home" {
        if let Some(h) = user_home_dir() {
            req_dir = as_posix(&h);
        }
    }

    let resolved = py_resolve(&req_dir);

    if !resolved.exists() || !resolved.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "folders": [], "error": "Invalid directory" })),
        )
            .into_response();
    }

    let Ok(rd) = std::fs::read_dir(&resolved) else {
        // `except PermissionError: return {"folders": []}`
        return Json(json!({ "folders": [] })).into_response();
    };

    // only get dirs and remove hidden dirs
    let mut dirs: Vec<Value> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with('$') {
            // ignore windows system folder
            continue;
        }
        if name.starts_with('.') {
            // ignore unix hidden folder
            continue;
        }

        let p = entry.path();
        let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            let child = py_resolve(&as_posix(&p));
            dirs.push(json!({ "name": name, "path": as_posix(&child) }));
        }
    }

    dirs.sort_by(|a, b| {
        casefold(a["name"].as_str().unwrap_or(""))
            .cmp(&casefold(b["name"].as_str().unwrap_or("")))
    });

    let parent = resolved.parent().map(|p| p.to_path_buf()).unwrap_or_default();

    // prepend the parent as "↑", then the current dir
    dirs.insert(0, json!({ "name": "↑", "path": as_posix(&parent) }));
    dirs.insert(
        1,
        json!({ "name": ". (this folder)", "path": as_posix(&resolved) }),
    );

    Json(json!({ "folders": dirs })).into_response()
}

// ---------------------------------------------------------------------------
// GET /folder/show-in-files
// ---------------------------------------------------------------------------

/// GET /folder/show-in-files — reveal path in OS file manager.
pub async fn open_in_file_manager(
    State(st): State<AppState>,
    Query(q): Query<ShowInFilesQuery>,
) -> Response {
    let root_dirs = st.settings.root_dirs();

    // Resolve path to prevent directory traversal
    let resolved_path = py_resolve(&q.path);

    // Validate path is within root directories
    if !is_path_within_root_dirs(&q.path, &root_dirs) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "error": "Path not within allowed directories",
            })),
        )
            .into_response();
    }

    if !resolved_path.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "error": "Path does not exist" })),
        )
            .into_response();
    }

    show_in_file_manager(&as_posix(&resolved_path));

    Json(json!({ "success": true })).into_response()
}

// ---------------------------------------------------------------------------
// GET /folder/tracks/all
// ---------------------------------------------------------------------------

/// GET /folder/tracks/all — all tracks under path (max 300).
pub async fn get_tracks_in_path(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Query(q): Query<TracksInPathQuery>,
) -> Response {
    let userid = crate::getall::user_id(&ext);
    let root_dirs = st.settings.root_dirs();

    // Resolve path to prevent directory traversal
    let resolved_path = py_resolve(&q.path);
    let resolved_str = as_posix(&resolved_path);

    // Validate path is within root directories
    if !is_path_within_root_dirs(&resolved_str, &root_dirs) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "tracks": [],
                "error": "Path not within allowed directories",
            })),
        )
            .into_response();
    }

    // TrackTable.get_tracks_in_path(): filepath contains path, ordered by last_mod
    let mut found: Vec<Track> = st
        .library
        .tracks
        .iter()
        .filter(|t| t.filepath.contains(&resolved_str))
        .cloned()
        .collect();
    found.sort_by(|a, b| {
        a.last_mod
            .partial_cmp(&b.last_mod)
            .unwrap_or(Ordering::Equal)
    });

    // Live re-reads TrackTable rows as fresh dataclasses: the store's
    // create-time hydration (color/blurhash) and create_artists markers
    // (`in_track`) never reach this response (gold: color='' ×300,
    // blurhash='' ×300, no in_track keys). Serialize sanitized clones.
    let tracks: Vec<Value> = found
        .into_iter()
        .filter(|t| Path::new(&t.filepath).exists())
        .take(300)
        .map(|mut t| {
            t.color = String::new();
            t.blurhash = String::new();
            for a in t.albumartists.iter_mut() {
                a.in_track = None;
            }
            for a in t.artists.iter_mut() {
                a.in_track = None;
            }
            t.to_json(userid, true)
        })
        .collect();

    Json(json!({ "tracks": tracks })).into_response()
}

#[allow(unused)]
fn _keep(_v: Value) {}
