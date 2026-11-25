//! Paths + settings.json (UserConfig subset) — parity with swingmusic settings.py / config.py.
use serde_json::{json, Value};
use crate::models::UserConfig;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub app_db_path: PathBuf,
    pub client_path: PathBuf,
}

impl Paths {
    /// Mirrors get_default_config_parent_dir: SWINGMUSIC_CONFIG_DIR → XDG_CONFIG_HOME
    /// → ~/.config; config_dir = parent/"swingmusic".
    pub fn new() -> Self {
        let parent = std::env::var("SWINGMUSIC_CONFIG_DIR")
            .ok()
            .map(PathBuf::from)
            .or_else(|| std::env::var("XDG_CONFIG_HOME").ok().map(PathBuf::from))
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            })
            .unwrap_or_else(|| PathBuf::from("."));

        let config_dir = parent.join("swingmusic");
        let _ = std::fs::create_dir_all(&config_dir);
        let app_db_path = config_dir.join("swingmusic.db");

        let client_path = std::env::var("SWINGMUSIC_CLIENT_DIR")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let local = PathBuf::from("client/client");
                if local.join("index.html").exists() {
                    local
                } else {
                    PathBuf::from("/opt/swingmusix/client")
                }
            });

        Self {
            config_dir,
            app_db_path,
            client_path,
        }
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    pub fn jwt_key_path(&self) -> PathBuf {
        self.config_dir.join("jwt.key")
    }

    /// User avatar dir — from upstream settings.py Paths.user_img_path.
    pub fn user_img_path(&self) -> PathBuf {
        self.config_dir.join("user_images")
    }
}

/// settings.json — read/modify/write preserving unknown keys (upstream UserConfig fields).
#[derive(Clone)]
pub struct Settings {
    path: PathBuf,
    /// Shared so /notsettings/add-root-dirs can update root dirs live without
    /// a restart — CT101's `config.rootDirs` is a process-global mutated in place.
    data: Arc<RwLock<Value>>,
}

impl Settings {
    pub fn load(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({}));
        Self {
            path,
            data: Arc::new(RwLock::new(data)),
        }
    }

    pub fn snapshot(&self) -> Value {
        self.data.read().map(|d| d.clone()).unwrap_or_else(|_| json!({}))
    }

    /// Snapshot of every setting the Vue UI reads from `GET /notsettings`.
    /// Mirrors settings.py `get_all_settings()`: asdict(UserConfig()) defaults
    /// overridden by settings.json, plus the computed/derived keys.
    pub fn all_settings_json(&self) -> Value {
        let defaults: Value = json!({
            "_finished": true,
            "_last_updated": 0.0,
            "artistArticleAwareSorting": false,
            "artistSeparators": [";", "/"],
            "artistSortingArticles": ["a","an","as","das","de","der","des","die","een","ein","eine","einem","einen","einer","el","gli","het","il","la","las","le","les","lo","los","o","os","the","um","uma","umas","un","una","unas","une","uno","unos","uns"],
            "artistSplitIgnoreList": [],
            "cleanAlbumTitle": true,
            "deviceId": "",
            "deviceName": "",
            "enablePeriodicScans": false,
            "enablePlugins": true,
            "enableWatchdog": false,
            "excludeDirs": [],
            "extractFeaturedArtists": true,
            "genreSeparators": ["/", ";", "&"],
            "lastfmApiKey": "",
            "lastfmApiSecret": "",
            "lastfmSessionKey": "",
            "legacy_serverId": "",
            "licenseInfo": null,
            "mergeAlbums": false,
            "plugins": [],
            "removeProdBy": true,
            "removeRemasterInfo": true,
            "rootDirs": [],
            "scanInterval": 10,
            "serverId": "",
            "showAlbumsAsSingles": false,
            "showPlaylistsInFolderView": false,
            "trustCloudLyrics": false,
            "usersOnLogin": true,
            "version": "2.1.14"
        });
        let mut out = defaults;
        if let (Some(obj), Some(over)) = (out.as_object_mut(), self.snapshot().as_object().cloned()) {
            for (k, v) in over {
                // licenseKey and lastfmSessionKeys are never exposed (gold deletes them)
                if k == "licenseKey" || k == "lastfmSessionKeys" {
                    continue;
                }
                obj.insert(k, v);
            }
        }
        // Computed keys the UI needs. Keep the values the live env uses; these
        // are not secrets (device fingerprint / display name / version string).
        {
            let o = out.as_object_mut().unwrap();
            o.insert("version".into(), json!("3.0.0"));
            o.insert("lastfmSessionKey".into(), json!(""));
            o.insert("licenseInfo".into(), Value::Null);
            let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "SwingMusix".into());
            let device_id = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                host.hash(&mut h);
                format!("{:016x}", h.finish())
            };
            o.insert("deviceId".into(), json!(device_id));
            o.insert("deviceName".into(), json!(host));
            // UI reads enableWatchDog (capital D) in one place; gold uses
            // enableWatchdog. Provide both so either code path is happy.
            let wd = o.get("enableWatchdog").cloned().unwrap_or(json!(false));
            o.insert("enableWatchDog".into(), wd);
        }
        out
    }

    pub fn server_id(&self) -> String {
        self.snapshot()["serverId"].as_str().unwrap_or("").to_string()
    }

    pub fn legacy_server_id(&self) -> String {
        self.snapshot()["legacy_serverId"].as_str().unwrap_or("").to_string()
    }

    pub fn users_on_login(&self) -> bool {
        self.snapshot()["usersOnLogin"].as_bool().unwrap_or(true)
    }

    pub fn show_playlists_in_folder_view(&self) -> bool {
        self.snapshot()["showPlaylistsInFolderView"]
            .as_bool()
            .unwrap_or(false)
    }

    pub fn root_dirs(&self) -> Vec<String> {
        self.snapshot()["rootDirs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Replace rootDirs with the normalized list and persist settings.json.
    /// Mirrors `config.rootDirs = normalize_paths(...); config.save()`.
    pub fn set_root_dirs(&self, dirs: Vec<String>) -> std::io::Result<Vec<String>> {
        let normalized = normalize_paths(&dirs);
        {
            let mut guard = self.data.write().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::Other, "settings lock poisoned")
            })?;
            guard["rootDirs"] = json!(normalized);
            std::fs::write(&self.path, serde_json::to_string_pretty(&*guard)?)?;
        }
        Ok(normalized)
    }

    #[allow(dead_code)]
    pub fn save(&self) -> std::io::Result<()> {
        let guard = self.data.read().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::Other, "settings lock poisoned")
        })?;
        std::fs::write(&self.path, serde_json::to_string_pretty(&*guard)?)
    }

    /// Effective user config — gold config.py union semantics: defaults
    /// overridden by present settings.json keys (unknown keys ignored).
    pub fn from_settings(&self) -> UserConfig {
        let data = self.snapshot();
        let mut cfg = UserConfig::default();
        if !data.is_object() {
            return cfg;
        }
        macro_rules! ov {
            ($f:ident, $k:literal, $t:ty) => {
                if let Some(v) = data.get($k) {
                    if let Ok(p) = serde_json::from_value::<$t>(v.clone()) {
                        cfg.$f = p;
                    }
                }
            };
        }
        ov!(extract_featured_artists, "extractFeaturedArtists", bool);
        ov!(remove_prod_by, "removeProdBy", bool);
        ov!(remove_remaster_info, "removeRemasterInfo", bool);
        ov!(merge_albums, "mergeAlbums", bool);
        ov!(clean_album_title, "cleanAlbumTitle", bool);
        ov!(show_albums_as_singles, "showAlbumsAsSingles", bool);
        ov!(artist_article_aware_sorting, "artistArticleAwareSorting", bool);
        ov!(artist_separators, "artistSeparators", HashSet<String>);
        ov!(genre_separators, "genreSeparators", HashSet<String>);
        ov!(artist_split_ignore_list, "artistSplitIgnoreList", Vec<String>);
        cfg
    }
}

/// Port of swingmusic.utils.paths.normalize_paths: resolve each path, drop any
/// path that is a child of an already-kept parent, return posix strings.
/// Kept parent-first (sorted by component count) so children get filtered.
pub fn normalize_paths(paths: &[String]) -> Vec<String> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut resolved: Vec<PathBuf> = paths
        .iter()
        .map(|p| {
            let pb = PathBuf::from(p);
            pb.canonicalize().unwrap_or_else(|_| {
                if pb.is_absolute() {
                    pb
                } else {
                    std::env::current_dir()
                        .map(|d| d.join(&pb))
                        .unwrap_or(pb)
                }
            })
        })
        .collect();
    resolved.sort_by_key(|p| p.components().count());
    let mut normalized: Vec<String> = Vec::new();
    for current in &resolved {
        let is_child = normalized.iter().any(|existing| {
            let e = Path::new(existing);
            current.starts_with(e) && current != e
        });
        if !is_child {
            normalized.push(current.to_string_lossy().replace('\\', "/"));
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::normalize_paths;

    #[test]
    fn normalize_drops_child_of_parent() {
        let out = normalize_paths(&[
            "/music/music".into(),
            "/music/music/extra".into(),
        ]);
        assert_eq!(out, vec!["/music/music".to_string()]);
    }

    #[test]
    fn normalize_keeps_distinct_paths() {
        let out = normalize_paths(&["/mnt/music".into(), "/home/user/music".into()]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn normalize_empty_is_empty() {
        assert!(normalize_paths(&[]).is_empty());
    }
}
