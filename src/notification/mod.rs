use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::db::models::{ChannelKind, HeartbeatStatus, NotificationChannel};

pub async fn notify_all(
    client: &reqwest::Client,
    channels: &[NotificationChannel],
    monitor_name: &str,
    target: &str,
    status: HeartbeatStatus,
    message: Option<&str>,
) {
    for channel in channels {
        if let Err(err) = send(client, channel, monitor_name, target, status, message).await {
            tracing::warn!(channel = %channel.name, %err, "failed to send notification");
        }
    }
}

async fn send(
    client: &reqwest::Client,
    channel: &NotificationChannel,
    monitor_name: &str,
    target: &str,
    status: HeartbeatStatus,
    message: Option<&str>,
) -> Result<(), String> {
    let config: serde_json::Value = serde_json::from_str(&channel.config).map_err(|e| e.to_string())?;
    let text = match message {
        Some(m) => format!("{monitor_name} is now {status} ({m})"),
        None => format!("{monitor_name} is now {status}"),
    };

    let (url, body) = match channel.kind {
        ChannelKind::Discord => (
            config_str(&config, "url")?,
            json!({ "embeds": [discord_embed(monitor_name, target, status, message)] }),
        ),
        ChannelKind::Slack => (config_str(&config, "url")?, json!({ "text": text })),
        ChannelKind::Webhook => (
            config_str(&config, "url")?,
            json!({ "monitor": monitor_name, "status": status.as_str(), "message": message }),
        ),
        ChannelKind::Telegram => {
            let token = config_str(&config, "bot_token")?;
            let chat_id = config_str(&config, "chat_id")?;
            (
                format!("https://api.telegram.org/bot{token}/sendMessage"),
                json!({ "chat_id": chat_id, "text": text }),
            )
        }
    };

    client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Builds a Discord rich embed: color-coded by status, with the monitor as a
/// clickable title (when the target is an http(s) URL), a status field, and
/// the check's error message (if any) as the description.
fn discord_embed(monitor_name: &str, target: &str, status: HeartbeatStatus, message: Option<&str>) -> serde_json::Value {
    let (color, status_label) = match status {
        HeartbeatStatus::Up => (0x0ca30c, "\u{1f7e2} Up"),
        HeartbeatStatus::Down => (0xd03b3b, "\u{1f534} Down"),
        HeartbeatStatus::Pending => (0xfab219, "\u{1f7e1} Pending"),
    };

    let mut embed = json!({
        "title": monitor_name,
        "color": color,
        "fields": [
            { "name": "Status", "value": status_label, "inline": true },
            { "name": "Target", "value": target, "inline": true },
        ],
        "footer": { "text": "Vigilant Vole" },
        "timestamp": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default(),
    });

    if target.starts_with("http://") || target.starts_with("https://") {
        embed["url"] = json!(target);
    }
    if let Some(m) = message {
        embed["description"] = json!(m);
    }

    embed
}

fn config_str(config: &serde_json::Value, key: &str) -> Result<String, String> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing '{key}' in channel config"))
}
