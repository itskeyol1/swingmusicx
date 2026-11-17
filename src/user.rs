//! User model + user table — parity with swingmusic models/user.py + db/userdata.py UserTable.
use rusqlite::{params, Connection};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct User {
    pub id: i64,
    pub image: Option<String>,
    pub password: String,
    pub username: String,
    pub roles: Vec<String>,
    pub extra: Value,
}

impl User {
    /// dict(minus password), keys sorted (Flask jsonify sort_keys parity via BTreeMap).
    pub fn todict(&self) -> Value {
        json!({
            "id": self.id,
            "image": self.image,
            "username": self.username,
            "roles": self.roles,
            "extra": self.extra,
        })
    }

    pub fn todict_simplified(&self) -> Value {
        json!({
            "id": self.id,
            "username": self.username,
            "image": self.image,
            "firstname": self.extra.get("firstname").and_then(|v| v.as_str()).unwrap_or(""),
        })
    }
}

fn row_to_user(row: &rusqlite::Row) -> rusqlite::Result<User> {
    let roles_raw: String = row.get(4)?;
    let roles: Vec<String> = serde_json::from_str(&roles_raw).unwrap_or_default();
    let extra_raw: Option<String> = row.get(5)?;
    let extra: Value = extra_raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    Ok(User {
        id: row.get(0)?,
        image: row.get(1)?,
        password: row.get(2)?,
        username: row.get(3)?,
        roles,
        extra,
    })
}

pub fn create_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS user (
    id INTEGER NOT NULL,
    image VARCHAR,
    password VARCHAR NOT NULL,
    username VARCHAR NOT NULL,
    roles JSON NOT NULL,
    extra JSON,
    PRIMARY KEY (id)
);
CREATE INDEX IF NOT EXISTS ix_user_username ON user (username);
"#,
    )
}

pub struct UserTable;

impl UserTable {
    pub fn get_all(conn: &Connection) -> Vec<User> {
        let mut stmt =
            match conn.prepare("SELECT id, image, password, username, roles, extra FROM user") {
                Ok(s) => s,
                Err(_) => return vec![],
            };
        let rows = stmt.query_map([], row_to_user);
        match rows {
            Ok(r) => r.filter_map(|x| x.ok()).collect(),
            Err(_) => vec![],
        }
    }

    pub fn get_by_id(conn: &Connection, id: i64) -> Option<User> {
        conn.query_row(
            "SELECT id, image, password, username, roles, extra FROM user WHERE id = ?1",
            params![id],
            row_to_user,
        )
        .ok()
    }

    pub fn get_by_username(conn: &Connection, username: &str) -> Option<User> {
        conn.query_row(
            "SELECT id, image, password, username, roles, extra FROM user WHERE username = ?1",
            params![username],
            row_to_user,
        )
        .ok()
    }

    pub fn insert_one(
        conn: &Connection,
        username: &str,
        password_hash: &str,
        roles: &[String],
    ) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO user (username, password, roles) VALUES (?1, ?2, ?3)",
            params![username, password_hash, serde_json::to_string(roles).unwrap()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// update_one parity: updates only the provided fields (image/password/username/roles).
    pub fn update_one(
        conn: &Connection,
        id: i64,
        username: Option<&str>,
        password: Option<&str>,
        roles: Option<&[String]>,
        image: Option<&str>,
    ) -> rusqlite::Result<()> {
        if let Some(u) = username {
            conn.execute("UPDATE user SET username = ?1 WHERE id = ?2", params![u, id])?;
        }
        if let Some(p) = password {
            conn.execute("UPDATE user SET password = ?1 WHERE id = ?2", params![p, id])?;
        }
        if let Some(r) = roles {
            conn.execute(
                "UPDATE user SET roles = ?1 WHERE id = ?2",
                params![serde_json::to_string(r).unwrap(), id],
            )?;
        }
        if let Some(i) = image {
            conn.execute("UPDATE user SET image = ?1 WHERE id = ?2", params![i, id])?;
        }
        Ok(())
    }

    pub fn remove_by_username(conn: &Connection, username: &str) -> rusqlite::Result<usize> {
        conn.execute("DELETE FROM user WHERE username = ?1", params![username])
    }
}
