//! HS256 JWT — parity with flask-jwt-extended (PyJWT) token format.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

pub const ACCESS_EXPIRES: i64 = 2_592_000; // 30 days — matches JWT_ACCESS_TOKEN_EXPIRES

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn b64d(s: &str) -> Result<Vec<u8>, ()> {
    URL_SAFE_NO_PAD.decode(s).map_err(|_| ())
}

fn sign_raw(secret: &str, msg: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(msg.as_bytes());
    b64(&mac.finalize().into_bytes())
}

pub fn create_token(secret: &str, identity: &Value, token_type: &str) -> String {
    let iat = now();
    let payload = json!({
        "fresh": false,
        "iat": iat,
        "jti": uuid::Uuid::new_v4().to_string(),
        "type": token_type,
        "sub": identity,
        "nbf": iat,
        "exp": iat + ACCESS_EXPIRES,
    });
    let header = json!({"alg": "HS256", "typ": "JWT"});
    let msg = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(payload.to_string().as_bytes())
    );
    let sig = sign_raw(secret, &msg);
    format!("{msg}.{sig}")
}

pub enum VerifyError {
    Malformed,
    InvalidSignature,
    Expired,
    NotYetValid,
    WrongType(&'static str),
}

/// Verify a token. `refresh == false` requires a non-refresh token
/// (fje message parity); `refresh == true` requires a refresh token.
pub fn verify(secret: &str, token: &str, refresh: bool) -> Result<Value, VerifyError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(VerifyError::Malformed);
    }
    let (h, p, s) = (parts[0], parts[1], parts[2]);

    let hdr = b64d(h).map_err(|_| VerifyError::Malformed)?;
    let _: Value = serde_json::from_slice(&hdr).map_err(|_| VerifyError::Malformed)?;

    let sig = b64d(s).map_err(|_| VerifyError::Malformed)?;
    let msg = format!("{h}.{p}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(msg.as_bytes());
    mac.verify_slice(&sig)
        .map_err(|_| VerifyError::InvalidSignature)?;

    let payload: Value =
        serde_json::from_slice(&b64d(p).map_err(|_| VerifyError::Malformed)?)
            .map_err(|_| VerifyError::Malformed)?;

    let typ = payload.get("type").and_then(|v| v.as_str()).unwrap_or("access");
    if refresh && typ != "refresh" {
        return Err(VerifyError::WrongType("Only refresh tokens are allowed"));
    }
    if !refresh && typ == "refresh" {
        return Err(VerifyError::WrongType("Only non-refresh tokens are allowed"));
    }

    let now = now();
    if let Some(exp) = payload.get("exp").and_then(|v| v.as_i64()) {
        if exp < now {
            return Err(VerifyError::Expired);
        }
    }
    if let Some(nbf) = payload.get("nbf").and_then(|v| v.as_i64()) {
        if nbf > now {
            return Err(VerifyError::NotYetValid);
        }
    }

    Ok(payload)
}
