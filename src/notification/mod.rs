use serde_json::json;

use crate::db::models::{ChannelKind, HeartbeatStatus, NotificationChannel};

pub async fn notify_all(
    client: &reqwest::Client,
    channels: &[NotificationChannel],
    monitor_name: &str,
    status: HeartbeatStatus,
    message: Option<&str>,
) {
    for channel in channels {
        if let Err(err) = send(client, channel, monitor_name, status, message).await {
            tracing::warn!(channel = %channel.name, %err, "failed to send notification");
        }
    }
}

async fn send(
    client: &reqwest::Client,
    channel: &NotificationChannel,
    monitor_name: &str,
    status: HeartbeatStatus,
    message: Option<&str>,
) -> Result<(), String> {
    let config: serde_json::Value = serde_json::from_str(&channel.config).map_err(|e| e.to_string())?;
    let text = match message {
        Some(m) => format!("{monitor_name} is now {status} ({m})"),
        None => format!("{monitor_name} is now {status}"),
    };

    let (url, body) = match channel.kind {
        ChannelKind::Discord => (config_str(&config, "url")?, json!({ "content": text })),
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

fn config_str(config: &serde_json::Value, key: &str) -> Result<String, String> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing '{key}' in channel config"))
}
