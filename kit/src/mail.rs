//! The instance's own mail settings: one SMTP account, kept in the
//! `settings` table and edited on the General page, for the mail the kit
//! itself sends (the one-time sign-in codes of
//! [`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes)).
//! Alert channels keep their own SMTP settings; this is not one of them.
//!
//! The password is sealed under the instance key
//! ([`crate::crypto`], purpose [`PURPOSE`]); everything else is stored as
//! typed. Delivery is [`notify::email::deliver`](crate::notify::email::deliver),
//! so the same address rules (no link-local or metadata addresses) and the
//! same timeouts apply. Nothing here logs a message's text.
//!
//! Rules that keep the mail safe:
//! * A connection without encryption (`none`) with a user name or password
//!   would send the credentials, and every emailed code, in clear text: it is
//!   refused unless the server is on this machine (loopback), when the
//!   settings are saved and again when a message is sent.
//! * A line break in the subject, the sender or the recipient cannot add a
//!   header: [`build`] drops control characters from the subject and refuses
//!   them anywhere else.
//! * Email addresses are taken as typed and are **not verified**: whoever may
//!   set a user's address (an administrator, with their password) decides
//!   where a sign-in code can go. A mistyped address sends the code to a
//!   stranger; the admin ticks "Also send by email" for each code and sees
//!   the address next to it.

use axum_extra::extract::cookie::Key;
use lettre::message::{Mailbox, header::ContentType};
use lettre::{Address, Message};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::db::settings as db_settings;
use crate::http::ApiResult;
use crate::state::Core;

/// What the password is sealed for.
pub const PURPOSE: &str = "vole-kit/instance-smtp";

/// The longest host name accepted.
const MAX_HOST_LEN: usize = 253;

/// The security modes of the connection.
pub const SECURITIES: [&str; 3] = ["starttls", "tls", "none"];

/// The stored SMTP settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Smtp {
    pub host: String,
    /// "" for the default of the security mode.
    pub port: String,
    /// One of [`SECURITIES`].
    pub security: String,
    pub username: String,
    /// The password as stored (sealed), "" for none.
    pub password: String,
    /// The sender, `alerts@example.com` or `Name <alerts@example.com>`.
    pub from: String,
}

impl Default for Smtp {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: String::new(),
            security: "starttls".into(),
            username: String::new(),
            password: String::new(),
            from: String::new(),
        }
    }
}

impl Smtp {
    /// The stored settings (default ones while none are saved).
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let get = |key| db_settings::get(conn, key).map(Option::unwrap_or_default);
        let security = get(db_settings::SMTP_SECURITY)?;
        Ok(Self {
            host: get(db_settings::SMTP_HOST)?,
            port: get(db_settings::SMTP_PORT)?,
            security: if SECURITIES.contains(&security.as_str()) {
                security
            } else {
                "starttls".into()
            },
            username: get(db_settings::SMTP_USERNAME)?,
            password: get(db_settings::SMTP_PASSWORD)?,
            from: get(db_settings::SMTP_FROM)?,
        })
    }

    /// Saves the settings; blank values are removed.
    pub fn store(&self, conn: &Connection) -> rusqlite::Result<()> {
        db_settings::set_opt(conn, db_settings::SMTP_HOST, Some(&self.host))?;
        db_settings::set_opt(conn, db_settings::SMTP_PORT, Some(&self.port))?;
        let security = (self.security != "starttls").then_some(self.security.as_str());
        db_settings::set_opt(conn, db_settings::SMTP_SECURITY, security)?;
        db_settings::set_opt(conn, db_settings::SMTP_USERNAME, Some(&self.username))?;
        db_settings::set_opt(conn, db_settings::SMTP_PASSWORD, Some(&self.password))?;
        db_settings::set_opt(conn, db_settings::SMTP_FROM, Some(&self.from))?;
        Ok(())
    }

    /// Whether mail can be sent: a server and a sender are set.
    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty() && !self.from.trim().is_empty()
    }

    /// Whether a password is saved.
    pub fn has_password(&self) -> bool {
        !self.password.is_empty()
    }

    /// Whether the saved password can be opened with this instance key.
    pub fn password_readable(&self, key: &Key) -> bool {
        self.password.is_empty() || crate::crypto::open(key, PURPOSE, &self.password).is_ok()
    }

    /// The settings as the SMTP channel reads them, with the password open.
    fn channel_config(&self, key: &Key) -> Result<Value, String> {
        let password = if self.password.is_empty() {
            String::new()
        } else {
            crate::crypto::open(key, PURPOSE, &self.password).map_err(|e| e.to_string())?
        };
        Ok(json!({
            "host": self.host,
            "port": self.port,
            "security": self.security,
            "username": self.username,
            "password": password,
        }))
    }
}

/// The saved settings, when mail can be sent and the feature is on.
pub async fn load(core: &Core) -> ApiResult<Option<Smtp>> {
    if !core.cfg.features.sign_in_codes {
        return Ok(None);
    }
    let smtp = core.with_conn(|c| Smtp::load(c)).await?;
    Ok(smtp.is_configured().then_some(smtp))
}

/// What the General page's form says.
#[derive(Debug, Default)]
pub struct Form {
    pub host: String,
    pub port: String,
    pub security: String,
    pub username: String,
    /// A new password; "" keeps the saved one.
    pub password: String,
    /// Remove the saved password.
    pub clear_password: bool,
    pub from: String,
}

/// What is wrong with a [`Form`]: the translation key of each message.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Problems {
    pub host: Option<&'static str>,
    pub port: Option<&'static str>,
    pub username: Option<&'static str>,
    pub from: Option<&'static str>,
}

impl Problems {
    pub fn any(&self) -> bool {
        self.host.is_some() || self.port.is_some() || self.username.is_some() || self.from.is_some()
    }
}

/// Whether `host` is this machine: `localhost` or a loopback address.
fn is_loopback(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether the settings would send a user name or password without
/// encryption to a server that is not on this machine.
fn clear_text_credentials(smtp: &Smtp) -> bool {
    smtp.security == "none"
        && (!smtp.username.is_empty() || !smtp.password.is_empty())
        && !is_loopback(&smtp.host)
}

/// Checks the form against the `saved` settings and returns what to store.
/// A form with nothing filled in switches mail off (everything removed); a
/// caller that wants a field left as it is passes the saved value in it.
pub fn parse(form: &Form, saved: &Smtp, key: &Key) -> Result<Smtp, Problems> {
    let host = form.host.trim();
    let from = form.from.trim();
    let port = form.port.trim();
    let username = form.username.trim();
    let keep_password = !form.clear_password && form.password.is_empty();
    let password_kept = keep_password && saved.has_password();
    let nothing = host.is_empty()
        && from.is_empty()
        && port.is_empty()
        && username.is_empty()
        && form.password.is_empty();
    if nothing {
        return Ok(Smtp {
            security: "starttls".into(),
            ..Smtp::default()
        });
    }
    let mut problems = Problems::default();
    let host_ok = !host.is_empty()
        && host.len() <= MAX_HOST_LEN
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._:[]".contains(&b));
    if !host_ok {
        problems.host = Some("notif.email.host.required");
    }
    if from.is_empty() {
        problems.from = Some("notif.email.from.required");
    } else if from.chars().any(char::is_control) || from.parse::<Mailbox>().is_err() {
        problems.from = Some("notif.err_email_from");
    }
    if !port.is_empty() && !port.parse::<u16>().is_ok_and(|p| p >= 1) {
        problems.port = Some("notif.err_port");
    }
    if !username.is_empty() && !password_kept && form.password.is_empty() {
        problems.username = Some("notif.err_email_password");
    }
    let security = if SECURITIES.contains(&form.security.as_str()) {
        form.security.clone()
    } else {
        "starttls".into()
    };
    let has_credentials = !username.is_empty() || !form.password.is_empty() || password_kept;
    if security == "none" && has_credentials && !is_loopback(host) {
        problems.username = Some("sign_in_code.err_smtp_plain");
    }
    if problems.any() {
        return Err(problems);
    }
    let password = if !form.password.is_empty() {
        crate::crypto::seal(key, PURPOSE, &form.password)
    } else if password_kept {
        saved.password.clone()
    } else {
        String::new()
    };
    Ok(Smtp {
        host: host.into(),
        port: port.into(),
        security,
        username: username.into(),
        password,
        from: from.into(),
    })
}

/// Whether `address` is a plain email address (`ada@example.com`).
pub fn is_address(address: &str) -> bool {
    address.len() <= crate::db::sign_in_codes::MAX_EMAIL_LEN
        && !address.contains(['<', '>', ',', ';', ' '])
        && !address.chars().any(char::is_control)
        && address.parse::<Address>().is_ok()
}

/// Builds a plain-text message. The subject loses its control characters
/// (a line break could start another header); a sender or recipient with one
/// is refused. The error is a short description and never holds the text.
pub fn build(from: &str, to: &str, subject: &str, body: &str) -> Result<Message, String> {
    if from.chars().any(char::is_control) {
        return Err("the sender address is not valid".into());
    }
    if to.chars().any(char::is_control) {
        return Err("the recipient address is not valid".into());
    }
    let from: Mailbox = from
        .parse()
        .map_err(|_| "the sender address is not valid".to_string())?;
    let to: Mailbox = to
        .parse()
        .map_err(|_| "the recipient address is not valid".to_string())?;
    let subject: String = subject.chars().filter(|c| !c.is_control()).collect();
    Message::builder()
        .from(from)
        .to(to)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|e| format!("the message could not be built: {e}"))
}

/// Sends a plain-text message through the instance's SMTP settings. The
/// error is a short description for the admin; it never holds the message.
pub async fn send(core: &Core, to: &str, subject: &str, body: &str) -> Result<(), String> {
    let smtp = core
        .with_conn(|c| Smtp::load(c))
        .await
        .map_err(|_| "the mail settings could not be read".to_string())?;
    if !smtp.is_configured() {
        return Err("no mail server is set up".into());
    }
    if clear_text_credentials(&smtp) {
        return Err("a user name and password would go over an unencrypted connection".into());
    }
    let config = smtp.channel_config(&core.session_key)?;
    let message = build(&smtp.from, to, subject, body)?;
    crate::notify::email::deliver(&config, message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::generate()
    }

    fn form() -> Form {
        Form {
            host: "smtp.example.com".into(),
            security: "starttls".into(),
            from: "Hedgerow <noreply@example.com>".into(),
            ..Form::default()
        }
    }

    #[test]
    fn an_empty_form_switches_mail_off() {
        let key = key();
        let saved = parse(
            &Form {
                password: "pw".into(),
                ..form()
            },
            &Smtp::default(),
            &key,
        )
        .unwrap();
        assert!(saved.is_configured());
        let off = parse(&Form::default(), &saved, &key).unwrap();
        assert!(!off.is_configured());
        assert!(!off.has_password());
    }

    #[test]
    fn the_password_is_sealed_and_kept_until_changed_or_cleared() {
        let key = key();
        let first = parse(
            &Form {
                username: "mailer".into(),
                password: "s3cret-pw".into(),
                ..form()
            },
            &Smtp::default(),
            &key,
        )
        .unwrap();
        assert!(first.password.starts_with(crate::crypto::PREFIX));
        assert!(!first.password.contains("s3cret-pw"));
        assert!(first.password_readable(&key));
        assert!(!first.password_readable(&Key::generate()));
        assert_eq!(first.channel_config(&key).unwrap()["password"], "s3cret-pw");
        // Left blank: the saved one stays.
        let kept = parse(
            &Form {
                username: "mailer".into(),
                ..form()
            },
            &first,
            &key,
        )
        .unwrap();
        assert_eq!(kept.password, first.password);
        // Typed: replaced.
        let changed = parse(
            &Form {
                username: "mailer".into(),
                password: "other-pw".into(),
                ..form()
            },
            &first,
            &key,
        )
        .unwrap();
        assert_ne!(changed.password, first.password);
        assert_eq!(
            changed.channel_config(&key).unwrap()["password"],
            "other-pw"
        );
        // Cleared (a username then needs one again).
        let cleared = parse(
            &Form {
                clear_password: true,
                ..form()
            },
            &first,
            &key,
        )
        .unwrap();
        assert!(!cleared.has_password());
        let problems = parse(
            &Form {
                username: "mailer".into(),
                clear_password: true,
                ..form()
            },
            &first,
            &key,
        )
        .unwrap_err();
        assert_eq!(problems.username, Some("notif.err_email_password"));
    }

    #[test]
    fn credentials_do_not_go_over_an_unencrypted_connection_unless_it_is_local() {
        let key = key();
        let with = |host: &str, security: &str, username: &str| {
            parse(
                &Form {
                    host: host.into(),
                    security: security.into(),
                    username: username.into(),
                    password: if username.is_empty() { "" } else { "pw" }.into(),
                    ..form()
                },
                &Smtp::default(),
                &key,
            )
        };
        let refused = with("mail.example.com", "none", "mailer").unwrap_err();
        assert_eq!(refused.username, Some("sign_in_code.err_smtp_plain"));
        assert!(
            with("mail.example.com", "none", "").is_ok(),
            "no credentials"
        );
        assert!(with("mail.example.com", "starttls", "mailer").is_ok());
        assert!(with("mail.example.com", "tls", "mailer").is_ok());
        for local in [
            "127.0.0.1",
            "localhost",
            "LocalHost",
            "[::1]",
            "::1",
            "127.9.9.9",
        ] {
            assert!(with(local, "none", "mailer").is_ok(), "{local}");
        }
        for remote in ["10.0.0.5", "localhost.example.com", "127.0.0.1.example.com"] {
            assert!(with(remote, "none", "mailer").is_err(), "{remote}");
        }
        // A saved password counts as credentials, too.
        let saved = with("mail.example.com", "starttls", "mailer").unwrap();
        let kept = parse(
            &Form {
                host: "mail.example.com".into(),
                security: "none".into(),
                ..form()
            },
            &saved,
            &key,
        );
        assert!(kept.is_err(), "the saved password would go in clear text");
        // And a stored setting that is already like this is not used.
        assert!(clear_text_credentials(&Smtp {
            host: "mail.example.com".into(),
            security: "none".into(),
            username: "u".into(),
            ..Smtp::default()
        }));
    }

    #[test]
    fn a_line_break_cannot_start_another_header() {
        let text = |m: Message| String::from_utf8_lossy(&m.formatted()).into_owned();
        let headers = |mail: &str| mail.split("\r\n\r\n").next().unwrap().to_string();
        // The subject: the break goes, the rest stays one line of the subject.
        for attack in [
            "Hello\r\nBcc: evil@example.com",
            "Hello\nBcc: evil@example.com",
            "Hello\rBcc: evil@example.com",
            "Hello\r\n\r\nBcc: evil@example.com",
            "Hello\u{85}Bcc: evil@example.com",
            "Hello\u{2028}Bcc: evil@example.com",
        ] {
            let mail = text(
                build(
                    "Vole <noreply@example.com>",
                    "ada@example.com",
                    attack,
                    "body",
                )
                .unwrap(),
            );
            let head = headers(&mail);
            assert!(
                !head.lines().any(|l| l.to_lowercase().starts_with("bcc:")),
                "{attack:?}: {head}"
            );
            assert_eq!(
                head.lines().filter(|l| l.starts_with("Subject:")).count(),
                1,
                "{attack:?}: {head}"
            );
        }
        // The sender (a display name) and the recipient: refused.
        for from in [
            "Evil\r\nBcc: evil@example.com <noreply@example.com>",
            "\"Evil\r\nBcc: evil@example.com\" <noreply@example.com>",
            "noreply@example.com\r\nBcc: evil@example.com",
        ] {
            assert!(
                build(from, "ada@example.com", "s", "b").is_err(),
                "{from:?}"
            );
        }
        for to in [
            "ada@example.com\r\nBcc: evil@example.com",
            "ada@example.com\nBcc: evil@example.com",
        ] {
            assert!(
                build("noreply@example.com", to, "s", "b").is_err(),
                "{to:?}"
            );
            assert!(!is_address(to), "{to:?}");
        }
        // The same checks on the form.
        let bad = parse(
            &Form {
                from: "Evil\r\nBcc: evil@example.com <noreply@example.com>".into(),
                ..form()
            },
            &Smtp::default(),
            &key(),
        )
        .unwrap_err();
        assert_eq!(bad.from, Some("notif.err_email_from"));
    }

    #[test]
    fn problems_name_the_field() {
        let key = key();
        let bad = parse(
            &Form {
                host: "smtp example".into(),
                port: "70000".into(),
                from: "not an address".into(),
                ..Form::default()
            },
            &Smtp::default(),
            &key,
        )
        .unwrap_err();
        assert_eq!(bad.host, Some("notif.email.host.required"));
        assert_eq!(bad.port, Some("notif.err_port"));
        assert_eq!(bad.from, Some("notif.err_email_from"));
        let missing = parse(
            &Form {
                host: "smtp.example.com".into(),
                ..Form::default()
            },
            &Smtp::default(),
            &key,
        )
        .unwrap_err();
        assert_eq!(missing.from, Some("notif.email.from.required"));
        assert!(
            parse(
                &Form {
                    port: "0".into(),
                    ..form()
                },
                &Smtp::default(),
                &key
            )
            .is_err()
        );
    }

    #[test]
    fn settings_round_trip_through_the_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::migrate::CORE_TABLES.1)
            .unwrap();
        assert_eq!(Smtp::load(&conn).unwrap(), Smtp::default());
        let key = key();
        let smtp = parse(
            &Form {
                host: "127.0.0.1".into(),
                port: "2525".into(),
                security: "none".into(),
                username: "u".into(),
                password: "p".into(),
                ..form()
            },
            &Smtp::default(),
            &key,
        )
        .unwrap();
        smtp.store(&conn).unwrap();
        assert_eq!(Smtp::load(&conn).unwrap(), smtp);
        Smtp::default().store(&conn).unwrap();
        assert_eq!(Smtp::load(&conn).unwrap(), Smtp::default());
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM settings WHERE key LIKE 'smtp_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn addresses_are_plain() {
        assert!(is_address("ada@example.com"));
        assert!(!is_address("Ada <ada@example.com>"));
        assert!(!is_address("ada@example.com, bob@example.com"));
        assert!(!is_address("ada"));
        assert!(!is_address(&format!("{}@example.com", "a".repeat(250))));
    }
}
