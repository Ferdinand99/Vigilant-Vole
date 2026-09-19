use std::path::Path;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum_extra::extract::cookie::Key;

pub const SESSION_COOKIE: &str = "vv_session";

pub fn hash_password(password: &str) -> String {
    Argon2::default()
        .hash_password(password.as_bytes())
        .expect("failed to hash password")
        .to_string()
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Loads the signed-cookie session key from `<data_dir>/session.key`, generating
/// and persisting a new one on first run so sessions survive container restarts.
pub fn load_or_create_key(data_dir: &Path) -> Key {
    let key_path = data_dir.join("session.key");
    if let Ok(bytes) = std::fs::read(&key_path)
        && bytes.len() >= 64
    {
        return Key::from(&bytes);
    }
    let key = Key::generate();
    std::fs::write(&key_path, key.master()).expect("failed to persist session key");
    key
}
