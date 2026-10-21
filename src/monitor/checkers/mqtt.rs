//! MQTT checker (3.1.1): `target` = `[mqtt|mqtts]://host[:port][/topic]`.
//! The topic is everything after the host and port, as written (wildcards
//! `+` and `#` and a literal `?` included; `%XX` is decoded, so `%25` is `%`).
//!
//! Connects with the monitor's user name and password (if any) and waits for
//! `CONNACK`. With a topic in the path it also subscribes (QoS 0); with a
//! keyword set, the check waits for a message on that topic whose payload
//! contains the keyword (retained messages arrive right after subscribing),
//! or, with "invert", doesn't contain it. Response time = connect + login
//! (+ subscribe and message).

use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::conn::{self, Io};
use super::endpoint;
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const MAX_PACKET: usize = 1024 * 1024;
const KEEP_ALIVE_SECS: u16 = 30;

type Stream = Box<dyn Io>;

fn put_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![first];
    let mut len = body.len();
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

fn connect_packet(client_id: &str, user: Option<&str>, password: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    put_string(&mut body, "MQTT");
    body.push(4); // protocol level 3.1.1
    let mut flags = 0x02; // clean session
    if user.is_some() {
        flags |= 0x80;
        if password.is_some() {
            flags |= 0x40;
        }
    }
    body.push(flags);
    body.extend_from_slice(&KEEP_ALIVE_SECS.to_be_bytes());
    put_string(&mut body, client_id);
    if let Some(user) = user {
        put_string(&mut body, user);
        if let Some(password) = password {
            put_string(&mut body, password);
        }
    }
    packet(0x10, &body)
}

fn subscribe_packet(topic: &str) -> Vec<u8> {
    let mut body = 1u16.to_be_bytes().to_vec();
    put_string(&mut body, topic);
    body.push(0); // QoS 0
    packet(0x82, &body)
}

/// A packet read from the broker.
struct Incoming {
    kind: u8,
    body: Vec<u8>,
}

async fn read_packet(s: &mut Stream) -> Result<Incoming, String> {
    let first = s.read_u8().await.map_err(io_err)?;
    let (mut len, mut shift) = (0usize, 0);
    loop {
        let byte = s.read_u8().await.map_err(io_err)?;
        len |= usize::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 21 {
            return Err("Not an MQTT broker (bad packet length)".into());
        }
    }
    if len > MAX_PACKET {
        return Err("A message from the broker is too large".into());
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.map_err(io_err)?;
    Ok(Incoming {
        kind: first >> 4,
        body,
    })
}

fn io_err(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        "Connection closed by the broker".into()
    } else {
        util::tidy_io_error(&e)
    }
}

fn connack_error(code: u8) -> String {
    match code {
        1 => "The broker doesn't speak MQTT 3.1.1".into(),
        2 => "The broker rejected the client identifier".into(),
        3 => "The broker is unavailable".into(),
        4 => "Authentication failed: wrong user name or password".into(),
        5 => "Not authorized: the broker refused this client".into(),
        other => format!("The broker refused the connection (code {other})"),
    }
}

/// The topic of an MQTT target: the text after the host and port. Read from
/// the raw target, because a URL parser takes `#` (the multi-level wildcard)
/// for a fragment and `?` for a query. `None` when there is no topic.
pub fn topic(target: &str) -> Option<String> {
    let t = target.trim();
    let rest = t.split_once("://").map_or(t, |(_, rest)| rest);
    let (_, path) = rest.split_once('/')?;
    let topic = endpoint::percent_decode(path);
    (!topic.is_empty()).then_some(topic)
}

/// Topic and payload of a `PUBLISH` (QoS 0 only, as subscribed).
fn parse_publish(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = usize::from(u16::from_be_bytes([*body.first()?, *body.get(1)?]));
    let topic = body.get(2..2 + len)?;
    Some((topic, body.get(2 + len..)?))
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Mqtt, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let topic = topic(&monitor.target);
    let topic = topic.as_deref();
    let keyword = monitor.keyword.as_deref().filter(|k| !k.is_empty());
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    let probe = async {
        let connected = conn::connect(&ep, monitor.ignore_tls, timeout).await?;
        let mut s: Stream = connected.io;
        let fail = CheckOutcome::down;
        let client_id = format!("vigilant-vole-{}", hex::encode(rand::random::<[u8; 4]>()));
        let hello = connect_packet(
            &client_id,
            monitor.basic_auth_user.as_deref(),
            monitor.basic_auth_pass.as_deref(),
        );
        s.write_all(&hello).await.map_err(|e| fail(io_err(e)))?;
        // CONNACK is exactly 20 02 <flags> <code>; anything else isn't MQTT.
        let mut ack = [0u8; 4];
        s.read_exact(&mut ack).await.map_err(|e| fail(io_err(e)))?;
        if ack[..2] != [0x20, 0x02] {
            return Err(fail("Not an MQTT broker (no CONNACK)".into()));
        }
        if ack[3] != 0 {
            return Err(fail(connack_error(ack[3])));
        }
        let mut message = "Connected".to_string();
        if let Some(topic) = topic {
            s.write_all(&subscribe_packet(topic))
                .await
                .map_err(|e| fail(io_err(e)))?;
            loop {
                let p = read_packet(&mut s).await.map_err(fail)?;
                match p.kind {
                    9 => {
                        if p.body.get(2).is_none_or(|rc| *rc == 0x80) {
                            return Err(fail(format!(
                                "The broker refused the subscription to \"{}\"",
                                util::truncate(topic, 60)
                            )));
                        }
                        break;
                    }
                    3 | 13 => {} // a message or a ping answer that raced the SUBACK
                    _ => return Err(fail("Not an MQTT broker (unexpected packet)".into())),
                }
            }
            message = format!("Subscribed to {}", util::truncate(topic, 60));
            if let Some(keyword) = keyword {
                let invert = monitor.keyword_invert;
                loop {
                    let p = read_packet(&mut s).await.map_err(fail)?;
                    if p.kind != 3 {
                        continue;
                    }
                    let Some((_, payload)) = parse_publish(&p.body) else {
                        continue;
                    };
                    let hit = String::from_utf8_lossy(payload).contains(keyword);
                    if hit != invert {
                        message.push_str(if invert {
                            " · message without the keyword"
                        } else {
                            " · message matched"
                        });
                        break;
                    }
                    if invert {
                        return Err(fail(format!(
                            "Message contains \"{}\": {}",
                            util::truncate(keyword, 40),
                            util::snippet(&String::from_utf8_lossy(payload), 80)
                        )));
                    }
                }
            }
        }
        let ms = util::elapsed_ms(start);
        let _ = s.write_all(&[0xE0, 0x00]).await;
        Ok(conn::up(
            ms,
            message,
            connected.cert.as_ref(),
            connected.tls_version,
        ))
    };
    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(outcome)) | Ok(Err(outcome)) => outcome,
        Err(_) => match (topic, keyword) {
            (Some(topic), Some(_)) => CheckOutcome::down(format!(
                "No matching message on \"{}\" within {}",
                util::truncate(topic, 60),
                util::format_secs(timeout)
            )),
            _ => conn::timed_out(&format!("MQTT broker {}:{}", ep.host, ep.port), timeout),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::HeartbeatStatus;
    use crate::monitor::checkers::test_support::{monitor, tls_acceptor};

    /// A scripted broker: user `vole` / `pw` (when `auth`), retained message
    /// `{"state":"on"}` on topic `home/status`, nothing on other topics.
    async fn handle(mut s: Stream, auth: bool) {
        let Ok(connect) = read_packet(&mut s).await else {
            return;
        };
        let b = &connect.body;
        let flags = b[7];
        let mut at = 10;
        let take = |at: &mut usize| {
            let len = usize::from(u16::from_be_bytes([b[*at], b[*at + 1]]));
            let v = String::from_utf8_lossy(&b[*at + 2..*at + 2 + len]).into_owned();
            *at += 2 + len;
            v
        };
        let _client = take(&mut at);
        let user = (flags & 0x80 != 0).then(|| take(&mut at));
        let pass = (flags & 0x40 != 0).then(|| take(&mut at));
        let ok = !auth || (user.as_deref() == Some("vole") && pass.as_deref() == Some("pw"));
        s.write_all(&[0x20, 0x02, 0, if ok { 0 } else { 4 }])
            .await
            .unwrap();
        if !ok {
            return;
        }
        while let Ok(p) = read_packet(&mut s).await {
            match p.kind {
                8 => {
                    let len = usize::from(u16::from_be_bytes([p.body[2], p.body[3]]));
                    let topic = String::from_utf8_lossy(&p.body[4..4 + len]).into_owned();
                    let code = if topic.starts_with("secret/") {
                        0x80
                    } else {
                        0
                    };
                    s.write_all(&[0x90, 0x03, p.body[0], p.body[1], code])
                        .await
                        .unwrap();
                    if topic == "home/status" {
                        let mut body = Vec::new();
                        put_string(&mut body, &topic);
                        body.extend_from_slice(br#"{"state":"on"}"#);
                        s.write_all(&packet(0x31, &body)).await.unwrap();
                    }
                }
                14 => return,
                _ => {}
            }
        }
    }

    async fn serve(auth: bool, tls: bool) -> std::net::SocketAddr {
        let acceptor = tls_acceptor();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if tls {
                        if let Ok(stream) = acceptor.accept(tcp).await {
                            handle(Box::new(stream), auth).await;
                        }
                    } else {
                        handle(Box::new(tcp), auth).await;
                    }
                });
            }
        });
        addr
    }

    fn mqtt(target: &str) -> Monitor {
        let mut m = monitor(MonitorType::Mqtt, target);
        m.timeout_seconds = 2;
        m
    }

    #[test]
    fn topics_keep_wildcards_and_question_marks() {
        assert_eq!(topic("mqtt://broker/home/#").as_deref(), Some("home/#"));
        assert_eq!(topic("mqtts://broker:8883/a/+/b").as_deref(), Some("a/+/b"));
        assert_eq!(
            topic("broker:1883/sensors/temp?x").as_deref(),
            Some("sensors/temp?x")
        );
        assert_eq!(topic("[::1]:1883/#").as_deref(), Some("#"));
        assert_eq!(
            topic("mqtt://broker//leading/slash").as_deref(),
            Some("/leading/slash")
        );
        assert_eq!(
            topic("mqtt://broker/100%25/a%2Bb").as_deref(),
            Some("100%/a+b")
        );
        assert_eq!(topic("mqtt://broker/"), None);
        assert_eq!(topic("mqtt://broker:1883"), None);
    }

    #[tokio::test]
    async fn wildcard_topics_are_subscribed_as_written() {
        let addr = serve(false, false).await;
        for t in ["home/#", "a/+/b", "sensors/temp?x"] {
            let out = check(&mqtt(&format!("mqtt://{addr}/{t}"))).await;
            assert_eq!(
                out.message.as_deref(),
                Some(format!("Subscribed to {t}").as_str()),
                "{t}"
            );
        }
    }

    #[tokio::test]
    async fn connect_only() {
        let addr = serve(false, false).await;
        let out = check(&mqtt(&addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("Connected"));
    }

    #[tokio::test]
    async fn credentials_are_checked() {
        let addr = serve(true, false).await;
        let out = check(&mqtt(&addr.to_string())).await;
        assert_eq!(
            out.message.as_deref(),
            Some("Authentication failed: wrong user name or password")
        );
        let mut m = mqtt(&addr.to_string());
        m.basic_auth_user = Some("vole".into());
        m.basic_auth_pass = Some("pw".into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    }

    #[tokio::test]
    async fn subscribe_and_match_the_retained_message() {
        let addr = serve(false, false).await;
        let mut m = mqtt(&format!("mqtt://{addr}/home/status"));
        let out = check(&m).await;
        assert_eq!(out.message.as_deref(), Some("Subscribed to home/status"));

        m.keyword = Some(r#""state":"on""#.into());
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(
            out.message.as_deref(),
            Some("Subscribed to home/status · message matched")
        );

        m.keyword = Some("off".into());
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(
            out.message
                .unwrap()
                .starts_with("No matching message on \"home/status\"")
        );

        m.keyword_invert = true;
        assert_eq!(
            check(&m).await.status,
            HeartbeatStatus::Up,
            "the message lacks `off`"
        );
        m.keyword = Some("on".into());
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Message contains \"on\""));
    }

    #[tokio::test]
    async fn refused_subscription_is_down() {
        let addr = serve(false, false).await;
        let out = check(&mqtt(&format!("mqtt://{addr}/secret/x"))).await;
        assert_eq!(
            out.message.as_deref(),
            Some("The broker refused the subscription to \"secret/x\"")
        );
    }

    #[tokio::test]
    async fn mqtts_checks_the_certificate() {
        let addr = serve(false, true).await;
        let target = format!("mqtts://localhost:{}", addr.port());
        let out = check(&mqtt(&target)).await;
        assert!(
            out.message.as_deref().unwrap().contains("not trusted"),
            "{:?}",
            out.message
        );
        let mut m = mqtt(&target);
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("Connected · TLS 1.3"));
        assert!(out.expiry.is_some());
    }

    #[tokio::test]
    async fn other_servers_are_down() {
        let addr =
            crate::monitor::checkers::test_support::foreign_server(b"SSH-2.0-OpenSSH_9.6\r\n")
                .await;
        let out = check(&mqtt(&addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Not an MQTT broker"));
    }

    #[test]
    fn remaining_length_uses_the_variable_encoding() {
        assert_eq!(&packet(0x30, &[0; 200])[..3], &[0x30, 0xC8, 0x01]);
        assert_eq!(&packet(0x30, &[])[..2], &[0x30, 0x00]);
    }

    /// Against a real broker: `VV_TEST_MQTT=mqtt://host:port/topic`.
    #[tokio::test]
    #[ignore = "needs an MQTT broker (VV_TEST_MQTT)"]
    async fn live_broker() {
        let target = std::env::var("VV_TEST_MQTT").expect("VV_TEST_MQTT");
        let out = check(&mqtt(&target)).await;
        println!("{:?}", out.message);
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    }
}
