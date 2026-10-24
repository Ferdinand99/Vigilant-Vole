//! Channel secrets sealed at rest.
//!
//! A channel's config is the provider's JSON object. Its secret fields (the
//! ones the schema marks `secret`, the same ones [`redact`](super::schema::redact)
//! masks) are stored sealed ([`crate::crypto`], purpose [`PURPOSE`]); every
//! other field stays readable JSON. A product:
//!
//! * seals before it writes a config ([`seal_config`]);
//! * opens after it reads one, to send, test or edit ([`open_config`],
//!   [`open_channel`]);
//! * seals what an older version stored in plaintext, once at startup
//!   ([`seal_stored`]; idempotent).
//!
//! When the instance key is lost or replaced, a sealed secret can't be
//! opened. [`open_config`] then leaves the field out and names it in
//! [`Opened::unreadable`]; the edit form shows "enter it again"
//! ([`Opened::errors`]), and [`open_channel`] (sending) fails with the same
//! message instead of sending anything.

use axum_extra::extract::cookie::Key;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::alert::NotificationChannel;
use super::schema::{FieldError, ProviderSchema, SECRET_MASK, field_error};
use crate::crypto::{self, Sealer};

/// The purpose channel secrets are sealed for (their own derived key).
pub const PURPOSE: &str = "vole-kit/notification-channels";

/// The table products keep their channels in (`id`, `kind`, `config`).
pub const TABLE: &str = "notification_channels";

/// The message for a secret that can't be opened.
pub fn unreadable_message() -> &'static str {
    crate::i18n::t("notif.err_secret_unreadable")
}

fn sealer(key: &Key) -> Sealer {
    Sealer::new(key, PURPOSE)
}

/// Seals `config`'s secret values with `sealer`; values already sealed stay.
fn seal_with(sealer: &Sealer, schema: &ProviderSchema, config: &Value) -> Value {
    let mut out = config.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    let seal = |v: &mut String| {
        if !v.is_empty() && !crypto::is_sealed(v) {
            *v = sealer.seal(v);
        }
    };
    for field in schema.fields.iter().filter(|f| f.secret) {
        match obj.get_mut(field.key) {
            Some(Value::String(v)) => seal(v),
            Some(Value::Object(map)) => {
                for v in map.values_mut() {
                    if let Value::String(v) = v {
                        seal(v);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// `config` (a `kind` channel's) as it is stored: every non-empty secret
/// string sealed, key–value secrets (headers) value by value. Values that
/// are sealed already are kept. A kind this build doesn't know is returned
/// as it is (it can't be sent or edited either).
pub fn seal_config(key: &Key, kind: &str, config: &Value) -> Value {
    match super::provider(kind) {
        Some(schema) => seal_with(&sealer(key), schema, config),
        None => config.clone(),
    }
}

/// A stored config with its secrets opened.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    /// The config with plaintext secrets. Fields that couldn't be opened
    /// are left out.
    pub config: Value,
    /// Secret fields that couldn't be opened (another instance key): they
    /// have to be entered again.
    pub unreadable: Vec<&'static str>,
}

impl Opened {
    /// Whether every secret could be opened.
    pub fn is_readable(&self) -> bool {
        self.unreadable.is_empty()
    }

    /// One "enter it again" error per unreadable field, for the edit form.
    pub fn errors(&self) -> Vec<FieldError> {
        self.unreadable
            .iter()
            .map(|key| field_error(key, unreadable_message()))
            .collect()
    }

    /// Errors for unreadable fields a submitted form doesn't replace: left
    /// empty or masked (for key–value secrets: any masked value), and not
    /// cleared on purpose.
    pub fn reentry_errors(&self, submitted: &Value, clear: &[String]) -> Vec<FieldError> {
        self.unreadable
            .iter()
            .filter(|key| !clear.iter().any(|c| c == *key))
            .filter(|key| match submitted.get(**key) {
                None | Some(Value::Null) => true,
                Some(Value::String(s)) => s.trim().is_empty() || s == SECRET_MASK,
                Some(Value::Object(map)) => {
                    map.is_empty() || map.values().any(|v| v.as_str() == Some(SECRET_MASK))
                }
                Some(_) => false,
            })
            .map(|key| field_error(key, unreadable_message()))
            .collect()
    }
}

fn open_with(sealer: &Sealer, schema: &ProviderSchema, config: &Value) -> Opened {
    let mut out = config.clone();
    let mut unreadable = Vec::new();
    if let Some(obj) = out.as_object_mut() {
        let open = |v: &mut String| -> bool {
            if !crypto::is_sealed(v) {
                // Plaintext from before sealing (or never sealed).
                return true;
            }
            match sealer.open(v) {
                Ok(plain) => {
                    *v = plain;
                    true
                }
                Err(_) => false,
            }
        };
        for field in schema.fields.iter().filter(|f| f.secret) {
            let ok = match obj.get_mut(field.key) {
                Some(Value::String(v)) => open(v),
                Some(Value::Object(map)) => map.values_mut().all(|v| match v {
                    Value::String(v) => open(v),
                    _ => true,
                }),
                _ => true,
            };
            if !ok {
                obj.remove(field.key);
                unreadable.push(field.key);
            }
        }
    }
    Opened {
        config: out,
        unreadable,
    }
}

/// `config` (a `kind` channel's, as stored) with its secrets opened. A
/// kind this build doesn't know comes back unchanged.
pub fn open_config(key: &Key, kind: &str, config: &Value) -> Opened {
    match super::provider(kind) {
        Some(schema) => open_with(&sealer(key), schema, config),
        None => Opened {
            config: config.clone(),
            unreadable: Vec::new(),
        },
    }
}

/// The channel, as stored, ready to send: its config with the secrets
/// opened. Fails (with a message that is safe to show and log) when a
/// secret can't be opened or the config isn't JSON.
pub fn open_channel(
    key: &Key,
    channel: &NotificationChannel,
) -> Result<NotificationChannel, String> {
    let config: Value = serde_json::from_str(&channel.config)
        .map_err(|e| format!("invalid channel config: {e}"))?;
    let opened = open_config(key, &channel.kind, &config);
    if !opened.is_readable() {
        return Err(unreadable_message().to_string());
    }
    Ok(NotificationChannel {
        config: opened.config.to_string(),
        ..channel.clone()
    })
}

/// Whether a secret value of `config` is still sealed (the config wasn't
/// opened, or couldn't be). Sending refuses such a config.
pub fn has_sealed(schema: &ProviderSchema, config: &Value) -> bool {
    schema
        .fields
        .iter()
        .filter(|f| f.secret)
        .any(|field| match config.get(field.key) {
            Some(Value::String(v)) => crypto::is_sealed(v),
            Some(Value::Object(map)) => map
                .values()
                .any(|v| v.as_str().is_some_and(crypto::is_sealed)),
            _ => false,
        })
}

fn table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [TABLE],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

/// Rewrites every row of [`TABLE`] whose config `change` alters, in one
/// transaction. Returns how many rows changed.
fn rewrite_stored(
    conn: &Connection,
    change: impl Fn(&ProviderSchema, &Value) -> Value,
) -> rusqlite::Result<usize> {
    if !table_exists(conn)? {
        return Ok(0);
    }
    let tx = conn.unchecked_transaction()?;
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = tx.prepare(&format!("SELECT id, kind, config FROM {TABLE} ORDER BY id"))?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut changed = 0;
    for (id, kind, text) in rows {
        let (Some(schema), Ok(config)) = (super::provider(&kind), serde_json::from_str(&text))
        else {
            continue;
        };
        let new = change(schema, &config);
        if new != config {
            tx.execute(
                &format!("UPDATE {TABLE} SET config = ?2 WHERE id = ?1"),
                params![id, new.to_string()],
            )?;
            changed += 1;
        }
    }
    tx.commit()?;
    Ok(changed)
}

/// Seals the plaintext secrets of every stored channel ([`TABLE`]) in
/// place, and returns how many channels changed. Run it once at startup,
/// after the migrations: channels saved by a version without sealing are
/// sealed, channels sealed already (the [`crypto::PREFIX`]) stay as they
/// are, so a second run changes nothing. Rows of unknown kinds and configs
/// that aren't JSON are left alone. No table, nothing to do.
pub fn seal_stored(conn: &Connection, key: &Key) -> rusqlite::Result<usize> {
    let sealer = sealer(key);
    let changed = rewrite_stored(conn, |schema, config| seal_with(&sealer, schema, config))?;
    if changed > 0 {
        tracing::info!(
            channels = changed,
            "sealed the plaintext secrets of notification channels"
        );
    }
    Ok(changed)
}

/// The opposite of [`seal_stored`], for a copy of the database that has to
/// work without this instance's key (a backup to restore elsewhere): every
/// secret that opens is written back in plaintext. Secrets that don't open
/// stay sealed. Returns how many channels changed. Never run it on the live
/// database.
pub fn open_stored(conn: &Connection, key: &Key) -> rusqlite::Result<usize> {
    let sealer = sealer(key);
    rewrite_stored(conn, |schema, config| {
        let opened = open_with(&sealer, schema, config);
        if opened.is_readable() {
            return opened.config;
        }
        // Keep the unreadable fields as they were; open the rest.
        let mut out = opened.config;
        if let (Some(obj), Some(old)) = (out.as_object_mut(), config.as_object()) {
            for field in &opened.unreadable {
                if let Some(v) = old.get(*field) {
                    obj.insert((*field).to_string(), v.clone());
                }
            }
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(byte: u8) -> Key {
        Key::from(&[byte; 64])
    }

    fn webhook() -> Value {
        json!({"url": "https://hooks.example/abc?key=s3cret", "headers": {"Authorization": "Bearer t0ken", "X-Empty": ""},
            "body_template": "{\"text\": \"{{summary}}\"}"})
    }

    #[test]
    fn only_secret_fields_are_sealed() {
        let sealed = seal_config(&key(1), "webhook", &webhook());
        let text = sealed.to_string();
        assert!(
            !text.contains("s3cret") && !text.contains("t0ken"),
            "{text}"
        );
        assert!(crypto::is_sealed(sealed["url"].as_str().unwrap()));
        assert!(crypto::is_sealed(
            sealed["headers"]["Authorization"].as_str().unwrap()
        ));
        // Empty values stay empty; non-secret fields stay readable.
        assert_eq!(sealed["headers"]["X-Empty"], "");
        assert_eq!(sealed["body_template"], webhook()["body_template"]);
        let telegram = seal_config(
            &key(1),
            "telegram",
            &json!({"bot_token": "1:SECRET", "chat_id": "42", "silent": true}),
        );
        assert_eq!(
            (&telegram["chat_id"], &telegram["silent"]),
            (&json!("42"), &json!(true))
        );
        assert!(crypto::is_sealed(telegram["bot_token"].as_str().unwrap()));
    }

    #[test]
    fn sealing_twice_changes_nothing_and_opens_back() {
        let once = seal_config(&key(1), "webhook", &webhook());
        assert_eq!(seal_config(&key(1), "webhook", &once), once);
        let opened = open_config(&key(1), "webhook", &once);
        assert!(opened.is_readable());
        assert_eq!(opened.config, webhook());
        // Plaintext (stored before sealing) opens as it is.
        assert_eq!(
            open_config(&key(1), "webhook", &webhook()).config,
            webhook()
        );
    }

    #[test]
    fn another_key_leaves_the_secret_out_and_names_it() {
        let sealed = seal_config(&key(1), "webhook", &webhook());
        let opened = open_config(&key(2), "webhook", &sealed);
        assert_eq!(opened.unreadable, vec!["url", "headers"]);
        assert!(opened.config.get("url").is_none() && opened.config.get("headers").is_none());
        assert_eq!(opened.config["body_template"], webhook()["body_template"]);
        let errors = opened.errors();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].1, unreadable_message());
        // Sending refuses, without a crash and without sending anything.
        let channel = NotificationChannel {
            id: 1,
            name: "Ops".into(),
            kind: "webhook".into(),
            config: sealed.to_string(),
            active: true,
            is_default: false,
            created_at: String::new(),
        };
        assert_eq!(
            open_channel(&key(2), &channel).unwrap_err(),
            unreadable_message()
        );
        let ok = open_channel(&key(1), &channel).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&ok.config).unwrap(),
            webhook()
        );
    }

    #[test]
    fn reentry_is_required_until_the_field_is_typed_or_cleared() {
        let sealed = seal_config(&key(1), "webhook", &webhook());
        let opened = open_config(&key(2), "webhook", &sealed);
        let keys = |e: Vec<FieldError>| e.into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        assert_eq!(
            keys(opened.reentry_errors(
                &json!({"url": "", "headers": {"Authorization": SECRET_MASK}}),
                &[]
            )),
            vec!["url", "headers"]
        );
        assert_eq!(
            keys(opened.reentry_errors(
                &json!({"url": "https://new.example/", "headers": {"Authorization": "Bearer new"}}),
                &[]
            )),
            Vec::<String>::new()
        );
        assert_eq!(
            keys(
                opened.reentry_errors(&json!({"url": "https://new.example/"}), &["headers".into()])
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn unknown_kinds_are_left_alone() {
        let config = json!({"token": "x"});
        assert_eq!(seal_config(&key(1), "carrier-pigeon", &config), config);
        assert_eq!(
            open_config(&key(1), "carrier-pigeon", &config).config,
            config
        );
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE notification_channels (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                 kind TEXT NOT NULL, config TEXT NOT NULL DEFAULT '{}');",
        )
        .unwrap();
        conn
    }

    fn configs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT config FROM notification_channels ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn stored_plaintext_is_sealed_once() {
        let conn = db();
        let k = key(3);
        let already = seal_config(&k, "telegram", &json!({"bot_token": "1:B", "chat_id": "7"}));
        for (kind, config) in [
            ("webhook", webhook().to_string()),
            ("telegram", already.to_string()),
            ("discord", json!({"url": ""}).to_string()),
            ("carrier-pigeon", json!({"token": "x"}).to_string()),
            ("webhook", "not json".to_string()),
        ] {
            conn.execute(
                "INSERT INTO notification_channels (name, kind, config) VALUES ('c', ?1, ?2)",
                params![kind, config],
            )
            .unwrap();
        }
        let before = configs(&conn);
        assert_eq!(
            seal_stored(&conn, &k).unwrap(),
            1,
            "only the plaintext webhook"
        );
        let after = configs(&conn);
        assert!(!after[0].contains("s3cret") && !after[0].contains("t0ken"));
        assert_eq!(after[1..], before[1..], "the others are untouched");
        // Idempotent.
        assert_eq!(seal_stored(&conn, &k).unwrap(), 0);
        assert_eq!(configs(&conn), after);
        // And it opens back to what was stored.
        let stored: Value = serde_json::from_str(&after[0]).unwrap();
        assert_eq!(open_config(&k, "webhook", &stored).config, webhook());
    }

    #[test]
    fn stored_secrets_open_for_a_copy() {
        let conn = db();
        let k = key(4);
        let sealed = seal_config(&k, "webhook", &webhook());
        let foreign = seal_config(
            &key(5),
            "telegram",
            &json!({"bot_token": "1:B", "chat_id": "7"}),
        );
        for (kind, config) in [("webhook", &sealed), ("telegram", &foreign)] {
            conn.execute(
                "INSERT INTO notification_channels (name, kind, config) VALUES ('c', ?1, ?2)",
                params![kind, config.to_string()],
            )
            .unwrap();
        }
        assert_eq!(open_stored(&conn, &k).unwrap(), 1);
        let after = configs(&conn);
        assert_eq!(serde_json::from_str::<Value>(&after[0]).unwrap(), webhook());
        // What doesn't open stays sealed, unchanged.
        assert_eq!(serde_json::from_str::<Value>(&after[1]).unwrap(), foreign);
    }

    #[test]
    fn no_table_means_nothing_to_seal() {
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(seal_stored(&conn, &key(1)).unwrap(), 0);
    }
}
