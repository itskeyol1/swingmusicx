//! SwingMusix — Rust rewrite of Swing Music v2.1.14 (AGPL-3.0-or-later).
//! F1: skeleton + global auth middleware + auth routes + docs/openapi + static client.
mod album;
mod auth;
mod crypto;
mod dates;
mod getall;
mod hashing;
mod jwt;
mod library;
mod models;
mod natsort;
mod cset;
mod parsers;
mod settings;
mod store;
mod user;
mod fuzz;
mod fuzz_tables;
mod unidecode;
mod unidecode_tables;
mod process;
mod search;
mod artist;
mod folder;
mod home;

#[cfg(test)]
mod fuzz_validate;

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use library::Library;
use models::UserConfig;
use settings::{Paths, Settings};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub paths: Paths,
    pub settings: Settings,
    pub jwt_secret: String,
    /// Global one-shot pair code — parity with api/auth.py `pair_token` dict.
    /// ponytail: process-local Mutex matches upstream single-process global.
    pub pair: Arc<Mutex<Option<(String, Value)>>>,
    pub library: Arc<Library>,
}

#[derive(Clone)]
pub struct CurrentUser(pub Value);

/// Middleware exempt list — parity with app_builder.py check_auth_need().
fn is_exempt(path: &str) -> bool {
    const URLS: [&str; 9] = [
        "/auth/login",
        "/auth/users",
        "/auth/pair",
        "/auth/logout",
        "/auth/refresh",
        "/auth/profile/create",
        "/events",
        "/docs",
        "/onboarding-data",
    ];
    if path == "/" {
        return true;
    }
    const FILES: [&str; 16] = [
        ".webp", ".jpg", ".js", ".css", ".html", ".json", ".png", ".svg", ".ico", ".woff",
        ".woff2", ".map", ".txt", ".ttf", ".gif", ".webmanifest",
    ];
    if FILES.iter().any(|f| path.ends_with(f)) {
        return true;
    }
    URLS.iter().any(|u| path.starts_with(u))
}

/// Routes that accept an optional token (fje jwt_required(optional=True)):
/// missing token is fine, invalid token still errors.
fn optional_auth(path: &str) -> bool {
    ["/auth/pair", "/auth/users", "/auth/profile/create", "/onboarding-data"]
        .iter()
        .any(|u| path.starts_with(u))
}

fn extract_token(req: &Request) -> Option<String> {
    // fje JWT_TOKEN_LOCATION = ["cookies", "headers"] — cookie wins when both present.
    if let Some(cookie) = req.headers().get(header::COOKIE) {
        if let Ok(v) = cookie.to_str() {
            for part in v.split(';') {
                let part = part.trim();
                if let Some(t) = part.strip_prefix("access_token_cookie=") {
                    if !t.is_empty() {
                        return Some(t.to_string());
                    }
                }
            }
        }
    }
    if let Some(authz) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(v) = authz.to_str() {
            if let Some(t) = v.strip_prefix("Bearer ") {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn fje_error(status: StatusCode, msg: String) -> Response {
    // Flask jsonify appends a trailing newline — match it byte-for-byte.
    let body = format!("{}\n", json!({ "msg": msg }));
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

async fn auth_middleware(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let exempt = is_exempt(&path);
    let has_authz_header = req.headers().contains_key(header::AUTHORIZATION);
    let token = extract_token(&req);

    let mut token_exp: Option<i64> = None;

    match token {
        Some(t) => match jwt::verify(&st.jwt_secret, &t, path.starts_with("/auth/refresh")) {
            Ok(payload) => {
                token_exp = payload.get("exp").and_then(|v| v.as_i64());
                let sub = payload.get("sub").cloned().unwrap_or(Value::Null);
                req.extensions_mut().insert(CurrentUser(sub));
            }
            Err(e) => {
                use jwt::VerifyError::*;
                return match e {
                    WrongType(m) => fje_error(StatusCode::UNPROCESSABLE_ENTITY, m.to_string()),
                    InvalidSignature => fje_error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Signature verification failed".to_string(),
                    ),
                    Expired => {
                        fje_error(StatusCode::UNAUTHORIZED, "Token has expired".to_string())
                    }
                    NotYetValid => fje_error(
                        StatusCode::UNAUTHORIZED,
                        "The token is not yet valid (nbf)".to_string(),
                    ),
                    Malformed => fje_error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Not enough segments".to_string(),
                    ),
                };
            }
        },
        None => {
            if !exempt && !optional_auth(&path) {
                return fje_error(
                    StatusCode::UNAUTHORIZED,
                    "Missing JWT in cookies or headers (Missing cookie \"access_token_cookie\"; Missing Authorization Header)".to_string(),
                );
            }
        }
    }

    let mut res = next.run(req).await;

    // parity: Flask's jsonify appends "\n" to every JSON body. Add it once here
    // so all JSON responses (2xx + handler errors) match CT101 byte-for-byte.
    if res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false)
    {
        let (parts, body) = res.into_parts();
        let mut bytes = match to_bytes(body, usize::MAX).await {
            Ok(b) => b.to_vec(),
            Err(_) => Vec::new(),
        };
        bytes.push(b'\n');
        res = Response::from_parts(parts, Body::from(bytes));
    }

    // after_request: refresh the access cookie when it expires within 7 days,
    // skipped when the caller used an Authorization header — parity app_builder.py.
    if let Some(exp) = token_exp {
        if !has_authz_header && exp - jwt::now() < 7 * 86_400 {
            if let Ok(user) = res.extensions().get::<CurrentUser>().map(|c| c.0.clone()).ok_or(()) {
                let fresh = jwt::create_token(&st.jwt_secret, &user, "access");
                if let Ok(val) = HeaderValue::from_str(&format!(
                    "access_token_cookie={fresh}; Max-Age={}; Path=/; HttpOnly; SameSite=None",
                    jwt::ACCESS_EXPIRES
                )) {
                    res.headers_mut().append(header::SET_COOKIE, val);
                }
            }
        }
    }

    res
}

/// Serve a file from the client dir — minimal stand-in for app.send_static_file.
async fn serve_static(st: &AppState, rel: &str) -> Response {
    let rel = rel.trim_start_matches('/');
    let path = if rel.is_empty() || rel == "index.html" {
        st.paths.client_path.join("index.html")
    } else {
        st.paths.client_path.join(rel)
    };
    // no traversal outside client dir
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                Body::from(bytes),
            )
                .into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

#[tokio::main]
async fn main() {
    let paths = Paths::new();
    let settings = Settings::load(paths.settings_file());
    let user_cfg = settings.from_settings();

    let jwt_secret = match crypto::load_jwt_secret(&paths.jwt_key_path()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: cannot load jwt secret: {e}");
            std::process::exit(1);
        }
    };

    // fresh-install parity: upstream creates tables on first run; then load
    // the in-memory library (setup/__init__.py startup order).
    let library = match rusqlite::Connection::open(&paths.app_db_path) {
        Ok(conn) => {
            if let Err(e) = user::create_tables(&conn) {
                eprintln!("warning: cannot create tables: {e}");
            }
            match Library::load(&conn, &user_cfg) {
                Ok(lib) => lib,
                Err(e) => {
                    eprintln!("warning: cannot load library: {e}");
                    Library::default()
                }
            }
        }
        Err(e) => {
            eprintln!("warning: cannot open db: {e}");
            Library::default()
        }
    };

    let state = AppState {
        paths,
        settings,
        jwt_secret,
        pair: Arc::new(Mutex::new(None)),
        library: Arc::new(library),
    };

    let port: u16 = std::env::var("SWINGMUSIC_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1970);

    let app = Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/getpaircode", get(auth::get_pair))
        .route("/auth/pair", get(auth::pair_with_code))
        .route("/auth/refresh", post(auth::refresh))
        .route("/auth/profile/update", put(auth::update_profile))
        .route("/auth/profile/create", post(auth::create_user))
        .route("/auth/profile/guest/create", post(auth::create_guest_user))
        .route("/auth/profile/delete", delete(auth::delete_user))
        .route("/auth/profile/image", put(auth::update_user_image).delete(auth::remove_user_image))
        .route("/auth/logout", get(auth::logout))
        .route("/auth/users", get(auth::get_all_users))
        .route("/auth/user", get(auth::get_logged_in_user))
        .route("/docs/openapi.json", get(docs_openapi))
        .route("/docs", get(docs_ui))
        .route("/openapi.json", get(docs_openapi))
        .route("/onboarding-data", get(auth::onboarding_data))
        .route("/getall/songs", get(getall::get_all_songs))
        .route("/getall/{itemtype}", get(getall::get_all_items))
        .route("/album", post(album::post_album))
        .route("/album/{albumhash}/tracks", get(album::get_album_tracks))
        .route("/album/from-artist", post(album::post_from_artist))
        .route("/album/other-versions", post(album::post_other_versions))
        .route("/album/similar", get(album::get_similar))
        .route("/artist/{artisthash}", get(artist::get_artist))
        .route("/artist/{artisthash}/albums", get(artist::get_artist_albums))
        .route("/artist/{artisthash}/tracks", get(artist::get_all_artist_tracks))
        .route("/artist/{artisthash}/similar", get(artist::get_similar_artists))
        .route("/search/top", get(search::get_top_results))
        .route("/search", get(search::search_items))
        .route("/search/", get(search::search_items))
        .route("/folder", post(folder::get_folder_tree))
        .route("/folder/", post(folder::get_folder_tree))
        .route("/folder/dir-browser", post(folder::list_folders))
        .route("/folder/show-in-files", get(folder::open_in_file_manager))
        .route("/folder/tracks/all", get(folder::get_tracks_in_path))
        .route("/nothome/", get(home::homepage_items))
        .route("/nothome/recents/added", get(home::get_recently_added))
        .route("/nothome/recents/played", get(home::get_recently_played))
        .route("/", get(root))
        .route("/{*path}", get(fallback_static))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind failed");
    println!("SwingMusix listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn root(State(st): State<AppState>, req: Request) -> Response {
    // /onboarding-data is exempt from auth but still routed normally; only
    // serve index.html for the literal root and client asset paths.
    if req.uri().path() == "/" {
        return serve_static(&st, "index.html").await;
    }
    serve_static(&st, req.uri().path()).await
}

async fn fallback_static(State(st): State<AppState>, req: Request) -> Response {
    serve_static(&st, req.uri().path()).await
}

async fn docs_ui() -> Response {
    let html = r#"<!doctype html><html><head><title>SwingMusix API</title>
<link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css"></head>
<body><div id="swagger-ui"></div>
<script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
<script>SwaggerUIBundle({url:'/docs/openapi.json',dom_id:'#swagger-ui'});</script>
</body></html>"#;
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

async fn docs_openapi() -> Response {
    let tags = json!([{"name": "Auth", "description": "Authentication stuff"}]);
    let mut paths = serde_json::Map::new();
    paths.insert(
        "/auth/login".to_string(),
        json!({"post": {"tags": ["Auth"], "summary": "Authenticate using username and password",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/LoginBody"}}}},
            "responses": {"200": {"description": "OK"}}}}),
    );
    paths.insert(
        "/auth/user".to_string(),
        json!({"get": {"tags": ["Auth"], "summary": "Get logged in user", "responses": {"200": {"description": "OK"}}}}),
    );
    paths.insert(
        "/auth/users".to_string(),
        json!({"get": {"tags": ["Auth"], "summary": "Get all users", "responses": {"200": {"description": "OK"}}}}),
    );
    paths.insert(
        "/auth/refresh".to_string(),
        json!({"post": {"tags": ["Auth"], "summary": "Refresh an access token", "responses": {"200": {"description": "OK"}}}}),
    );
    paths.insert(
        "/auth/logout".to_string(),
        json!({"get": {"tags": ["Auth"], "summary": "Log out and clear the access token cookie", "responses": {"200": {"description": "OK"}}}}),
    );
    paths.insert(
        "/auth/getpaircode".to_string(),
        json!({"get": {"tags": ["Auth"], "summary": "Get a new pair code", "responses": {"200": {"description": "OK"}}}}),
    );
    paths.insert(
        "/auth/pair".to_string(),
        json!({"get": {"tags": ["Auth"], "summary": "Get an access token by sending a pair code",
            "parameters": [{"name": "code", "in": "query", "schema": {"type": "string"}}],
            "responses": {"200": {"description": "OK"}}}}),
    );
    let spec = json!({
        "openapi": "3.1.0",
        "info": {"title": "Swing Music", "description": "Swing Music API", "version": "2.1.14"},
        "tags": tags,
        "paths": paths,
    });
    Json(spec).into_response()
}
