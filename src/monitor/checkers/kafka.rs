//! Kafka checker: `target` = `[kafka|kafkas]://host[:port][/topic]`.
//!
//! Speaks the Kafka wire protocol directly (no SASL): `ApiVersions` v0 to
//! prove a broker answers, then `Metadata` v4 for the broker count and the
//! topic from the path. The topic is never created (`allow_auto_topic_creation`
//! is off); the check fails when it doesn't exist or a partition has no
//! leader. Response time = connect + both requests.

use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::conn::{self, Io};
use super::endpoint;
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const API_METADATA: i16 = 3;
const API_VERSIONS: i16 = 18;
const METADATA_VERSION: i16 = 4;
const CLIENT_ID: &str = "vigilant-vole";

type Stream = Box<dyn Io>;

fn io_err(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        "Connection closed by the broker (does it require SASL?)".into()
    } else {
        util::tidy_io_error(&e)
    }
}

/// A big-endian reader over a response body.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }
    fn i16(&mut self) -> Option<i16> {
        self.take(2).map(|b| i16::from_be_bytes([b[0], b[1]]))
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4).map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// A non-negative array length, bounded by the bytes left (each element is at least `min` bytes).
    fn count(&mut self, min: usize) -> Option<usize> {
        let n = usize::try_from(self.i32()?).ok()?;
        (n.checked_mul(min)? <= self.0.len()).then_some(n)
    }
    fn string(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.i16()?).ok()?;
        self.take(n)
    }
    fn nullable_string(&mut self) -> Option<()> {
        let n = self.i16()?;
        if n >= 0 {
            self.take(n as usize)?;
        }
        Some(())
    }
}

fn request(api_key: i16, version: i16, correlation: i32, body: &[u8]) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(&api_key.to_be_bytes());
    msg.extend_from_slice(&version.to_be_bytes());
    msg.extend_from_slice(&correlation.to_be_bytes());
    msg.extend_from_slice(&(CLIENT_ID.len() as i16).to_be_bytes());
    msg.extend_from_slice(CLIENT_ID.as_bytes());
    msg.extend_from_slice(body);
    let mut out = (msg.len() as i32).to_be_bytes().to_vec();
    out.extend_from_slice(&msg);
    out
}

/// Send a request and return the response body after the correlation id.
async fn call(s: &mut Stream, api_key: i16, version: i16, correlation: i32, body: &[u8]) -> Result<Vec<u8>, String> {
    s.write_all(&request(api_key, version, correlation, body)).await.map_err(io_err)?;
    let len = s.read_i32().await.map_err(io_err)?;
    let len = usize::try_from(len).ok().filter(|l| (4..=MAX_RESPONSE).contains(l)).ok_or("Not a Kafka broker (bad response)")?;
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf).await.map_err(io_err)?;
    if i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) != correlation {
        return Err("Not a Kafka broker (wrong correlation id)".into());
    }
    buf.drain(..4);
    Ok(buf)
}

/// The API ranges from `ApiVersions`: `(api_key, min, max)`.
fn parse_api_versions(body: &[u8]) -> Result<Vec<(i16, i16, i16)>, String> {
    let bad = || "Not a Kafka broker (bad ApiVersions reply)".to_string();
    let mut r = Reader(body);
    let error = r.i16().ok_or_else(bad)?;
    let n = r.count(6).ok_or_else(bad)?;
    let apis = (0..n).map(|_| Some((r.i16()?, r.i16()?, r.i16()?))).collect::<Option<Vec<_>>>().ok_or_else(bad)?;
    // 35 = UNSUPPORTED_VERSION: the reply still lists what the broker speaks.
    if error != 0 && error != 35 {
        return Err(format!("The broker refused the ApiVersions request (error {error})"));
    }
    Ok(apis)
}

struct Cluster {
    brokers: usize,
    /// `(error_code, partitions, partitions without a leader)` of the topic asked for.
    topic: Option<(i16, usize, usize)>,
}

fn parse_metadata(body: &[u8], want_topic: bool) -> Option<Cluster> {
    let mut r = Reader(body);
    r.i32()?; // throttle time
    let brokers = r.count(10)?;
    for _ in 0..brokers {
        r.i32()?; // node id
        r.string()?; // host
        r.i32()?; // port
        r.nullable_string()?; // rack
    }
    r.nullable_string()?; // cluster id
    r.i32()?; // controller id
    let topics = r.count(7)?;
    let mut topic = None;
    for _ in 0..topics {
        let error = r.i16()?;
        r.string()?;
        r.take(1)?; // is_internal
        let n = r.count(14)?;
        let mut no_leader = 0;
        for _ in 0..n {
            r.i16()?; // partition error
            r.i32()?; // index
            if r.i32()? < 0 {
                no_leader += 1;
            }
            for _ in 0..2 {
                // replicas, then in-sync replicas
                let m = r.count(4)?;
                r.take(m * 4)?;
            }
        }
        if want_topic {
            topic = Some((error, n, no_leader));
        }
    }
    Some(Cluster { brokers, topic })
}

fn topic_error(code: i16, topic: &str) -> String {
    let topic = util::truncate(topic, 60);
    match code {
        3 => format!("The topic \"{topic}\" does not exist"),
        17 => format!("\"{topic}\" is not a valid topic name"),
        29 => format!("Not authorized to read the topic \"{topic}\""),
        other => format!("The broker reported error {other} for the topic \"{topic}\""),
    }
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Kafka, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let topic = (!ep.path.is_empty()).then_some(ep.path.as_str());
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    let probe = async {
        let connected = conn::connect(&ep, monitor.ignore_tls, timeout).await?;
        let mut s: Stream = connected.io;
        let fail = CheckOutcome::down;
        let apis = parse_api_versions(&call(&mut s, API_VERSIONS, 0, 1, &[]).await.map_err(fail)?).map_err(fail)?;
        let metadata_ok = apis.iter().any(|(k, min, max)| *k == API_METADATA && (*min..=*max).contains(&METADATA_VERSION));
        if !metadata_ok {
            return Err(fail(format!("The broker doesn't support Metadata v{METADATA_VERSION}")));
        }
        let mut body = Vec::new();
        match topic {
            Some(t) => {
                body.extend_from_slice(&1i32.to_be_bytes());
                body.extend_from_slice(&(t.len() as i16).to_be_bytes());
                body.extend_from_slice(t.as_bytes());
            }
            None => body.extend_from_slice(&0i32.to_be_bytes()),
        }
        body.push(0); // allow_auto_topic_creation = false
        let reply = call(&mut s, API_METADATA, METADATA_VERSION, 2, &body).await.map_err(fail)?;
        let cluster = parse_metadata(&reply, topic.is_some()).ok_or_else(|| fail("Not a Kafka broker (bad Metadata reply)".into()))?;
        let ms = util::elapsed_ms(start);
        let brokers = match cluster.brokers {
            1 => "1 broker".to_string(),
            n => format!("{n} brokers"),
        };
        let message = match (topic, cluster.topic) {
            (Some(t), Some((0, partitions, 0))) => format!("{brokers} · topic {} · {partitions} partitions", util::truncate(t, 60)),
            (Some(t), Some((0, _, no_leader))) => {
                return Err(fail(format!("The topic \"{}\" has {no_leader} partition(s) without a leader", util::truncate(t, 60))));
            }
            (Some(t), Some((code, _, _))) => return Err(fail(topic_error(code, t))),
            (Some(t), None) => return Err(fail(topic_error(3, t))),
            (None, _) => brokers,
        };
        Ok(conn::up(ms, message, connected.cert.as_ref(), connected.tls_version))
    };
    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(outcome)) | Ok(Err(outcome)) => outcome,
        Err(_) => conn::timed_out(&format!("Kafka broker {}:{}", ep.host, ep.port), timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::HeartbeatStatus;
    use crate::monitor::checkers::test_support::{monitor, tls_acceptor};

    fn string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as i16).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    /// A scripted broker with topics `orders` (3 healthy partitions) and
    /// `stuck` (one partition without a leader).
    async fn handle(mut s: Stream, metadata_min: i16) {
        loop {
            let Ok(len) = s.read_i32().await else { return };
            let mut msg = vec![0u8; len as usize];
            if s.read_exact(&mut msg).await.is_err() {
                return;
            }
            let api = i16::from_be_bytes([msg[0], msg[1]]);
            let correlation = &msg[4..8];
            let client_len = i16::from_be_bytes([msg[8], msg[9]]) as usize;
            let body = &msg[10 + client_len..];
            let mut reply = correlation.to_vec();
            match api {
                18 => {
                    reply.extend_from_slice(&0i16.to_be_bytes());
                    reply.extend_from_slice(&2i32.to_be_bytes());
                    for (k, min, max) in [(18i16, 0i16, 3i16), (3, metadata_min, 12)] {
                        for v in [k, min, max] {
                            reply.extend_from_slice(&v.to_be_bytes());
                        }
                    }
                }
                3 => {
                    reply.extend_from_slice(&0i32.to_be_bytes()); // throttle
                    reply.extend_from_slice(&2i32.to_be_bytes()); // brokers
                    for id in 1..=2i32 {
                        reply.extend_from_slice(&id.to_be_bytes());
                        string(&mut reply, "kafka.local");
                        reply.extend_from_slice(&(9091 + id).to_be_bytes());
                        reply.extend_from_slice(&(-1i16).to_be_bytes()); // rack
                    }
                    string(&mut reply, "cluster-1");
                    reply.extend_from_slice(&1i32.to_be_bytes()); // controller
                    let n = i32::from_be_bytes(body[..4].try_into().unwrap());
                    assert_eq!(*body.last().unwrap(), 0, "auto topic creation must stay off");
                    let names: Vec<String> = if n == 0 {
                        vec![]
                    } else {
                        let l = i16::from_be_bytes([body[4], body[5]]) as usize;
                        vec![String::from_utf8_lossy(&body[6..6 + l]).into_owned()]
                    };
                    reply.extend_from_slice(&(names.len() as i32).to_be_bytes());
                    for name in names {
                        let known = name == "orders" || name == "stuck";
                        reply.extend_from_slice(&(if known { 0i16 } else { 3 }).to_be_bytes());
                        string(&mut reply, &name);
                        reply.push(0);
                        let partitions: i32 = if name == "orders" { 3 } else if name == "stuck" { 2 } else { 0 };
                        reply.extend_from_slice(&partitions.to_be_bytes());
                        for p in 0..partitions {
                            reply.extend_from_slice(&0i16.to_be_bytes());
                            reply.extend_from_slice(&p.to_be_bytes());
                            let leader = if name == "stuck" && p == 1 { -1i32 } else { 1 };
                            reply.extend_from_slice(&leader.to_be_bytes());
                            for _ in 0..2 {
                                reply.extend_from_slice(&1i32.to_be_bytes());
                                reply.extend_from_slice(&1i32.to_be_bytes());
                            }
                        }
                    }
                }
                _ => return,
            }
            let mut out = (reply.len() as i32).to_be_bytes().to_vec();
            out.extend_from_slice(&reply);
            s.write_all(&out).await.unwrap();
        }
    }

    async fn serve(tls: bool, metadata_min: i16) -> std::net::SocketAddr {
        let acceptor = tls_acceptor();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if tls {
                        if let Ok(stream) = acceptor.accept(tcp).await {
                            handle(Box::new(stream), metadata_min).await;
                        }
                    } else {
                        handle(Box::new(tcp), metadata_min).await;
                    }
                });
            }
        });
        addr
    }

    fn kafka(target: &str) -> Monitor {
        monitor(MonitorType::Kafka, target)
    }

    #[tokio::test]
    async fn broker_answers() {
        let addr = serve(false, 0).await;
        let out = check(&kafka(&addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("2 brokers"));
    }

    #[tokio::test]
    async fn topics_are_checked() {
        let addr = serve(false, 4).await;
        let out = check(&kafka(&format!("kafka://{addr}/orders"))).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("2 brokers · topic orders · 3 partitions"));
        let out = check(&kafka(&format!("kafka://{addr}/missing"))).await;
        assert_eq!(out.message.as_deref(), Some("The topic \"missing\" does not exist"));
        let out = check(&kafka(&format!("kafka://{addr}/stuck"))).await;
        assert_eq!(out.message.as_deref(), Some("The topic \"stuck\" has 1 partition(s) without a leader"));
    }

    #[tokio::test]
    async fn brokers_without_metadata_v4_are_refused() {
        let addr = serve(false, 5).await;
        let out = check(&kafka(&addr.to_string())).await;
        assert_eq!(out.message.as_deref(), Some("The broker doesn't support Metadata v4"));
    }

    #[tokio::test]
    async fn kafkas_checks_the_certificate() {
        let addr = serve(true, 0).await;
        let target = format!("kafkas://localhost:{}", addr.port());
        let out = check(&kafka(&target)).await;
        assert!(out.message.as_deref().unwrap().contains("not trusted"), "{:?}", out.message);
        let mut m = kafka(&target);
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert!(out.expiry.is_some());
    }

    #[tokio::test]
    async fn other_servers_are_down() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });
        let out = check(&kafka(&addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Not a Kafka broker"), "");
    }

    #[test]
    fn hostile_lengths_do_not_panic() {
        assert!(parse_api_versions(&[0, 0, 0x7F, 0xFF, 0xFF, 0xFF]).is_err());
        assert!(parse_metadata(&[0, 0, 0, 0, 0x7F, 0xFF, 0xFF, 0xFF], false).is_none());
        assert!(parse_metadata(&[], false).is_none());
    }

    /// Against a real broker: `VV_TEST_KAFKA=kafka://host:port/topic`.
    #[tokio::test]
    #[ignore = "needs a Kafka broker (VV_TEST_KAFKA)"]
    async fn live_broker() {
        let target = std::env::var("VV_TEST_KAFKA").expect("VV_TEST_KAFKA");
        let out = check(&kafka(&target)).await;
        println!("{:?}", out.message);
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    }
}
