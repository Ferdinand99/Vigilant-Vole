//! TOTP two-factor authentication (RFC 6238: SHA-1, 6 digits, 30 s steps,
//! ±1 step of clock skew) and single-use recovery codes.
//!
//! Replay protection: [`match_step`] only accepts steps *after* the last one
//! used; callers then record the step atomically
//! (`db::users::consume_totp_step`), so a code works once.

use argon2::password_hash::PasswordHasher;
use argon2::{Algorithm as ArgonAlgorithm, Argon2, Params, Version};
use axum_extra::extract::cookie::Key;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rusqlite::{Connection, OptionalExtension as _};
use totp_rs::{Algorithm, Builder, Secret, Totp};

use crate::crypto::{self, Sealer};
use crate::db::users as db_users;
use crate::http::ApiError;

pub const DIGITS: usize = 6;
pub const STEP_SECS: u64 = 30;
pub const SKEW_STEPS: u64 = 1;
pub const RECOVERY_CODE_COUNT: usize = 10;

/// Recovery code alphabet: lowercase, no look-alikes (0/o, 1/l/i).
const RECOVERY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
const RECOVERY_LEN: usize = 10;

/// A fresh 160-bit secret, base32 (what authenticator apps expect).
pub fn generate_secret() -> String {
    Secret::generate().to_base32()
}

/// `otpauth://` labels may not contain `:`.
fn label_part(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c == ':' { '-' } else { c }).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        crate::config::get().product.into()
    } else {
        trimmed.to_string()
    }
}

/// Builds the TOTP for a stored base32 secret.
pub fn build(secret_base32: &str, account: &str, issuer: &str) -> Result<Totp, ApiError> {
    let secret = Secret::try_from_base32(secret_base32)
        .map_err(|e| ApiError::internal(format!("bad totp secret: {e:?}")))?;
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(DIGITS as u8)
        .with_step_duration(STEP_SECS)
        .with_skew(SKEW_STEPS as u16)
        .with_secret(secret)
        .with_account_name(label_part(account))
        .with_issuer(Some(label_part(issuer)))
        .build()
        .map_err(|e| ApiError::internal(format!("totp setup failed: {e:?}")))
}

/// The `otpauth://totp/...` URL encoded in the QR code.
pub fn otpauth_url(totp: &Totp) -> Result<String, ApiError> {
    totp.to_url()
        .map_err(|e| ApiError::internal(format!("totp url failed: {e:?}")))
}

/// QR code for `data` as an `<img src>`-ready SVG data URI (black on white,
/// with a quiet zone, so it scans in dark mode too).
pub fn qr_data_uri(data: &str) -> Result<String, ApiError> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(data.as_bytes()).map_err(ApiError::internal)?;
    let svg = code
        .render::<svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(svg::Color("#000"))
        .light_color(svg::Color("#fff"))
        .build();
    Ok(format!(
        "data:image/svg+xml;base64,{}",
        STANDARD.encode(svg)
    ))
}

/// Groups a base32 secret in fours for manual entry.
pub fn format_secret(secret: &str) -> String {
    secret
        .as_bytes()
        .chunks(4)
        .filter_map(|c| std::str::from_utf8(c).ok())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strips spaces/dashes from a typed code; `None` unless exactly 6 digits.
pub fn normalize_code(input: &str) -> Option<String> {
    let digits: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    (digits.len() == DIGITS && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

/// Constant-time byte comparison.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The time step `code` is valid for at `now` (±[`SKEW_STEPS`]), skipping any
/// step at or before `last_used` (replay). Checks every candidate step so
/// timing doesn't reveal which one matched.
pub fn match_step(totp: &Totp, code: &str, now_unix: u64, last_used: Option<i64>) -> Option<u64> {
    let code = normalize_code(code)?;
    let current = now_unix / STEP_SECS;
    let mut matched = None;
    for step in current.saturating_sub(SKEW_STEPS)..=current + SKEW_STEPS {
        let fresh = last_used.is_none_or(|last| i64::try_from(step).is_ok_and(|s| s > last));
        let expected = totp.generate(step * STEP_SECS).to_string();
        if ct_eq(expected.as_bytes(), code.as_bytes()) && fresh && matched.is_none() {
            matched = Some(step);
        }
    }
    matched
}

/// The current code (tests, CLI helpers).
pub fn code_at(totp: &Totp, now_unix: u64) -> String {
    totp.generate(now_unix).to_string()
}

// ---------------------------------------------------------------------------
// Secrets at rest
// ---------------------------------------------------------------------------

/// The purpose TOTP secrets are sealed for (their own derived key, see
/// [`crate::crypto`]).
pub const PURPOSE: &str = "vole-kit/totp";

/// `secret` (base32), sealed for storage in `users.totp_secret`.
pub fn seal_secret(key: &Key, secret: &str) -> String {
    crypto::seal(key, PURPOSE, secret)
}

/// The base32 secret behind a stored `users.totp_secret`. Plaintext, as
/// versions before sealing wrote it, is returned as it is (until
/// [`seal_stored`] has sealed it, a login must keep working); `None` when
/// the value is sealed but can't be opened (the instance key was replaced).
pub fn open_secret(key: &Key, stored: &str) -> Option<String> {
    if !crypto::is_sealed(stored) {
        return Some(stored.to_string());
    }
    match crypto::open(key, PURPOSE, stored) {
        Ok(secret) => Some(secret),
        Err(err) => {
            tracing::warn!(%err, "a stored two-factor secret can't be opened; the user has to set up two-factor again");
            None
        }
    }
}

/// Seals the plaintext TOTP secrets of every user in place and returns how
/// many changed. Run at startup after the migrations ([`Core::new`] does,
/// in the background): secrets sealed already (the [`crypto::PREFIX`]) stay
/// as they are, so a second run changes nothing. A secret saved or removed
/// while it runs is left alone (compare-and-swap).
///
/// [`Core::new`]: crate::state::Core::new
pub fn seal_stored(conn: &Connection, key: &Key) -> rusqlite::Result<usize> {
    if !users_table_exists(conn)? {
        return Ok(0);
    }
    let sealer = Sealer::new(key, PURPOSE);
    let mut changed = 0;
    for (id, stored) in db_users::stored_totp_secrets(conn)? {
        if stored.is_empty() || crypto::is_sealed(&stored) {
            continue;
        }
        if db_users::swap_totp_secret(conn, id, &stored, &sealer.seal(&stored))? {
            changed += 1;
        }
    }
    if changed > 0 {
        tracing::info!(users = changed, "sealed the stored two-factor secrets");
    }
    Ok(changed)
}

/// The opposite of [`seal_stored`], for a **copy** of the database that has
/// to work without this instance's key (a backup download): every secret
/// that opens is written back in plaintext. Secrets that don't open stay
/// sealed. Returns how many changed. Never run it on the live database.
pub fn open_stored(conn: &Connection, key: &Key) -> rusqlite::Result<usize> {
    if !users_table_exists(conn)? {
        return Ok(0);
    }
    let mut changed = 0;
    for (id, stored) in db_users::stored_totp_secrets(conn)? {
        if !crypto::is_sealed(&stored) {
            continue;
        }
        if let Ok(plain) = crypto::open(key, PURPOSE, &stored)
            && db_users::swap_totp_secret(conn, id, &stored, &plain)?
        {
            changed += 1;
        }
    }
    Ok(changed)
}

/// Whether any TOTP secret is still stored in plaintext.
pub fn has_plaintext_stored(conn: &Connection) -> rusqlite::Result<bool> {
    if !users_table_exists(conn)? {
        return Ok(false);
    }
    Ok(db_users::stored_totp_secrets(conn)?
        .iter()
        .any(|(_, stored)| !stored.is_empty() && !crypto::is_sealed(stored)))
}

fn users_table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'users'",
        [],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

// ---------------------------------------------------------------------------
// Recovery codes
// ---------------------------------------------------------------------------

fn random_recovery_code() -> String {
    // Rejection sampling keeps the distribution uniform.
    let limit = 256 - (256 % RECOVERY_ALPHABET.len());
    let mut out = String::with_capacity(RECOVERY_LEN + 1);
    let mut buf = [0u8; 32];
    while out.len() < RECOVERY_LEN + 1 {
        rand::fill(&mut buf);
        for b in buf {
            if out.len() == RECOVERY_LEN / 2 {
                out.push('-');
            }
            if out.len() == RECOVERY_LEN + 1 {
                break;
            }
            if usize::from(b) < limit {
                out.push(char::from(
                    RECOVERY_ALPHABET[usize::from(b) % RECOVERY_ALPHABET.len()],
                ));
            }
        }
    }
    out
}

/// Ten fresh codes like `k7m2p-x9q4r` (≈50 bits each). Shown once.
pub fn generate_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODE_COUNT)
        .map(|_| random_recovery_code())
        .collect()
}

/// Canonical form for hashing/comparison: lowercase, no separators.
pub fn normalize_recovery_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Recovery codes are high-entropy, so a light Argon2id setting is plenty and
/// keeps checking ten of them fast.
fn recovery_hasher() -> Argon2<'static> {
    let params = Params::new(8 * 1024, 1, 1, None).unwrap_or(Params::DEFAULT);
    Argon2::new(ArgonAlgorithm::Argon2id, Version::V0x13, params)
}

pub fn hash_recovery_code(code: &str) -> Result<String, ApiError> {
    recovery_hasher()
        .hash_password(normalize_recovery_code(code).as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| ApiError::internal(format!("recovery code hashing failed: {e}")))
}

pub fn hash_recovery_codes(codes: &[String]) -> Result<Vec<String>, ApiError> {
    codes.iter().map(|c| hash_recovery_code(c)).collect()
}

/// Looks like a recovery code (so the 2FA form can accept either kind).
pub fn looks_like_recovery_code(input: &str) -> bool {
    let n = normalize_recovery_code(input);
    n.len() == RECOVERY_LEN && n.bytes().all(|b| RECOVERY_ALPHABET.contains(&b))
}

/// The stored hash matching `input`, if any (caller removes it to make the
/// code single-use).
pub fn find_recovery_code(hashes: &[String], input: &str) -> Option<String> {
    if !looks_like_recovery_code(input) {
        return None;
    }
    let normalized = normalize_recovery_code(input);
    hashes
        .iter()
        .find(|h| super::verify_password(h, &normalized))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totp() -> Totp {
        build("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP", "ada:admin", "Vole").unwrap()
    }

    const NOW: u64 = 1_760_000_015; // mid-step

    #[test]
    fn current_code_is_accepted() {
        let t = totp();
        let code = code_at(&t, NOW);
        assert_eq!(match_step(&t, &code, NOW, None), Some(NOW / STEP_SECS));
    }

    #[test]
    fn codes_one_step_away_are_accepted() {
        let t = totp();
        let prev = code_at(&t, NOW - STEP_SECS);
        let next = code_at(&t, NOW + STEP_SECS);
        assert!(match_step(&t, &prev, NOW, None).is_some());
        assert!(match_step(&t, &next, NOW, None).is_some());
    }

    #[test]
    fn codes_two_steps_away_are_rejected() {
        let t = totp();
        let old = code_at(&t, NOW - 2 * STEP_SECS);
        assert_eq!(match_step(&t, &old, NOW, None), None);
    }

    #[test]
    fn replayed_step_is_rejected() {
        let t = totp();
        let code = code_at(&t, NOW);
        let step = match_step(&t, &code, NOW, None).unwrap() as i64;
        assert_eq!(match_step(&t, &code, NOW, Some(step)), None);
    }

    #[test]
    fn previous_step_code_is_rejected_after_a_newer_one_was_used() {
        let t = totp();
        let prev = code_at(&t, NOW - STEP_SECS);
        assert_eq!(
            match_step(&t, &prev, NOW, Some((NOW / STEP_SECS) as i64)),
            None
        );
    }

    #[test]
    fn spaced_codes_are_normalised() {
        let t = totp();
        let code = code_at(&t, NOW);
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert!(match_step(&t, &spaced, NOW, None).is_some());
    }

    #[test]
    fn malformed_codes_are_rejected() {
        let t = totp();
        for bad in ["", "12345", "1234567", "abcdef", "+12345"] {
            assert_eq!(match_step(&t, bad, NOW, None), None, "{bad}");
        }
    }

    #[test]
    fn colons_in_labels_do_not_break_the_url() {
        let url = otpauth_url(&totp()).unwrap();
        assert!(url.starts_with("otpauth://totp/Vole:ada-admin?"), "{url}");
    }

    #[test]
    fn qr_is_an_svg_data_uri() {
        assert!(
            qr_data_uri("otpauth://totp/x")
                .unwrap()
                .starts_with("data:image/svg+xml;base64,")
        );
    }

    #[test]
    fn secrets_are_160_bit_base32() {
        let s = generate_secret();
        assert_eq!(Secret::try_from_base32(&s).unwrap().as_bytes().len(), 20);
    }

    #[test]
    fn recovery_codes_have_the_expected_shape() {
        let codes = generate_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        for c in &codes {
            assert_eq!(c.len(), 11, "{c}");
            assert_eq!(c.as_bytes()[5], b'-');
            assert!(looks_like_recovery_code(c));
        }
    }

    #[test]
    fn recovery_codes_are_unique() {
        let codes = generate_recovery_codes();
        let set: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(set.len(), codes.len());
    }

    #[test]
    fn recovery_code_matches_its_hash_in_any_format() {
        let codes = vec!["abcde-fghjk".to_string(), "mnpqr-stuvw".to_string()];
        let hashes = hash_recovery_codes(&codes).unwrap();
        assert_eq!(
            find_recovery_code(&hashes, "MNPQR STUVW"),
            Some(hashes[1].clone())
        );
        assert_eq!(find_recovery_code(&hashes, "zzzzz-zzzzz"), None);
    }

    #[test]
    fn ct_eq_compares_content_and_length() {
        assert!(ct_eq(b"123456", b"123456"));
        assert!(!ct_eq(b"123456", b"123457"));
        assert!(!ct_eq(b"12345", b"123456"));
    }

    #[test]
    fn secret_is_grouped_for_reading() {
        assert_eq!(format_secret("ABCDEFGHIJ"), "ABCD EFGH IJ");
    }

    // -- Secrets at rest ----------------------------------------------------

    const SECRET: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";

    fn key(byte: u8) -> Key {
        Key::from(&[byte; 64])
    }

    fn stored(conn: &Connection, id: i64) -> Option<String> {
        conn.query_row("SELECT totp_secret FROM users WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[test]
    fn a_sealed_secret_opens_and_hides_the_plaintext() {
        let sealed = seal_secret(&key(1), SECRET);
        assert!(crypto::is_sealed(&sealed));
        assert!(!sealed.contains(SECRET));
        assert_eq!(open_secret(&key(1), &sealed).as_deref(), Some(SECRET));
        // Another instance key (a replaced session.key) can't open it.
        assert_eq!(open_secret(&key(2), &sealed), None);
        // Sealed for another purpose: not a TOTP secret.
        let other = crypto::seal(&key(1), "vole-kit/notification-channels", SECRET);
        assert_eq!(open_secret(&key(1), &other), None);
    }

    #[test]
    fn plaintext_from_before_sealing_still_opens() {
        assert_eq!(open_secret(&key(1), SECRET).as_deref(), Some(SECRET));
    }

    #[test]
    fn stored_secrets_are_sealed_once() {
        let conn = crate::db::test_conn();
        let (a, b, c) = (
            db_users::insert_user(&conn, "a", "h", crate::auth::Role::Viewer).unwrap(),
            db_users::insert_user(&conn, "b", "h", crate::auth::Role::Viewer).unwrap(),
            // No secret at all.
            db_users::insert_user(&conn, "c", "h", crate::auth::Role::Viewer).unwrap(),
        );
        let k = key(3);
        // `a`: plaintext, 2FA on (from before sealing). `b`: sealed already.
        db_users::set_pending_totp_secret(&conn, a, SECRET).unwrap();
        db_users::enable_totp(&conn, a, 7, &["hash".into()]).unwrap();
        let sealed_b = seal_secret(&k, "KRSXG5CTMVRXEZLUKRSXG5CTMVRXEZLU");
        db_users::set_pending_totp_secret(&conn, b, &sealed_b).unwrap();
        assert!(has_plaintext_stored(&conn).unwrap());

        assert_eq!(seal_stored(&conn, &k).unwrap(), 1);
        let now_a = stored(&conn, a).unwrap();
        assert!(crypto::is_sealed(&now_a));
        assert_eq!(open_secret(&k, &now_a).as_deref(), Some(SECRET));
        // What was sealed stays byte for byte, and nothing is left to seal.
        assert_eq!(stored(&conn, b).as_deref(), Some(sealed_b.as_str()));
        assert_eq!(stored(&conn, c), None);
        assert!(!has_plaintext_stored(&conn).unwrap());

        // A second run changes nothing, not even the nonce.
        assert_eq!(seal_stored(&conn, &k).unwrap(), 0);
        assert_eq!(stored(&conn, a).as_deref(), Some(now_a.as_str()));
        // 2FA, the replay guard and the recovery codes are untouched.
        let tf = db_users::get_totp(&conn, a).unwrap().unwrap();
        assert!(tf.enabled);
        assert_eq!(tf.last_step, Some(7));
        assert_eq!(tf.recovery_codes, ["hash"]);
    }

    #[test]
    fn a_secret_that_changed_meanwhile_is_not_overwritten() {
        let conn = crate::db::test_conn();
        let id = db_users::insert_user(&conn, "a", "h", crate::auth::Role::Viewer).unwrap();
        db_users::set_pending_totp_secret(&conn, id, SECRET).unwrap();
        // `seal_stored` read the old text; the user started over before it wrote.
        let old = db_users::stored_totp_secrets(&conn).unwrap().remove(0).1;
        db_users::set_pending_totp_secret(&conn, id, "KRSXG5CTMVRXEZLUKRSXG5CTMVRXEZLU").unwrap();
        assert!(!db_users::swap_totp_secret(&conn, id, &old, "sealed:v1:x").unwrap());
        assert_eq!(
            stored(&conn, id).as_deref(),
            Some("KRSXG5CTMVRXEZLUKRSXG5CTMVRXEZLU")
        );
    }

    #[test]
    fn a_copy_for_another_instance_gets_its_secrets_opened() {
        let conn = crate::db::test_conn();
        let a = db_users::insert_user(&conn, "a", "h", crate::auth::Role::Viewer).unwrap();
        let b = db_users::insert_user(&conn, "b", "h", crate::auth::Role::Viewer).unwrap();
        let lost = seal_secret(&key(9), SECRET);
        db_users::set_pending_totp_secret(&conn, a, &seal_secret(&key(3), SECRET)).unwrap();
        db_users::set_pending_totp_secret(&conn, b, &lost).unwrap();
        // `a` opens; `b` was sealed by another key and stays as it is.
        assert_eq!(open_stored(&conn, &key(3)).unwrap(), 1);
        assert_eq!(stored(&conn, a).as_deref(), Some(SECRET));
        assert_eq!(stored(&conn, b).as_deref(), Some(lost.as_str()));
        // The other instance seals it again at its start.
        assert_eq!(seal_stored(&conn, &key(4)).unwrap(), 1);
        assert_eq!(
            open_secret(&key(4), &stored(&conn, a).unwrap()).as_deref(),
            Some(SECRET)
        );
    }

    #[test]
    fn sealing_a_database_without_users_is_a_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(seal_stored(&conn, &key(1)).unwrap(), 0);
        assert!(!has_plaintext_stored(&conn).unwrap());
    }
}
