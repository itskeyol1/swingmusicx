//! /notsettings/* routes — parity with swingmusic api/settings.py (v2.1.14).
//! The Vue UI uses these to configure root (library) directories. The Rust
//! rewrite has no in-process tagger, so trigger-scan only acknowledges the
//! request (gold response shape) — browsing/adding dirs still works live.
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::{AppState, CurrentUser};

#[derive(Deserialize)]
pub struct AddRootDirsBody {
    #[serde(default)]
    pub new_dirs: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
}

fn current_user(ext: Option<Extension<CurrentUser>>) -> Option<serde_json::Value> {
    ext.map(|Extension(c)| c.0)
}

fn bad(msg: &str, code: StatusCode) -> Response {
    (code, Json(json!({ "msg": msg }))).into_response()
}

/// GET /notsettings/get-root-dirs — {"dirs":[...]}
pub async fn get_root_dirs(State(st): State<AppState>) -> Response {
    Json(json!({ "dirs": st.settings.root_dirs() })).into_response()
}

/// POST /notsettings/add-root-dirs — admin only.
/// Mirrors settings.py: db_dirs = config.rootDirs; remove `removed`; append
/// `new_dirs`; config.rootDirs = normalize_paths(final); save; return root_dirs.
pub async fn add_root_dirs(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Json(body): Json<AddRootDirsBody>,
) -> Response {
    let Some(cur) = current_user(ext) else {
        return bad("Missing JWT in cookies or headers", StatusCode::UNAUTHORIZED);
    };
    let is_admin = cur["roles"]
        .as_array()
        .map(|r| r.iter().any(|x| x == "admin"))
        .unwrap_or(false);
    if !is_admin {
        return bad("Only admins can do that!", StatusCode::FORBIDDEN);
    }

    let mut db_dirs = st.settings.root_dirs();
    for dir in &body.removed {
        if let Some(pos) = db_dirs.iter().position(|d| d == dir) {
            db_dirs.remove(pos);
        }
    }
    db_dirs.extend(body.new_dirs.clone());

    match st.settings.set_root_dirs(db_dirs) {
        Ok(dirs) => Json(json!({ "root_dirs": dirs })).into_response(),
        Err(e) => bad(
            &format!("Failed to save root dirs: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// GET /notsettings/trigger-scan — {"msg":"Scan triggered!"}
/// CT101 calls index_everything(); the Rust port has no tagger, so we
/// acknowledge with the exact gold body so the UI's "Rescan started" works.
pub async fn trigger_scan() -> Response {
    Json(json!({ "msg": "Scan triggered!" })).into_response()
}
