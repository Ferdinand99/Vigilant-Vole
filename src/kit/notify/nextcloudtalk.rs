//! Nextcloud Talk: a message from a Talk bot, signed with the bot's shared
//! secret (HMAC-SHA256 over a random value and the message).

use rand::RngExt;
use serde_json::{Value, json};

use super::http::{OutRequest, flag, hmac_sha256, join, path_segment, req};
use super::message::{Severity, markdown_details, severity, title};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::kit::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "nextcloudtalk",
    label: "Nextcloud Talk",
    icon: "megaphone",
    category: Category::Chat,
    description: "Messages in a Nextcloud Talk conversation",
    docs_url: "https://nextcloud-talk.readthedocs.io/en/latest/bots/",
    fields: &[
        Field::url("host", "Nextcloud URL")
            .required()
            .placeholder("https://cloud.example.com…"),
        Field::text("conversation_token", "Conversation token")
            .required()
            .placeholder("abcd1234…")
            .help("The last part of the conversation’s link."),
        Field::secret("bot_secret", "Bot secret")
            .required()
            .help("Set when you install the bot: occ talk:bot:install … (40 to 128 characters)."),
        Field::boolean("silent_up", "No notification when up")
            .help("Post recoveries without notifying the participants."),
    ],
    transport: Transport::Http(build),
    check: None,
};

/// Header value for `X-Nextcloud-Talk-Bot-Signature`.
fn sign(secret: &str, random: &str, message: &str) -> String {
    hmac_sha256(secret.as_bytes(), format!("{random}{message}").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_hex() -> String {
    let mut rng = rand::rng();
    (0..32)
        .map(|_| format!("{:02x}", rng.random::<u8>()))
        .collect()
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    build_with(config, event, &random_hex())
}

fn build_with(
    config: &Value,
    event: &NotificationEvent,
    random: &str,
) -> Result<Vec<OutRequest>, String> {
    let host = req(config, "host")?;
    let token = req(config, "conversation_token")?;
    let secret = req(config, "bot_secret")?;
    let message = format!("**{}**\n{}", title(event), markdown_details(event));
    let silent = flag(config, "silent_up") && severity(event.kind) == Severity::Ok;
    let signature = sign(secret, random, &message);
    let url = join(
        host,
        &format!(
            "/ocs/v2.php/apps/spreed/api/v1/bot/{}/message",
            path_segment(token)
        ),
    );
    Ok(vec![
        OutRequest::post_json(url, json!({"message": message, "silent": silent}))
            .header("OCS-APIRequest", "true")
            .header("X-Nextcloud-Talk-Bot-Random", random)
            .header("X-Nextcloud-Talk-Bot-Signature", signature),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::notify::alert::EventKind;
    use crate::kit::notify::tests::event;

    fn config() -> Value {
        json!({"host": "https://cloud.example.com/", "conversation_token": "abcd1234", "bot_secret": "s3cret-s3cret-s3cret-s3cret-s3cret-s3cret"})
    }

    #[test]
    fn signs_the_random_value_and_the_message() {
        let r = &build_with(&config(), &event(EventKind::Down), "00ff").unwrap()[0];
        assert_eq!(
            r.url,
            "https://cloud.example.com/ocs/v2.php/apps/spreed/api/v1/bot/abcd1234/message"
        );
        assert_eq!(r.header_value("OCS-APIRequest"), Some("true"));
        assert_eq!(r.header_value("X-Nextcloud-Talk-Bot-Random"), Some("00ff"));
        let message = r.json().unwrap()["message"].as_str().unwrap();
        assert!(message.contains("Payments API is down"));
        // The signature is HMAC-SHA256(secret, random + message), hex.
        assert_eq!(
            r.header_value("X-Nextcloud-Talk-Bot-Signature").unwrap(),
            sign("s3cret-s3cret-s3cret-s3cret-s3cret-s3cret", "00ff", message)
        );
        assert_eq!(
            r.header_value("X-Nextcloud-Talk-Bot-Signature")
                .unwrap()
                .len(),
            64
        );
    }

    #[test]
    fn random_values_differ_and_recoveries_can_be_silent() {
        let (a, b) = (random_hex(), random_hex());
        assert!(a.len() == 64 && a != b);
        let mut c = config();
        c["silent_up"] = json!(true);
        assert_eq!(
            build(&c, &event(EventKind::Up)).unwrap()[0].json().unwrap()["silent"],
            true
        );
        assert_eq!(
            build(&c, &event(EventKind::Down)).unwrap()[0]
                .json()
                .unwrap()["silent"],
            false
        );
        assert_eq!(
            build(&config(), &event(EventKind::Up)).unwrap()[0]
                .json()
                .unwrap()["silent"],
            false
        );
    }

    #[test]
    fn hmac_matches_a_known_vector() {
        // RFC 4231 test case 2.
        let mac: String = hmac_sha256(b"Jefe", b"what do ya want for nothing?")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            mac,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
