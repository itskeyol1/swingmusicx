//! api/stream.py — `/file/*`: audio streaming with Range support, chunked
//! transcoding, silence paddings and waveform extraction.
//!
//! Port of swingmusic/api/stream.py (477 lines) + lib/transcoder.py +
//! lib/trackslib.py silence helpers. Behavior verified byte-exact against the
//! live CT101 pendant (etag `"mtime.0-size-adler32(path)"`, 206/416/304
//! headers, error bodies) before this port was written.

use axum::{
    body::Body,
    extract::{Path as AxPath, Query, Request as AxRequest, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path as StdPath, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Command;

use crate::folder::{py_resolve, tracks_by_filepaths};
use crate::models::Track;
use crate::AppState;

const CHUNK_SIZE: u64 = 1024 * 512; // 0.5MB — parity stream.py

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn error_json(body: Value, code: StatusCode) -> Response {
    (code, Json(body)).into_response()
}

fn file_not_found() -> Response {
    error_json(json!({ "msg": "File Not Found" }), StatusCode::NOT_FOUND)
}

/// zlib.adler32(data, 1) — the checksum component of the send_file etag.
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

/// utils/files.py `guess_mime_type()` — mimetypes.guess_type with the
/// `audio/{ext}` fallback. Common audio extensions are pinned to Python's
/// table (mime_guess differs on a few, e.g. .opus).
fn guess_mime_type(filename: &str) -> String {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp3" | "mpga" => "audio/mpeg".to_string(),
        "ogg" | "oga" | "opus" => "audio/ogg".to_string(),
        "flac" => "audio/flac".to_string(),
        "m4a" => "audio/mp4".to_string(),
        "aac" => "audio/aac".to_string(),
        "wav" => "audio/x-wav".to_string(),
        "wma" => "audio/x-ms-wma".to_string(),
        "webm" => "video/webm".to_string(),
        "mp4" => "video/mp4".to_string(),
        "aiff" | "aif" => "audio/x-aiff".to_string(),
        "mka" => "audio/matroska".to_string(),
        "m3u" => "audio/mpegurl".to_string(),
        _ => match mime_guess::from_path(filename).first() {
            Some(m) => m.to_string(),
            None => {
                if ext.is_empty() {
                    format!("audio/{}", filename)
                } else {
                    format!("audio/{}", ext)
                }
            }
        },
    }
}

/// flask/werkzeug `send_from_directory(..., as_attachment=False)` disposition:
/// `inline; filename="x.mp3"`; non-ASCII names use RFC 5987 `filename*`.
fn content_disposition(name: &str) -> String {
    let ascii_plain =
        name.bytes().all(|b| (0x20..0x7f).contains(&b)) && !name.contains('"') && !name.contains('\\');
    if ascii_plain {
        format!("inline; filename=\"{}\"", name)
    } else {
        let mut enc = String::new();
        for &b in name.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' => {
                    enc.push(b as char)
                }
                _ => enc.push_str(&format!("%{:02X}", b)),
            }
        }
        format!("inline; filename*=UTF-8''{}", enc)
    }
}

/// Werkzeug conditional/range etag comparison (weak-safe, list-aware).
fn strip_weak_quotes(s: &str) -> &str {
    let s = s.trim();
    let s = s.strip_prefix("W/").unwrap_or(s);
    s.trim_matches('"')
}

fn etag_matches(header_val: &str, etag: &str) -> bool {
    if header_val.trim() == "*" {
        return true;
    }
    let want = strip_weak_quotes(etag);
    header_val.split(',').any(|t| strip_weak_quotes(t) == want)
}

/// If-Range gate: etag exact match, or an HTTP-date not older than the file.
fn if_range_ok(value: &str, etag: &str, mtime_floor: u64) -> bool {
    let v = value.trim();
    if v.starts_with('"') || v.starts_with("W/") {
        return strip_weak_quotes(v) == strip_weak_quotes(etag);
    }
    if let Ok(t) = httpdate::parse_http_date(v) {
        return t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX)
            >= mtime_floor;
    }
    false
}

enum RangeResult {
    Invalid,
    Unsatisfiable,
    Satisfiable(u64, u64),
}

/// Werkzeug-style single-range parser. Malformed or multi-range headers are
/// invalid (→ 416, verified on the live pendant: `bytes=abc` and
/// `bytes=0-9,20-29` both return 416).
fn parse_range(header_val: &str, size: u64) -> RangeResult {
    let v = header_val.trim();
    let Some(spec) = v.strip_prefix("bytes=") else {
        return RangeResult::Invalid;
    };
    if spec.contains(',') {
        return RangeResult::Invalid;
    }
    let Some((a, b)) = spec.split_once('-') else {
        return RangeResult::Invalid;
    };
    let a = a.trim();
    let b = b.trim();
    if a.is_empty() {
        // suffix: bytes=-N (last N bytes)
        let Ok(n) = b.parse::<u64>() else {
            return RangeResult::Invalid;
        };
        if n == 0 || size == 0 {
            return RangeResult::Unsatisfiable;
        }
        return RangeResult::Satisfiable(size.saturating_sub(n), size - 1);
    }
    let Ok(start) = a.parse::<u64>() else {
        return RangeResult::Invalid;
    };
    if size == 0 || start >= size {
        return RangeResult::Unsatisfiable;
    }
    let end = if b.is_empty() {
        size - 1
    } else {
        match b.parse::<u64>() {
            Ok(e) => {
                if e < start {
                    return RangeResult::Invalid;
                }
                e.min(size - 1)
            }
            Err(_) => return RangeResult::Invalid,
        }
    };
    RangeResult::Satisfiable(start, end)
}

/// Werkzeug's default 416 body (177 bytes, captured byte-exact from CT101).
fn range_416(size: u64) -> Response {
    const BODY: &str = "<!doctype html>\n<html lang=en>\n<title>416 Requested Range Not Satisfiable</title>\n<h1>Requested Range Not Satisfiable</h1>\n<p>The server cannot provide the requested range.</p>\n";
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_RANGE, format!("bytes */{}", size))
        .header(header::CONTENT_LENGTH, BODY.len().to_string())
        .body(Body::from(BODY))
        .unwrap()
}

async fn read_chunk(filepath: &str, start: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let mut f = tokio::fs::File::open(filepath).await?;
    f.seek(SeekFrom::Start(start)).await?;
    let mut buf: Vec<u8> = Vec::new();
    let mut take = f.take(len);
    take.read_to_end(&mut buf).await?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// find_playable_track + send_audio_file (stream.py L56-L177)
// ---------------------------------------------------------------------------

/// parity: `root_dir in requested_filepath.parents` — strict ancestor check.
fn inside_roots(resolved: &StdPath, root_dirs: &[String]) -> bool {
    for rd in root_dirs {
        let root_path: PathBuf = if rd == "$home" {
            match std::env::var("HOME") {
                Ok(h) => PathBuf::from(h),
                Err(_) => continue,
            }
        } else {
            py_resolve(rd)
        };
        if resolved != root_path.as_path() && resolved.starts_with(&root_path) {
            return true;
        }
    }
    false
}

fn find_playable_track(
    st: &AppState,
    requested_trackhash: &str,
    filepath: &str,
) -> Result<Track, Response> {
    let requested_trackhash = requested_trackhash.trim();
    let filepath = filepath.trim();

    // prevent path traversal
    if filepath.contains("/../") {
        return Err(error_json(
            json!({ "msg": "Invalid filepath", "error": "Path traversal detected" }),
            StatusCode::BAD_REQUEST,
        ));
    }

    let resolved = py_resolve(filepath);
    if !inside_roots(&resolved, &st.settings.root_dirs()) {
        return Err(error_json(
            json!({ "msg": "Invalid filepath", "error": "File not inside root directories" }),
            StatusCode::BAD_REQUEST,
        ));
    }

    let candidates = tracks_by_filepaths(&st.library.tracks, &[filepath.to_string()]);
    let track: Option<Track> =
        if !candidates.is_empty() && StdPath::new(&candidates[0].filepath).exists() {
            candidates
                .iter()
                .find(|t| StdPath::new(&t.filepath).exists() && t.trackhash == requested_trackhash)
                .cloned()
        } else {
            // fall back to the highest-bitrate existing copy of the trackhash
            let mut group: Vec<Track> = st
                .library
                .tracks
                .iter()
                .filter(|t| t.trackhash == requested_trackhash)
                .cloned()
                .collect();
            group.sort_by(|a, b| b.bitrate.cmp(&a.bitrate));
            group
                .into_iter()
                .find(|t| StdPath::new(&t.filepath).exists())
        };

    track.ok_or_else(file_not_found)
}

/// stream.py `send_audio_file()` via flask's conditional `send_from_directory`:
/// 200 full / 206 range / 304 conditional / 416 unsat, etag
/// `"{mtime}-{size}-{adler32(filepath)}"`, `cache-control: no-cache`.
async fn send_audio_file(filepath: &str, hdrs: &HeaderMap) -> Response {
    let meta = match tokio::fs::metadata(filepath).await {
        Ok(m) => m,
        Err(_) => return file_not_found(),
    };
    let size = meta.len();
    let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
    let mtime_f = mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mtime_floor = mtime_f.floor() as u64;

    // python str(float): 1511363296.0 — Rust `{:?}` matches for f64.
    let etag = format!("\"{:?}-{}-{}\"", mtime_f, size, adler32(filepath.as_bytes()));
    let last_mod = httpdate::fmt_http_date(mtime);
    let name = StdPath::new(filepath)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let disposition = content_disposition(&name);
    let mime = guess_mime_type(filepath);

    // conditional — ETag first, then Last-Modified (werkzeug order).
    let inm = hdrs.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    let ims = hdrs
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok());
    let mut not_modified = false;
    if let Some(v) = inm {
        not_modified = etag_matches(v, &etag);
    } else if let Some(v) = ims {
        if let Ok(t) = httpdate::parse_http_date(v.trim()) {
            if t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) >= mtime_floor {
                not_modified = true;
            }
        }
    }
    if not_modified {
        // parity: 304 keeps disposition/cache-control/etag/accept-ranges only.
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::CONTENT_DISPOSITION, disposition)
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::ETAG, etag)
            .header(header::ACCEPT_RANGES, "bytes")
            .body(Body::empty())
            .unwrap();
    }

    // Range (ignored when If-Range does not validate — full 200, verified live).
    let mut range: Option<(u64, u64)> = None;
    if let Some(rv) = hdrs.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        let apply = match hdrs.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) {
            None => true,
            Some(ifr) => if_range_ok(ifr, &etag, mtime_floor),
        };
        if apply {
            match parse_range(rv, size) {
                RangeResult::Satisfiable(s, e) => range = Some((s, e)),
                _ => return range_416(size),
            }
        }
    }

    match range {
        None => {
            let bytes = match tokio::fs::read(filepath).await {
                Ok(b) => b,
                Err(_) => return file_not_found(),
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_DISPOSITION, disposition)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_LENGTH, size.to_string())
                .header(header::LAST_MODIFIED, last_mod)
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::ETAG, etag)
                .header(header::ACCEPT_RANGES, "bytes")
                .body(Body::from(bytes))
                .unwrap()
        }
        Some((s, e)) => {
            let len = e - s + 1;
            let bytes = match read_chunk(filepath, s, len).await {
                Ok(b) => b,
                Err(_) => return file_not_found(),
            };
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_DISPOSITION, disposition)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_LENGTH, len.to_string())
                .header(header::LAST_MODIFIED, last_mod)
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::ETAG, etag)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_RANGE, format!("bytes {}-{}/{}", s, e, size))
                .body(Body::from(bytes))
                .unwrap()
        }
    }
}

// ---------------------------------------------------------------------------
// transcode_and_stream + send_file_as_chunks (stream.py L216-L371)
// ---------------------------------------------------------------------------

fn transcode_store() -> &'static Mutex<HashMap<String, String>> {
    static STORE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn temp_path(suffix: &str) -> String {
    let n: u64 = rand::random();
    std::env::temp_dir()
        .join(format!("swingmusix-{:x}{}", n, suffix))
        .to_string_lossy()
        .to_string()
}

async fn transcode_and_stream(
    trackhash: &str,
    filepath: &str,
    bitrate: &str,
    container: &str,
    hdrs: &HeaderMap,
) -> Response {
    // TransCodeStore is keyed by trackhash only — repeat requests reuse the temp file.
    // NOTE: hoisted out of the `if let` scrutinee so the MutexGuard drops before
    // the await below (guard-across-await makes the handler future non-Send).
    let cached = transcode_store().lock().unwrap().get(trackhash).cloned();
    if let Some(temp) = cached {
        return send_file_as_chunks(&temp, hdrs).await;
    }

    let container_args: &[&str] = match container {
        "mp3" => &["-c:a", "libmp3lame"],
        "aac" => &["-c:a", "aac"],
        "webm" => &["-c:a", "libopus"],
        "ogg" => &["-c:a", "libvorbis"],
        "flac" => &["-c:a", "flac"],
        _ => &["-c:a", "flac"],
    };
    let suffix = match container {
        "mp3" | "aac" | "webm" | "ogg" | "flac" => format!(".{}", container),
        _ => ".flac".to_string(),
    };
    let temp_filename = temp_path(&suffix);
    let _ = tokio::fs::File::create(&temp_filename).await;
    transcode_store()
        .lock()
        .unwrap()
        .insert(trackhash.to_string(), temp_filename.clone());

    let orig_size = tokio::fs::metadata(filepath)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    // ffmpeg args — parity lib/transcoder.py start_transcoding().
    match Command::new("ffmpeg")
        .arg("-i")
        .arg(filepath)
        .arg("-map_metadata")
        .arg("0")
        .arg("-b:a")
        .arg(bitrate)
        .arg("-vn")
        .arg("-compression_level")
        .arg("12")
        .arg("-movflags")
        .arg("faststart+frag_keyframe+empty_moov")
        .arg("-write_xing")
        .arg("0")
        .arg("-fflags")
        .arg("+bitexact")
        .args(container_args)
        .arg(&temp_filename)
        .arg("-y")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(_) => {
            return error_json(
                json!({ "msg": "ffmpeg not found on server" }),
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    }

    // Poll for the first 512KB (stream.py polls until the temp file has
    // chunk_size bytes). Extra: stop once the output size is stable for ~1s so
    // short transcodes don't stall; hard cap 60s.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_size = 0u64;
    let mut stable = 0u32;
    loop {
        let size = tokio::fs::metadata(&temp_filename)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if size >= CHUNK_SIZE {
            break;
        }
        if size > 0 && size == last_size {
            stable += 1;
            if stable >= 10 {
                break;
            }
        } else {
            stable = 0;
        }
        last_size = size;
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let data = read_chunk(&temp_filename, 0, CHUNK_SIZE)
        .await
        .unwrap_or_default();
    let audio_type = guess_mime_type(&temp_filename);
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, audio_type)
        // parity: end is the literal chunk_size, denominator the original size.
        .header(
            header::CONTENT_RANGE,
            format!("bytes 0-{}/{}", CHUNK_SIZE, orig_size),
        )
        .header(header::ACCEPT_RANGES, "bytes")
        .header("x-transcoded-bitrate", bitrate)
        .body(Body::from(data))
        .unwrap()
}

async fn send_file_as_chunks(filepath: &str, hdrs: &HeaderMap) -> Response {
    let size = match tokio::fs::metadata(filepath).await {
        Ok(m) => m.len(),
        Err(_) => return file_not_found(),
    };
    let start = hdrs
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(get_start_range)
        .unwrap_or(0);

    let _end = start + CHUNK_SIZE - 1;
    let end = if _end > size {
        size.saturating_sub(1)
    } else {
        _end
    };

    let remaining = end.saturating_sub(start) + 1;
    let read_len = if remaining < CHUNK_SIZE {
        // final chunk — stream.py reads to EOF from here
        size.saturating_sub(start)
    } else {
        CHUNK_SIZE
    };
    let data = read_chunk(filepath, start, read_len)
        .await
        .unwrap_or_default();
    let position = start + data.len() as u64;
    let is_final = position >= size;
    let bytes_to_add = if is_final { 0 } else { CHUNK_SIZE };

    let audio_type = guess_mime_type(filepath);
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, audio_type)
        .header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, position, size + bytes_to_add),
        )
        .header("Access-Control-Expose-Headers", "Content-Range")
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::from(data))
        .unwrap()
}

fn get_start_range(range_header: &str) -> u64 {
    range_header
        .trim()
        .split('=')
        .nth(1)
        .and_then(|s| s.split('-').next())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// /file/silence + /file/waveform (stream.py L374-L477)
// ---------------------------------------------------------------------------

async fn run_silencedetect(filepath: &str) -> Option<String> {
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostats",
            "-i",
            filepath,
            "-af",
            "silencedetect=noise=-40dB:d=0.01",
            "-f",
            "null",
            "-",
        ])
        .output()
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&out.stderr).to_string())
}

fn parse_duration_secs(stderr: &str) -> Option<f64> {
    for line in stderr.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Duration:") {
            let t = rest.trim().split(',').next()?.trim();
            let mut parts = t.split(':');
            let h: f64 = parts.next()?.trim().parse().ok()?;
            let m: f64 = parts.next()?.trim().parse().ok()?;
            let s: f64 = parts.next()?.trim().parse().ok()?;
            return Some(h * 3600.0 + m * 60.0 + s);
        }
    }
    None
}

fn parse_silence_intervals(stderr: &str) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    let mut cur: Option<f64> = None;
    for line in stderr.lines() {
        if let Some(idx) = line.find("silence_start:") {
            let v = line[idx + "silence_start:".len()..]
                .trim()
                .split_whitespace()
                .next()
                .unwrap_or("")
                .parse::<f64>();
            if let Ok(v) = v {
                cur = Some(v);
            }
        } else if let Some(idx) = line.find("silence_end:") {
            let v = line[idx + "silence_end:".len()..]
                .trim()
                .split_whitespace()
                .next()
                .unwrap_or("")
                .parse::<f64>();
            if let Ok(v) = v {
                if let Some(s) = cur.take() {
                    out.push((s, v));
                }
            }
        }
    }
    if let Some(s) = cur {
        out.push((s, f64::INFINITY));
    }
    out
}

/// trackslib.py `get_leading_silence_end()` — ms of leading silence, 0 unless >1000.
async fn leading_silence_end(filepath: &str) -> i64 {
    let Some(stderr) = run_silencedetect(filepath).await else {
        return 0;
    };
    let intervals = parse_silence_intervals(&stderr);
    if let Some((s, e)) = intervals.first().copied() {
        if s <= 0.05 && e.is_finite() {
            let ms = (e * 1000.0) as i64;
            return if ms > 1000 { ms } else { 0 };
        }
    }
    0
}

/// trackslib.py `get_trailing_silence_start()` — start point (ms) of trailing
/// silence when it reaches EOF, else the track duration.
async fn trailing_silence_start(filepath: &str) -> i64 {
    let Some(stderr) = run_silencedetect(filepath).await else {
        return 0;
    };
    let dur = parse_duration_secs(&stderr).unwrap_or(0.0);
    let duration_ms = (dur * 1000.0) as i64;
    let intervals = parse_silence_intervals(&stderr);
    let Some((s, e)) = intervals.last().copied() else {
        return duration_ms;
    };
    // pydub: only when the silent group reaches the end of the audio
    if !e.is_finite() || (e - dur).abs() <= 0.15 {
        let end = if e.is_finite() { e } else { dur };
        return duration_ms - ((end - s) * 1000.0) as i64;
    }
    duration_ms
}

#[derive(Deserialize)]
pub struct GetAudioSilenceBody {
    ending_file: Option<String>,
    starting_file: Option<String>,
}

pub async fn get_audio_silence(
    State(_st): State<AppState>,
    Json(body): Json<GetAudioSilenceBody>,
) -> Response {
    let ending = body.ending_file.unwrap_or_default();
    let starting = body.starting_file.unwrap_or_default();

    // NOTE: gold's `TrackStore.is_valid_track_filepath` call is an undefined
    // attribute (live: 500 on 4-field payloads, 422 on the worker's 2-field
    // payload). This port accepts the worker payload and computes paddings
    // directly; missing files fall back to 0 (get_silence_paddings defaults).
    let ending_ms = if !ending.is_empty() && StdPath::new(&ending).exists() {
        trailing_silence_start(&ending).await
    } else {
        0
    };
    let starting_ms = if !starting.is_empty() && StdPath::new(&starting).exists() {
        leading_silence_end(&starting).await
    } else {
        0
    };

    Json(json!({ "ending_file": ending_ms, "starting_file": starting_ms })).into_response()
}

pub async fn get_track_waveform(
    State(st): State<AppState>,
    AxPath(trackhash): AxPath<String>,
) -> Response {
    let th = trackhash.trim();
    let track = st
        .library
        .tracks
        .iter()
        .filter(|t| t.trackhash == th)
        .find(|t| StdPath::new(&t.filepath).exists())
        .cloned();
    let Some(track) = track else {
        return file_not_found();
    };

    let duration_ms = track.duration.max(0).saturating_mul(1000);

    let fut = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-i",
            &track.filepath,
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            "8000",
            "-f",
            "s16le",
            "-",
        ])
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(60), fut).await {
        Err(_) => {
            return error_json(
                json!({ "msg": "Waveform computation timed out" }),
                StatusCode::GATEWAY_TIMEOUT,
            )
        }
        Ok(Err(_)) => {
            return error_json(
                json!({ "msg": "ffmpeg not found on server" }),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        }
        Ok(Ok(o)) => o,
    };
    if out.status.code() != Some(0) || out.stdout.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr)
            .chars()
            .take(200)
            .collect::<String>();
        let msg = if err.is_empty() {
            "no audio output".to_string()
        } else {
            err
        };
        return error_json(
            json!({ "msg": format!("Waveform computation failed: {}", msg) }),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }

    let raw: Vec<i16> = out
        .stdout
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    if raw.is_empty() {
        return error_json(
            json!({ "msg": "Waveform computation failed: empty audio stream" }),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }

    let n = 600usize;
    let chunk = raw.len() as f64 / n as f64;
    let mut samples: Vec<i64> = Vec::with_capacity(n);
    for i in 0..n {
        let start = (((i as f64) * chunk) as usize).min(raw.len());
        let end = std::cmp::max(start + 1, (((i + 1) as f64) * chunk) as usize).min(raw.len());
        let mut peak: i32 = 0;
        for &v in &raw[start..end] {
            peak = peak.max((v as i32).abs());
        }
        samples.push(std::cmp::min(100, (peak as f64 / 327.67).round() as i64));
    }

    let generated_at = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Json(json!({
        "trackhash": track.trackhash,
        "samples": samples,
        "duration_ms": duration_ms,
        "generated_at": generated_at,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// routes (stream.py L44-L213)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SendTrackFileQuery {
    filepath: Option<String>,
    quality: Option<String>,
    container: Option<String>,
}

/// flask-openapi3 rejects a missing `filepath` with pydantic's validation
/// error. The middleware appends "\n" to JSON bodies, so this ships as 125 B
/// vs the live 124 B on an edge the stock client never hits (it always sends
/// filepath).
fn pydantic_missing_filepath() -> Response {
    let body = r#"[{"type":"missing","loc":["filepath"],"msg":"Field required","input":{},"url":"https://errors.pydantic.dev/2.13/v/missing"}]"#;
    Response::builder()
        .status(StatusCode::UNPROCESSABLE_ENTITY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

pub async fn send_track_file_legacy(
    State(st): State<AppState>,
    AxPath(trackhash): AxPath<String>,
    Query(q): Query<SendTrackFileQuery>,
    req: AxRequest,
) -> Response {
    let hdrs = req.headers().clone();
    send_track_file_impl(&st, &trackhash, q, &hdrs).await
}

pub async fn send_track_file(
    State(st): State<AppState>,
    AxPath(trackhash): AxPath<String>,
    Query(q): Query<SendTrackFileQuery>,
    req: AxRequest,
) -> Response {
    let hdrs = req.headers().clone();
    send_track_file_impl(&st, &trackhash, q, &hdrs).await
}

async fn send_track_file_impl(
    st: &AppState,
    trackhash: &str,
    q: SendTrackFileQuery,
    hdrs: &HeaderMap,
) -> Response {
    let Some(filepath) = q.filepath else {
        return pydantic_missing_filepath();
    };

    let track = match find_playable_track(st, trackhash, &filepath) {
        Ok(t) => t,
        Err(r) => return r,
    };

    let quality = q.quality.as_deref().unwrap_or("original");
    if quality != "original" && !quality.is_empty() {
        if let Ok(requested) = quality.parse::<i64>() {
            let container = q.container.as_deref().unwrap_or("mp3");
            // drop to 320 for non-flac containers
            let requested = if container != "flac" {
                requested.min(320)
            } else {
                requested
            };
            let max_bitrate = std::cmp::max(
                1,
                if track.bitrate > 0 {
                    track.bitrate
                } else {
                    requested
                },
            );
            let bitrate = format!("{}k", std::cmp::min(max_bitrate, requested));
            return transcode_and_stream(trackhash, &track.filepath, &bitrate, container, hdrs)
                .await;
        }
        // invalid quality value → serve the original (parity: ValueError branch)
    }

    send_audio_file(&track.filepath, hdrs).await
}
