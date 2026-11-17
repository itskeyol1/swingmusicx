//! Auth routes — byte-parity with swingmusic api/auth.py (v2.1.14).
use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::user::UserTable;
use crate::{crypto, jwt, AppState, CurrentUser};

fn db(st: &AppState) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(&st.paths.app_db_path)?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    Ok(conn)
}

fn bad(msg: &str, code: StatusCode) -> Response {
    (code, Json(json!({ "msg": msg }))).into_response()
}

fn cookie_val(name: &str, value: &str, max_age: i64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Max-Age={max_age}; Path=/; HttpOnly; SameSite=None"
    ))
    .unwrap()
}

/// create_new_token — parity api/auth.py.
pub fn create_new_token(st: &AppState, identity: &Value) -> Value {
    let access = jwt::create_token(&st.jwt_secret, identity, "access");
    let refresh = jwt::create_token(&st.jwt_secret, identity, "refresh");
    let username = identity
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    json!({
        "msg": format!("Logged in as {username}"),
        "accesstoken": access,
        "refreshtoken": refresh,
        "maxage": jwt::ACCESS_EXPIRES,
    })
}

fn json_with_cookie(body: Value, cookie: HeaderValue) -> Response {
    let mut res = Json(body).into_response();
    res.headers_mut().append(header::SET_COOKIE, cookie);
    res
}

fn current_user(ext: Option<Extension<CurrentUser>>) -> Option<Value> {
    ext.map(|Extension(c)| c.0)
}

#[derive(Deserialize)]
pub struct LoginBody {
    username: String,
    password: String,
}

pub async fn login(State(st): State<AppState>, Json(body): Json<LoginBody>) -> Response {
    let Ok(conn) = db(&st) else {
        return bad("Database error", StatusCode::INTERNAL_SERVER_ERROR);
    };
    let Some(user) = UserTable::get_by_username(&conn, &body.username) else {
        return bad("User not found", StatusCode::NOT_FOUND);
    };

    let (ok, with_legacy) = crypto::check_password(
        &body.password,
        &user.password,
        &st.settings.server_id(),
        &st.settings.legacy_server_id(),
    );
    if !ok {
        return bad("Hehe! invalid password", StatusCode::UNAUTHORIZED);
    }

    if with_legacy {
        let rehashed = crypto::hash_password(&body.password, &st.settings.server_id());
        let _ = UserTable::update_one(&conn, user.id, None, Some(&rehashed), None, None);
    }

    let res = create_new_token(&st, &user.todict());
    let token = res["accesstoken"].as_str().unwrap_or("").to_string();
    json_with_cookie(res, cookie_val("access_token_cookie", &token, jwt::ACCESS_EXPIRES))
}

pub async fn get_pair(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let identity = current_user(ext).unwrap_or(Value::Null);
    let token = create_new_token(&st, &identity);
    let key = token["accesstoken"]
        .as_str()
        .unwrap_or("")
        .chars()
        .rev()
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    *st.pair.lock().unwrap() = Some((key.clone(), token));
    Json(json!({ "code": key })).into_response()
}

#[derive(Deserialize)]
pub struct PairQuery {
    #[serde(default)]
    code: String,
}

pub async fn pair_with_code(State(st): State<AppState>, Query(q): Query<PairQuery>) -> Response {
    let mut pair = st.pair.lock().unwrap();
    match pair.as_ref() {
        Some((key, token)) if *key == q.code => {
            let token = token.clone();
            *pair = None; // one-shot
            Json(token).into_response()
        }
        _ => bad("Invalid code", StatusCode::BAD_REQUEST),
    }
}

pub async fn refresh(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let Some(identity) = current_user(ext) else {
        return bad(
            "Missing JWT in cookies or headers (Missing cookie \"access_token_cookie\"; Missing Authorization Header)",
            StatusCode::UNAUTHORIZED,
        );
    };
    Json(create_new_token(&st, &identity)).into_response()
}

#[derive(Deserialize)]
pub struct UpdateProfileBody {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    #[allow(dead_code)]
    email: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    roles: Option<Vec<String>>,
}

fn is_reserved_username(username: &str) -> bool {
    username.trim().to_lowercase() == "guest"
}

pub async fn update_profile(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Json(body): Json<UpdateProfileBody>,
) -> Response {
    let Some(cur) = current_user(ext) else {
        return bad("Missing JWT in cookies or headers", StatusCode::UNAUTHORIZED);
    };
    let cur_roles = cur["roles"].as_array().cloned().unwrap_or_default();
    let cur_is_guest = cur_roles.iter().any(|r| r == "guest");
    let cur_username = cur["username"].as_str().unwrap_or("");
    let cur_id = cur["id"].as_i64().unwrap_or(0);

    if cur_is_guest || is_reserved_username(cur_username) {
        return bad("Cannot update guest user", StatusCode::BAD_REQUEST);
    }

    let target_id = if body.id != 0 { body.id } else { cur_id };
    let is_admin = cur_roles.iter().any(|r| r == "admin");
    if target_id != cur_id && !is_admin {
        return bad("You can only update your own profile", StatusCode::FORBIDDEN);
    }

    let Ok(conn) = db(&st) else {
        return bad("Database error", StatusCode::INTERNAL_SERVER_ERROR);
    };
    let Some(target) = UserTable::get_by_id(&conn, target_id) else {
        return bad("User not found", StatusCode::NOT_FOUND);
    };
    if target.roles.iter().any(|r| r == "guest") || is_reserved_username(&target.username) {
        return bad("Cannot update guest user", StatusCode::BAD_REQUEST);
    }
    if is_reserved_username(&body.username) {
        return bad("Cannot update guest user", StatusCode::BAD_REQUEST);
    }

    if let Some(roles) = &body.roles {
        if !is_admin {
            return bad("Only admins can update roles", StatusCode::FORBIDDEN);
        }
        if !roles.iter().any(|r| r == "admin") {
            let all = UserTable::get_all(&conn);
            let admins: Vec<_> = all.iter().filter(|u| u.roles.iter().any(|r| r == "admin")).collect();
            if admins.len() == 1 && admins[0].id == target_id {
                return bad("Cannot remove the only admin", StatusCode::BAD_REQUEST);
            }
        }
    }

    let password_hash = if body.password.is_empty() {
        None
    } else {
        Some(crypto::hash_password(&body.password, &st.settings.server_id()))
    };
    let username = if body.username.is_empty() {
        None
    } else {
        Some(body.username.as_str())
    };
    let roles = body.roles.as_deref();

    if let Err(e) = UserTable::update_one(&conn, target_id, username, password_hash.as_deref(), roles, None) {
        if let rusqlite::Error::SqliteFailure(err, _) = &e {
            if err.code == rusqlite::ErrorCode::ConstraintViolation {
                return bad("Username already exists", StatusCode::BAD_REQUEST);
            }
        }
    }

    match UserTable::get_by_id(&conn, target_id) {
        Some(u) => Json(u.todict()).into_response(),
        None => bad("User not found", StatusCode::NOT_FOUND),
    }
}

pub async fn create_user(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Json(body): Json<UpdateProfileBody>,
) -> Response {
    if is_reserved_username(&body.username) {
        return bad("Username 'guest' is reserved", StatusCode::BAD_REQUEST);
    }
    let Ok(conn) = db(&st) else {
        return bad("Database error", StatusCode::INTERNAL_SERVER_ERROR);
    };
    let cur = current_user(ext);

    let users = UserTable::get_all(&conn);
    if users.is_empty() {
        let roles = vec!["admin".to_string()];
        let hash = crypto::hash_password(&body.password, &st.settings.server_id());
        if UserTable::insert_one(&conn, &body.username, &hash, &roles).is_err() {
            return bad("Failed to create user", StatusCode::INTERNAL_SERVER_ERROR);
        }
        let Some(user) = UserTable::get_by_username(&conn, &body.username) else {
            return bad("Failed to create user", StatusCode::INTERNAL_SERVER_ERROR);
        };
        let userhome = std::env::var("HOME").unwrap_or_default();
        let response = json!({ "user": user.todict(), "userhome": userhome });
        let token_res = create_new_token(&st, &user.todict());
        let token = token_res["accesstoken"].as_str().unwrap_or("").to_string();
        return json_with_cookie(response, cookie_val("access_token_cookie", &token, jwt::ACCESS_EXPIRES));
    }

    let is_admin = cur
        .as_ref()
        .map(|u| u["roles"].as_array().map(|r| r.iter().any(|x| x == "admin")).unwrap_or(false))
        .unwrap_or(false);
    if !is_admin {
        return bad("Only admins can do that!", StatusCode::FORBIDDEN);
    }
    if body.username.is_empty() || body.password.is_empty() {
        return bad("Username and password are required", StatusCode::BAD_REQUEST);
    }
    if UserTable::get_by_username(&conn, &body.username).is_some() {
        return bad("Username already exists", StatusCode::BAD_REQUEST);
    }
    let hash = crypto::hash_password(&body.password, &st.settings.server_id());
    if UserTable::insert_one(&conn, &body.username, &hash, &[]).is_err() {
        return bad("Failed to create user", StatusCode::INTERNAL_SERVER_ERROR);
    }
    match UserTable::get_by_username(&conn, &body.username) {
        Some(u) => Json(u.todict()).into_response(),
        None => bad("Failed to create user", StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn create_guest_user(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let Some(cur) = current_user(ext) else {
        return bad("Missing JWT in cookies or headers", StatusCode::UNAUTHORIZED);
    };
    let is_admin = cur["roles"].as_array().map(|r| r.iter().any(|x| x == "admin")).unwrap_or(false);
    if !is_admin {
        return bad("Only admins can do that!", StatusCode::FORBIDDEN);
    }
    let Ok(conn) = db(&st) else {
        return bad("Database error", StatusCode::INTERNAL_SERVER_ERROR);
    };
    if UserTable::get_by_username(&conn, "guest").is_some() {
        return bad("Guest user already exists", StatusCode::BAD_REQUEST);
    }
    // guest password is a random secret — parity with UserTable.insert_guest_user
    let pw = uuid::Uuid::new_v4().to_string();
    let hash = crypto::hash_password(&pw, &st.settings.server_id());
    match UserTable::insert_one(&conn, "guest", &hash, &["guest".to_string()]) {
        Ok(_) => Json(json!({ "msg": "Guest user created" })).into_response(),
        Err(_) => bad("Failed to create guest user", StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Deserialize)]
pub struct DeleteUserBody {
    #[serde(default)]
    username: String,
}

pub async fn delete_user(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Json(body): Json<DeleteUserBody>,
) -> Response {
    let Some(cur) = current_user(ext) else {
        return bad("Missing JWT in cookies or headers", StatusCode::UNAUTHORIZED);
    };
    let is_admin = cur["roles"].as_array().map(|r| r.iter().any(|x| x == "admin")).unwrap_or(false);
    if !is_admin {
        return bad("Only admins can do that!", StatusCode::FORBIDDEN);
    }
    if body.username == cur["username"].as_str().unwrap_or("") {
        return bad("Sorry! you cannot delete yourselfu", StatusCode::BAD_REQUEST);
    }
    let Ok(conn) = db(&st) else {
        return bad("Database error", StatusCode::INTERNAL_SERVER_ERROR);
    };
    let all = UserTable::get_all(&conn);
    let admins: Vec<_> = all.iter().filter(|u| u.roles.iter().any(|r| r == "admin")).collect();
    if admins.len() == 1 && admins[0].username == body.username {
        return bad("Cannot delete the only admin", StatusCode::BAD_REQUEST);
    }
    let target = UserTable::get_by_username(&conn, &body.username);
    let _ = UserTable::remove_by_username(&conn, &body.username);
    if let Some(t) = target {
        if let Some(img) = t.image {
            let _ = std::fs::remove_file(st.paths.user_img_path().join(img));
        }
    }
    Json(json!({ "msg": format!("User {} deleted", body.username) })).into_response()
}

pub async fn update_user_image() -> Response {
    // ponytail: avatar upload needs image+webp encoding; F1 leaves it stubbed.
    // Add when webclient avatar upload is exercised (`PUT /auth/profile/image`).
    bad("Not implemented", StatusCode::NOT_IMPLEMENTED)
}

pub async fn remove_user_image(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let Some(cur) = current_user(ext) else {
        return bad("Missing JWT in cookies or headers", StatusCode::UNAUTHORIZED);
    };
    let cur_roles = cur["roles"].as_array().cloned().unwrap_or_default();
    let cur_username = cur["username"].as_str().unwrap_or("");
    if cur_roles.iter().any(|r| r == "guest") || is_reserved_username(cur_username) {
        return bad("Cannot update guest user", StatusCode::BAD_REQUEST);
    }
    let Ok(conn) = db(&st) else {
        return bad("Database error", StatusCode::INTERNAL_SERVER_ERROR);
    };
    let id = cur["id"].as_i64().unwrap_or(0);
    let _ = UserTable::update_one(&conn, id, None, None, None, Some(""));
    match UserTable::get_by_id(&conn, id) {
        Some(u) => {
            if let Some(img) = &u.image {
                let _ = std::fs::remove_file(st.paths.user_img_path().join(img));
            }
            Json(u.todict()).into_response()
        }
        None => bad("User not found", StatusCode::NOT_FOUND),
    }
}

pub async fn logout() -> Response {
    let mut res = Json(json!({ "msg": "Logged out" })).into_response();
    res.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "access_token_cookie=; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/",
        ),
    );
    res
}

#[derive(Deserialize)]
pub struct AllUsersQuery {
    #[serde(default)]
    simplified: bool,
}

pub async fn get_all_users(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
    Query(q): Query<AllUsersQuery>,
) -> Response {
    let mut settings = json!({
        "enableGuest": false,
        "usersOnLogin": st.settings.users_on_login(),
    });
    let mut res = json!({ "settings": {}, "users": [] });

    let Ok(conn) = db(&st) else {
        return bad("Database error", StatusCode::INTERNAL_SERVER_ERROR);
    };
    let mut users = UserTable::get_all(&conn);
    let cur = current_user(ext);
    let is_admin = cur
        .as_ref()
        .map(|u| u["roles"].as_array().map(|r| r.iter().any(|x| x == "admin")).unwrap_or(false))
        .unwrap_or(false);
    let enable_guest = users.iter().any(|u| u.username == "guest");
    settings["enableGuest"] = json!(enable_guest);

    if is_admin {
        res["settings"] = settings.clone();
    } else if cur.is_some() {
        return Json(res).into_response();
    } else if !settings["usersOnLogin"].as_bool().unwrap_or(true) && !enable_guest {
        return Json(res).into_response();
    }

    if !settings["usersOnLogin"].as_bool().unwrap_or(true) {
        users.retain(|u| u.username == "guest");
    }

    users.reverse();
    if let Some(c) = &cur {
        let cur_username = c["username"].as_str().unwrap_or("");
        users.sort_by_key(|u| !u.roles.iter().any(|r| r == "admin")); // admins first (stable)
        users.sort_by_key(|u| u.username != cur_username); // current user index 0 (stable)
    } else {
        users.sort_by_key(|u| u.username == "guest"); // guest last (stable)
    }

    let simplified = q.simplified || cur.is_none();
    res["users"] = json!(users
        .iter()
        .map(|u| if simplified { u.todict_simplified() } else { u.todict() })
        .collect::<Vec<_>>());

    Json(res).into_response()
}

pub async fn get_logged_in_user(ext: Option<Extension<CurrentUser>>) -> Response {
    match current_user(ext) {
        Some(u) => Json(u).into_response(),
        None => bad(
            "Missing JWT in cookies or headers (Missing cookie \"access_token_cookie\"; Missing Authorization Header)",
            StatusCode::UNAUTHORIZED,
        ),
    }
}

pub async fn onboarding_data(
    State(st): State<AppState>,
    ext: Option<Extension<CurrentUser>>,
) -> Response {
    let Ok(conn) = db(&st) else {
        return bad("Database error", StatusCode::INTERNAL_SERVER_ERROR);
    };
    let users = UserTable::get_all(&conn);
    let admin_exists = users.iter().any(|u| u.roles.iter().any(|r| r == "admin"));
    let root_dirs_set = !st.settings.root_dirs().is_empty();
    let onboarding_complete = admin_exists;
    let is_admin = current_user(ext)
        .as_ref()
        .map(|u| u["roles"].as_array().map(|r| r.iter().any(|x| x == "admin")).unwrap_or(false))
        .unwrap_or(false);
    let mut res = json!({
        "onboardingComplete": onboarding_complete,
        "rootDirsSet": root_dirs_set,
        "adminExists": admin_exists,
        "isAdmin": is_admin,
        "rootDirs": st.settings.root_dirs(),
    });
    let _ = &mut res;
    Json(res).into_response()
}
