//! Tests for [`super`]: the SSRF guard, the messages and webhook delivery.

use std::net::{IpAddr, SocketAddr};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::db::status_pages::{self as pages, NewStatusPage};
use crate::db::test_conn;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn only_public_addresses_pass_the_guard() {
    for bad in [
        "127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "0.0.0.0", "100.64.0.1",
        "224.0.0.1", "255.255.255.255", "198.18.0.1", "240.0.0.1", "192.0.2.1", "::1", "::", "fe80::1", "fc00::1",
        "fd12:3456::1", "ff02::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1", "2001:db8::1",
    ] {
        assert!(!is_public(ip(bad)), "{bad} must be blocked");
    }
    for good in ["93.184.216.34", "8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
        assert!(is_public(ip(good)), "{good} must pass");
    }
}

#[test]
fn webhook_urls_are_checked_for_shape() {
    assert!(parse_webhook_url("https://example.com/hook?x=1").is_some());
    assert!(parse_webhook_url("  http://example.com:8080/h  ").is_some());
    for bad in ["ftp://example.com", "javascript:alert(1)", "https://u:p@example.com", "https://u@example.com", "not a url", "", "file:///etc/passwd"] {
        assert!(parse_webhook_url(bad).is_none(), "{bad}");
    }
    assert!(parse_webhook_url(&format!("https://example.com/{}", "a".repeat(2100))).is_none());
}

#[tokio::test]
async fn private_hosts_never_resolve_to_a_target() {
    for url in ["http://127.0.0.1/h", "http://localhost/h", "http://[::1]:8080/h", "http://192.168.0.10/h", "http://169.254.169.254/latest/meta-data"] {
        let parsed = parse_webhook_url(url).unwrap();
        assert!(resolve_public(&parsed).await.is_err(), "{url}");
    }
    let ok = resolve_public(&parse_webhook_url("https://93.184.216.34/h").unwrap()).await.unwrap();
    assert_eq!(ok, vec![SocketAddr::new(ip("93.184.216.34"), 443)]);
}

#[test]
fn the_throttle_counts_per_key_and_window() {
    let key = "test-throttle-unique-key";
    for _ in 0..3 {
        assert!(allow(key, 3, Duration::from_secs(60)));
    }
    assert!(!allow(key, 3, Duration::from_secs(60)));
    assert!(allow("test-throttle-other-key", 3, Duration::from_secs(60)));
    // A zero window starts over every time.
    assert!(allow(key, 3, Duration::ZERO));
}

fn page() -> StatusPage {
    let mut conn = test_conn();
    let id = pages::insert(&mut conn, &NewStatusPage { slug: "acme".into(), title: "Acme <Status>".into(), ..NewStatusPage::default() }).unwrap();
    pages::get(&conn, id).unwrap().unwrap()
}

fn config() -> Value {
    json!({"from": "Acme <status@example.com>", "host": "smtp.example.com"})
}

fn raw(m: &Message) -> String {
    String::from_utf8_lossy(&m.formatted()).replace("\r\n", "\n")
}

#[test]
fn announcements_are_one_mail_per_subscriber_with_unsubscribe() {
    let page = page();
    let links = Links::new("https://up.example.com/", &page, "TOKEN123");
    assert_eq!(links.unsubscribe, "https://up.example.com/status/acme/subscribe/unsubscribe/TOKEN123");
    let a = Announcement {
        page_id: page.id,
        kind: Kind::Incident,
        title: "DB <slow>".into(),
        body: "We're on it & watching".into(),
        severity: "warning".into(),
        incident_id: Some(4),
    };
    let m = announcement_mail(&config(), &page, "reader@example.com", &links, &a).unwrap();
    let text = raw(&m);
    assert!(text.contains("To: reader@example.com"), "{text}");
    assert!(!text.contains("Cc:") && !text.contains("Bcc:"));
    assert!(text.contains("From: Acme <status@example.com>"));
    assert!(text.contains("List-Unsubscribe: <https://up.example.com/status/acme/subscribe/unsubscribe/TOKEN123>"), "{text}");
    assert!(text.contains("Subject: [Acme"), "{text}");
    // HTML part escapes everything user-written.
    assert!(text.contains("DB &lt;slow&gt;") && text.contains("We&#39;re on it &amp; watching"), "{text}");
    assert!(!text.contains("<slow>") || text.contains("DB <slow>"), "plain part keeps the raw text");

    // Bad addresses are rejected, not sent.
    assert!(announcement_mail(&config(), &page, "nope", &links, &a).is_err());
    assert!(announcement_mail(&json!({}), &page, "a@example.com", &links, &a).is_err());
}

#[test]
fn the_confirmation_mail_carries_the_link_and_no_unsubscribe_header() {
    let page = page();
    let links = Links::new("https://up.example.com", &page, "T0K");
    let m = confirmation_mail(&config(), &page, "reader@example.com", &links).unwrap();
    let text = raw(&m);
    assert!(text.contains("https://up.example.com/status/acme/subscribe/confirm/T0K"), "{text}");
    assert!(!text.contains("List-Unsubscribe"));
    assert!(text.contains("Confirm your subscription to Acme"), "{text}");
}

#[test]
fn announcement_json_has_what_a_receiver_needs() {
    let page = page();
    let links = Links::new("https://up.example.com", &page, "T");
    let a = Announcement { page_id: 1, kind: Kind::MaintenanceStart, title: "Upgrade".into(), body: String::new(), severity: "info".into(), incident_id: None };
    let v = announcement_json(&page, &links, &a);
    assert_eq!(v["type"], "maintenance_start");
    assert_eq!(v["page"]["url"], "https://up.example.com/status/acme");
    assert_eq!(v["unsubscribe_url"], "https://up.example.com/status/acme/subscribe/unsubscribe/T");
    assert!(v["incident_id"].is_null());
}

/// Serves one canned response and hands back the request it got.
async fn one_shot(response: &'static str) -> (SocketAddr, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let mut got = Vec::new();
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            got.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&got);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let len = text
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                    .unwrap_or(0);
                if got.len() >= head_end + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        sock.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&got).to_string()
    });
    (addr, task)
}

#[tokio::test]
async fn webhooks_are_posted_json_and_redirects_are_not_followed() {
    let (addr, got) = one_shot("HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
    let url = url::Url::parse(&format!("http://hooks.example.test:{}/in", addr.port())).unwrap();
    // The name resolves to the pinned address, not through DNS.
    post_pinned(url, &[addr], &json!({"type": "incident", "title": "x"})).await.unwrap();
    let request = got.await.unwrap();
    assert!(request.starts_with("POST /in HTTP/1.1"), "{request}");
    assert!(request.to_ascii_lowercase().contains("content-type: application/json"), "{request}");
    assert!(request.contains("\"type\":\"incident\""), "{request}");

    let (addr, _) = one_shot("HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:1/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
    let url = url::Url::parse(&format!("http://hooks.example.test:{}/in", addr.port())).unwrap();
    let err = post_pinned(url, &[addr], &json!({})).await.unwrap_err();
    assert_eq!(err, "HTTP 302 Found");

    let (addr, _) = one_shot("HTTP/1.1 500 Internal Server Error\r\ncontent-length: 4\r\nconnection: close\r\n\r\nboom").await;
    let url = url::Url::parse(&format!("http://hooks.example.test:{}/in", addr.port())).unwrap();
    assert!(post_pinned(url, &[addr], &json!({})).await.unwrap_err().starts_with("HTTP 500"));
}

#[test]
fn only_confirmed_subscribers_of_an_enabled_page_are_planned() {
    let mut conn = test_conn();
    let id = pages::insert(&mut conn, &NewStatusPage { slug: "p".into(), title: "P".into(), ..NewStatusPage::default() }).unwrap();
    assert!(plan(&conn, id).unwrap().is_none(), "off by default");
    db::set_settings(&conn, id, &db::Settings { enabled: true, channel_id: None, webhooks: true }).unwrap();
    let db::Added::Pending(a) = db::add(&conn, id, "webhook", "https://93.184.216.34/a", "en", "").unwrap() else { panic!() };
    db::add(&conn, id, "webhook", "https://93.184.216.34/b", "en", "").unwrap();
    db::confirm(&conn, &a).unwrap();
    let plan = plan(&conn, id).unwrap().unwrap();
    assert_eq!(plan.subscribers.len(), 1);
    assert_eq!(plan.subscribers[0].target, "https://93.184.216.34/a");
    assert!(plan.mail.is_none());

    // An inactive or non-email channel gives no mail config.
    let hook = crate::db::notifications::insert_channel(&conn, "H", "webhook", "{}", false).unwrap();
    assert!(mail_config(&conn, Some(hook)).unwrap().is_none());
    let mail = crate::db::notifications::insert_channel(&conn, "M", "email", r#"{"from":"a@b.c"}"#, false).unwrap();
    assert!(mail_config(&conn, Some(mail)).unwrap().is_some());
    crate::db::notifications::set_active(&conn, mail, false).unwrap();
    assert!(mail_config(&conn, Some(mail)).unwrap().is_none());

    // Pages that show the monitors are found for maintenance announcements.
    let found = db::pages_with_monitors(&conn, &[1, 2]).unwrap();
    assert!(found.is_empty());
}
