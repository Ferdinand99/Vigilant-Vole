//! The device register (`push_devices`): which app installs get the pushes
//! of this server.
//!
//! The app makes one handle per connection at the relay and gives it to this
//! server. The handle is a secret (whoever holds it can ask the relay to push
//! to that device), so it is sealed at rest ([`PURPOSE`]); `handle_hash`, its
//! SHA-256, is what lookups and duplicate checks use. A device belongs to a
//! user and, when it registered with an API token, to that token: deleting
//! the user or the token deletes the device.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::crypto::Sealer;

/// The purpose handles are sealed for (their own derived key).
pub const PURPOSE: &str = "push-handle";

/// Most devices one user can have registered.
pub const MAX_PER_USER: i64 = 16;
/// Most devices on the whole server. Every device is a handle presented to
/// the relay at each alert, so the number is bounded.
pub const MAX_TOTAL: i64 = 256;
/// Most devices of one user that no push has reached yet. A handle that
/// doesn't exist makes the relay count an "unknown handle" against the whole
/// server (20 an hour), so handles nobody has confirmed are few.
pub const MAX_PENDING_PER_USER: i64 = 4;
/// Most devices on the whole server that no push has reached yet. The
/// per-user cap alone leaves room for many users to burn the relay's
/// unknown-handle allowance together (a read-only token may register).
pub const MAX_PENDING_TOTAL: i64 = 20;
/// Most new handles one user can register in an hour.
pub const MAX_NEW_PER_HOUR: i64 = 4;

/// The limits of [`register_with`].
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub per_user: i64,
    pub total: i64,
    pub pending_per_user: i64,
    pub pending_total: i64,
    pub new_per_hour: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            per_user: MAX_PER_USER,
            total: MAX_TOTAL,
            pending_per_user: MAX_PENDING_PER_USER,
            pending_total: MAX_PENDING_TOTAL,
            new_per_hour: MAX_NEW_PER_HOUR,
        }
    }
}

/// Whether `handle` has the shape of a relay handle: `hrh_` and a body of
/// URL-safe base64 characters. (The relay's handles are 54 characters long;
/// the length is left loose so a longer one later still registers.)
pub fn valid_handle(handle: &str) -> bool {
    handle.strip_prefix("hrh_").is_some_and(|body| {
        (20..=100).contains(&body.len())
            && body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// Lowercase hex SHA-256 of a handle (`push_devices.handle_hash`).
pub fn hash(handle: &str) -> String {
    hex::encode(Sha256::digest(handle.as_bytes()))
}

/// Why a device could not be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitReached {
    /// The user has [`MAX_PER_USER`] devices.
    User,
    /// The server has [`MAX_TOTAL`] devices.
    Server,
    /// The user has [`MAX_PENDING_PER_USER`] devices no push has reached.
    Pending,
    /// The server has [`MAX_PENDING_TOTAL`] devices no push has reached.
    PendingServer,
    /// The user registered [`MAX_NEW_PER_HOUR`] new handles in the last hour.
    Rate,
}

/// How a registration ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registered {
    Created(i64),
    /// The user had registered this handle before; the same id.
    Existing(i64),
}

impl Registered {
    pub fn id(self) -> i64 {
        match self {
            Registered::Created(id) | Registered::Existing(id) => id,
        }
    }
}

/// Registers `handle` for `user_id`. `token_id` is the API token the request
/// came with, if any. The same user registering the same handle again gets
/// the same id (and the newest token).
pub fn register(
    conn: &mut Connection,
    sealer: &Sealer,
    user_id: i64,
    token_id: Option<i64>,
    handle: &str,
) -> rusqlite::Result<Result<Registered, LimitReached>> {
    register_with(conn, sealer, user_id, token_id, handle, &Limits::default())
}

/// [`register`] with other limits.
pub fn register_with(
    conn: &mut Connection,
    sealer: &Sealer,
    user_id: i64,
    token_id: Option<i64>,
    handle: &str,
    limits: &Limits,
) -> rusqlite::Result<Result<Registered, LimitReached>> {
    let digest = hash(handle);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM push_devices WHERE user_id = ?1 AND handle_hash = ?2",
            params![user_id, digest],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        if token_id.is_some() {
            tx.execute(
                "UPDATE push_devices SET token_id = ?2 WHERE id = ?1",
                params![id, token_id],
            )?;
        }
        tx.commit()?;
        return Ok(Ok(Registered::Existing(id)));
    }
    let mine: i64 = tx.query_row(
        "SELECT COUNT(*) FROM push_devices WHERE user_id = ?1",
        params![user_id],
        |r| r.get(0),
    )?;
    if mine >= limits.per_user {
        return Ok(Err(LimitReached::User));
    }
    let all: i64 = tx.query_row("SELECT COUNT(*) FROM push_devices", [], |r| r.get(0))?;
    if all >= limits.total {
        return Ok(Err(LimitReached::Server));
    }
    let pending: i64 = tx.query_row(
        "SELECT COUNT(*) FROM push_devices WHERE user_id = ?1 AND last_used_at IS NULL",
        params![user_id],
        |r| r.get(0),
    )?;
    if pending >= limits.pending_per_user {
        return Ok(Err(LimitReached::Pending));
    }
    let pending_all: i64 = tx.query_row(
        "SELECT COUNT(*) FROM push_devices WHERE last_used_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    if pending_all >= limits.pending_total {
        return Ok(Err(LimitReached::PendingServer));
    }
    // Counted in a table of its own, so that deleting a device and adding it
    // again doesn't reset the count.
    let recent: i64 = tx.query_row(
        "SELECT COUNT(*) FROM push_registrations
         WHERE user_id = ?1 AND created_at > datetime('now', '-1 hour')",
        params![user_id],
        |r| r.get(0),
    )?;
    if recent >= limits.new_per_hour {
        return Ok(Err(LimitReached::Rate));
    }
    tx.execute(
        "DELETE FROM push_registrations WHERE created_at <= datetime('now', '-1 day')",
        [],
    )?;
    tx.execute(
        "INSERT INTO push_registrations (user_id) VALUES (?1)",
        params![user_id],
    )?;
    tx.execute(
        "INSERT INTO push_devices (user_id, token_id, handle_sealed, handle_hash)
         VALUES (?1, ?2, ?3, ?4)",
        params![user_id, token_id, sealer.seal(handle), digest],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(Ok(Registered::Created(id)))
}

/// Deletes the device `id` if it is `user_id`'s; whether there was one.
pub fn delete_own(conn: &Connection, user_id: i64, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM push_devices WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
    )? == 1)
}

/// Deletes every registration of these handles (the relay answered `gone`:
/// the handle is unknown, revoked, or its device is no longer valid).
pub fn forget(conn: &Connection, handles: &[String]) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("DELETE FROM push_devices WHERE handle_hash = ?1")?;
    for handle in handles {
        stmt.execute(params![hash(handle)])?;
    }
    Ok(())
}

/// Records that the relay took a push for these handles.
pub fn mark_used(conn: &Connection, handles: &[String]) -> rusqlite::Result<()> {
    let mut stmt = conn
        .prepare("UPDATE push_devices SET last_used_at = datetime('now') WHERE handle_hash = ?1")?;
    for handle in handles {
        stmt.execute(params![hash(handle)])?;
    }
    Ok(())
}

/// How many devices are registered.
pub fn count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM push_devices", [], |r| r.get(0))
}

/// Every handle to push to, opened, oldest first and each once (two users
/// who registered the same handle get one push), with whether a push to it
/// was ever accepted. Devices of disabled users are left out: they could no
/// longer fetch the text. So are devices whose handle can't be opened
/// (another `session.key`), which are counted in the second value.
pub fn handles(
    conn: &Connection,
    sealer: &Sealer,
) -> rusqlite::Result<(Vec<(String, bool)>, usize)> {
    let mut stmt = conn.prepare(
        "SELECT d.handle_sealed, d.last_used_at IS NOT NULL FROM push_devices d
         JOIN users u ON u.id = d.user_id AND u.active = 1
         ORDER BY d.id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?;
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut unreadable = 0;
    for row in rows {
        let (sealed, proven) = row?;
        match sealer.open(&sealed) {
            Ok(handle) => match out.iter_mut().find(|(h, _)| *h == handle) {
                Some(seen) => seen.1 |= proven,
                None => out.push((handle, proven)),
            },
            Err(_) => unreadable += 1,
        }
    }
    Ok((out, unreadable))
}

/// Whether the server has an active `hedgerow` notification channel: push
/// is off without one. A product without a channels table has none.
pub fn has_active_channel(conn: &Connection) -> rusqlite::Result<bool> {
    let query = format!(
        "SELECT EXISTS (SELECT 1 FROM {} WHERE kind = 'hedgerow' AND active = 1)",
        crate::notify::sealed::TABLE
    );
    match conn.query_row(&query, [], |r| r.get::<_, bool>(0)) {
        Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {
            Ok(false)
        }
        other => other,
    }
}
