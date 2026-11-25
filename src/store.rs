//! SwingMusix — store port of swingmusic/{lib/albumslib.py create_albums(),
//! lib/tagger.py create_artists(), store/tracks.py loader}.
//!
//! Albums/artists are DERIVED at runtime from the flat `track` table — the live
//! DB has no album/artist tables (verified 2026-11-17).
//! Aggregation rules (verbatim from lib/albumslib.py, read 2026-11-18):
//!   playcount +=, playduration +=, lastplayed = max, duration +=, date = min,
//!   created_date = min(last_mod), genres extend + dedup,
//!   genrehashes = " ".join(...)  (STRING — upstream bug kept 1:1),
//!   trackhashes appended, trackcount = len(trackhashes).
#![allow(dead_code)]

use std::collections::HashMap;

use crate::models::{Album, Artist, ArtistRef, Track, UserConfig};
use crate::natsort::natsorted;

/// Load every track from the live sqlite db using the positional column map.
pub fn load_tracks(conn: &rusqlite::Connection, cfg: &UserConfig) -> rusqlite::Result<Vec<Track>> {
    // settings.json `function` map — empty in every live env seen (models.rs note).
    let fns: crate::models::TitleFunctions = HashMap::new();
    let mut stmt = conn.prepare("SELECT * FROM track ORDER BY id")?;
    let mut rows = stmt.query([])?;
    let mut out: Vec<Track> = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(Track::from_row(row, cfg, &fns)?);
    }
    Ok(out)
}

/// Port of lib/albumslib.py create_albums() — in-memory grouping by albumhash.
///
/// `&mut` because the live startup order is create_albums() THEN
/// create_artists(), and the latter mutates the shared track dicts in place
/// (`albumartists[i]["in_track"] = False`). We mirror that exactly: after an
/// album is built from a track, the same mutation is applied to the track.
pub fn create_albums(tracks: &mut [Track]) -> Vec<Album> {
    let mut order: HashMap<String, usize> = HashMap::new();
    let mut albums: Vec<Album> = Vec::new();

    for t in tracks.iter_mut() {
        // Live order check: create_artists() runs after create_albums(), and
        // the album dicts reference the SAME artist dicts as their tracks, so
        // the aa∉ar markers applied by create_artists() are visible through
        // the album too (aliasing). The Rust album clone snapshots its
        // artist refs, so apply the markers BEFORE the album is built to
        // keep the same serialized state on both sides.
        apply_albumartists_in_track_flags(t);

        match order.get(&t.albumhash).copied() {
            Some(i) => {
                let a = &mut albums[i];
                a.playcount += t.playcount;
                a.playduration += t.playduration;
                a.lastplayed = a.lastplayed.max(t.lastplayed);
                a.duration += t.duration;
                a.date = a.date.min(t.date);
                a.created_date = a.created_date.min(t.last_mod as i64);
                a.trackhashes.push(t.trackhash.clone());
                a.trackcount = a.trackhashes.len() as i64;
                for g in &t.genres {
                    if !a.genres.iter().any(|x| x.genrehash == g.genrehash) {
                        a.genres.push(g.clone());
                    }
                }
            }
            None => {
                order.insert(t.albumhash.clone(), albums.len());
                albums.push(Album::from_track(t));
            }
        }
    }

    // genrehashes as " ".join (upstream bug), trackcount finalize
    for a in albums.iter_mut() {
        a.genrehashes = a
            .genres
            .iter()
            .map(|g| g.genrehash.clone())
            .collect::<Vec<_>>()
            .join(" ");
        a.trackcount = a.trackhashes.len() as i64;
    }

    albums
}

/// Per-albumartist `in_track` flags for a track: true when the exact
/// (name, artisthash) pair is present in the track's `artists` list — the live
/// `a not in this_artists` dict-equality check (lib/tagger.py L237-241).
fn albumartists_in_track_flags(artists: &[ArtistRef], albumartists: &[ArtistRef]) -> Vec<bool> {
    albumartists
        .iter()
        .map(|a| {
            artists
                .iter()
                .any(|x| x.name == a.name && x.artisthash == a.artisthash)
        })
        .collect()
}

/// Apply the live `create_artists()` mutation to a store track in place: every
/// albumartist NOT present in `artists` is marked `in_track = false`. Entries
/// present in both keep the absent key (live only sets it when appending).
pub fn apply_albumartists_in_track_flags(t: &mut Track) {
    let flags = albumartists_in_track_flags(&t.artists, &t.albumartists);
    for (a, present) in t.albumartists.iter_mut().zip(flags.iter()) {
        if !*present {
            a.in_track = Some(false);
        }
    }
}

/// Port of lib/tagger.py create_artists() — group tracks per artisthash.
/// Gold walks `[*track.artists]` plus every `track.albumartists` entry not
/// already present (marked in_track=false: those get tracks empty, count 0).
pub fn create_artists(tracks: &[Track]) -> Vec<Artist> {
    // gold L231: `all_tracks = remove_duplicates(all_tracks)` — group by
    // trackhash (insertion order preserved), keep the FIRST row with the
    // max bitrate; each trackhash is then aggregated exactly once.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut deduped: Vec<&Track> = Vec::new();
    for t in tracks {
        match seen.get(t.trackhash.as_str()) {
            Some(&i) => {
                if t.bitrate > deduped[i].bitrate {
                    deduped[i] = t;
                }
            }
            None => {
                seen.insert(t.trackhash.as_str(), deduped.len());
                deduped.push(t);
            }
        }
    }

    let mut order: HashMap<String, usize> = HashMap::new();
    let mut artists: Vec<Artist> = Vec::new();

    for t in &deduped {
        // gold L235-240: this_artists = [*track.artists] + albumartists not in it
        let aa_flags = albumartists_in_track_flags(&t.artists, &t.albumartists);
        let mut this: Vec<_> = t.artists.iter().map(|a| (a, true)).collect();
        for (a, present) in t.albumartists.iter().zip(aa_flags.iter()) {
            if !*present {
                this.push((a, false));
            }
        }

        for (ar, in_track) in this {
            let i = match order.get(&ar.artisthash).copied() {
                Some(i) => i,
                None => {
                    order.insert(ar.artisthash.clone(), artists.len());
                    artists.push(Artist::from_track(t, ar));
                    artists.len() - 1
                }
            };
            let a = &mut artists[i];

            a.duration += t.duration;
            a.playcount += t.playcount;
            a.playduration += t.playduration;
            a.lastplayed = a.lastplayed.max(t.lastplayed);
            a.date = a.date.min(t.date);
            a.created_date = a.created_date.min(t.last_mod as i64);

            if !a.albumhashes.contains(&t.albumhash) {
                a.albumhashes.push(t.albumhash.clone());
            }

            // gold: only in-track occurrences gain the trackhash (set add)
            if in_track && !a.trackhashes.contains(&t.trackhash) {
                a.trackhashes.push(t.trackhash.clone());
            }

            if !a.names.contains(&ar.name) {
                a.names.push(ar.name.clone());
            }

            if !t.genres.is_empty() {
                for g in &t.genres {
                    if !a.genres.iter().any(|x| x.genrehash == g.genrehash) {
                        a.genres.push(g.clone());
                    }
                }
            }
        }
    }

    // pick natsorted-first name for spellings that share an artisthash; finalize counts
    for a in artists.iter_mut() {
        let mut names = a.names.clone();
        natsorted(&mut names);
        if let Some(first) = names.first() {
            a.name = first.clone();
        }
        a.albumcount = a.albumhashes.len() as i64;
        a.trackcount = a.trackhashes.len() as i64;
        a.image = format!("{}.webp", a.artisthash);
        a.genrehashes = a.genres.iter().map(|g| g.genrehash.clone()).collect();
    }

    artists
}

/// Build an album's playlist view: its tracks sorted by disc/track number.
pub fn sort_by_track_no(tracks: &mut [Track]) {
    tracks.sort_by(|a, b| (a.disc, a.track).cmp(&(b.disc, b.track)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ArtistRef, GenreRef};
    use serde_json::json;

    fn track(albumhash: &str, trackhash: &str, title: &str) -> Track {
        Track {
            track_id: -1,
            album: "Album".to_string(),
            albumartists: vec![ArtistRef {
                name: "Artist".to_string(),
                artisthash: "ah1".to_string(),
                in_track: None,
            }],
            albumhash: albumhash.to_string(),
            artists: vec![ArtistRef {
                name: "Artist".to_string(),
                artisthash: "ah1".to_string(),
                in_track: None,
            }],
            bitrate: 0,
            copyright: String::new(),
            date: 0,
            disc: 1,
            duration: 100,
            filepath: String::new(),
            folder: "/music/a".to_string(),
            genres: vec![GenreRef {
                name: "pop".to_string(),
                genrehash: "gh1".to_string(),
            }],
            last_mod: 10.0,
            title: title.to_string(),
            track: 1,
            trackhash: trackhash.to_string(),
            lastplayed: 0,
            playcount: 1,
            playduration: 100,
            extra: json!({}),
            og_title: title.to_string(),
            og_album: "Album".to_string(),
            artisthashes: vec!["ah1".to_string()],
            genrehashes: vec!["gh1".to_string()],
            weakhash: String::new(),
            pathhash: String::new(),
            explicit: false,
            image: String::new(),
            color: String::new(),
            blurhash: String::new(),
            fav_userids: Vec::new(),
            artists_raw: String::new(),
            albumartists_raw: String::new(),
            genres_raw: String::new(),
        }
    }

    #[test]
    fn album_aggregation() {
        let mut tracks = vec![
            track("alb1", "t1", "One"),
            track("alb1", "t2", "Two"),
            track("alb2", "t3", "Three"),
        ];
        let albums = create_albums(&mut tracks);
        assert_eq!(albums.len(), 2);
        let a = albums.iter().find(|a| a.albumhash == "alb1").unwrap();
        assert_eq!(a.trackcount, 2);
        assert_eq!(a.duration, 200);
        assert_eq!(a.playcount, 2);
        assert_eq!(a.genrehashes, "gh1");
    }

    #[test]
    fn artist_aggregation_dedups_per_track() {
        let tracks = vec![track("alb1", "t1", "One"), track("alb1", "t2", "Two")];
        let artists = create_artists(&tracks);
        assert_eq!(artists.len(), 1);
        let a = &artists[0];
        assert_eq!(a.trackcount, 2);
        assert_eq!(a.albumcount, 1);
        assert_eq!(a.duration, 200);
    }

    /// Wave-2b gold parity: only albumartists pairs absent from `artists`
    /// (exact name+hash) get `in_track=false`; matching pairs keep the key
    /// absent. Serializer emits the key only when set.
    #[test]
    fn albumartists_marker_matches_gold() {
        let mut t = track("alb1", "t1", "One");
        t.albumartists.push(ArtistRef {
            name: "Guest".to_string(),
            artisthash: "ah2".to_string(),
            in_track: None,
        });
        let mut tracks = vec![t];
        let albums = create_albums(&mut tracks);
        // in-list entry untouched (no key), aa∉ar entry marked false
        assert_eq!(tracks[0].albumartists[0].in_track, None);
        assert_eq!(tracks[0].albumartists[1].in_track, Some(false));
        // live aliasing: the album references the same artist dicts, so the
        // markers are visible through the album clone too
        assert_eq!(albums[0].albumartists[0].in_track, None);
        assert_eq!(albums[0].albumartists[1].in_track, Some(false));
        assert_eq!(
            tracks[0].albumartists[1].to_value(),
            json!({"name": "Guest", "artisthash": "ah2", "in_track": false})
        );
        assert_eq!(
            tracks[0].albumartists[0].to_value(),
            json!({"name": "Artist", "artisthash": "ah1"})
        );
    }
}
