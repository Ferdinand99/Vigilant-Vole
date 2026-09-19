use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorType {
    Http,
    Tcp,
    Ping,
}

impl MonitorType {
    pub fn as_str(self) -> &'static str {
        match self {
            MonitorType::Http => "http",
            MonitorType::Tcp => "tcp",
            MonitorType::Ping => "ping",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "http" => Some(MonitorType::Http),
            "tcp" => Some(MonitorType::Tcp),
            "ping" => Some(MonitorType::Ping),
            _ => None,
        }
    }
}

impl fmt::Display for MonitorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatStatus {
    Up,
    Down,
    Pending,
}

impl HeartbeatStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HeartbeatStatus::Up => "up",
            HeartbeatStatus::Down => "down",
            HeartbeatStatus::Pending => "pending",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "up" => Some(HeartbeatStatus::Up),
            "down" => Some(HeartbeatStatus::Down),
            "pending" => Some(HeartbeatStatus::Pending),
            _ => None,
        }
    }
}

impl fmt::Display for HeartbeatStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub monitor_type: MonitorType,
    pub target: String,
    pub interval_seconds: i64,
    pub timeout_seconds: i64,
    pub retries: i64,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct NewMonitor {
    pub name: String,
    pub monitor_type: MonitorType,
    pub target: String,
    pub interval_seconds: i64,
    pub timeout_seconds: i64,
    pub retries: i64,
}

#[derive(Debug, Clone)]
pub struct MonitorWithStatus {
    pub monitor: Monitor,
    pub status: Option<HeartbeatStatus>,
    pub response_time_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Discord,
    Slack,
    Telegram,
    Webhook,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Discord => "discord",
            ChannelKind::Slack => "slack",
            ChannelKind::Telegram => "telegram",
            ChannelKind::Webhook => "webhook",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "discord" => Some(ChannelKind::Discord),
            "slack" => Some(ChannelKind::Slack),
            "telegram" => Some(ChannelKind::Telegram),
            "webhook" => Some(ChannelKind::Webhook),
            _ => None,
        }
    }
}

impl fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct NotificationChannel {
    pub id: i64,
    pub name: String,
    pub kind: ChannelKind,
    pub config: String,
    pub active: bool,
}
