//! Paths + settings.json (UserConfig subset) — parity with swingmusic settings.py / config.py.
use serde_json::{json, Value};
use std::path::PathBuf;

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
    pub data: Value,
}

impl Settings {
    pub fn load(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({}));
        Self { path, data }
    }

    pub fn server_id(&self) -> String {
        self.data["serverId"].as_str().unwrap_or("").to_string()
    }

    pub fn legacy_server_id(&self) -> String {
        self.data["legacy_serverId"].as_str().unwrap_or("").to_string()
    }

    pub fn users_on_login(&self) -> bool {
        self.data["usersOnLogin"].as_bool().unwrap_or(true)
    }

    pub fn root_dirs(&self) -> Vec<String> {
        self.data["rootDirs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn save(&self) -> std::io::Result<()> {
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.data)?)
    }
}
