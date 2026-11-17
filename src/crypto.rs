//! Password hashing + JWT secret, parity with swingmusic utils/auth.py + lib/crypto.py.
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// PBKDF2-HMAC-SHA256, 100_000 iterations, salt = serverId — byte parity with
/// Python `hashlib.pbkdf2_hmac("sha256", pw, serverId, 100000).hex()`.
pub fn hash_password(password: &str, server_id: &str) -> String {
    let mut out = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), server_id.as_bytes(), 100_000, &mut out);
    hex::encode(out)
}

/// Constant-time string compare (Python hmac.compare_digest).
pub fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Returns (verified, with_legacy) — parity with utils/auth.py check_password.
pub fn check_password(
    password: &str,
    hashed: &str,
    server_id: &str,
    legacy_server_id: &str,
) -> (bool, bool) {
    let mut verified = false;
    let mut with_legacy = false;

    if !legacy_server_id.is_empty() {
        verified = ct_eq(&hash_password(password, legacy_server_id), hashed);
        with_legacy = verified;
    }
    if !verified {
        verified = ct_eq(&hash_password(password, server_id), hashed);
    }
    (verified, with_legacy)
}

/// Load (creating if missing, mode 0600) the JWT signing secret —
/// parity with lib/crypto.py load_jwt_secret (secrets.token_hex(32)).
pub fn load_jwt_secret(path: &Path) -> std::io::Result<String> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        let secret: [u8; 32] = rand::random();
        f.write_all(hex::encode(secret).as_bytes())?;
    }
    Ok(std::fs::read_to_string(path)?.trim().to_string())
}
