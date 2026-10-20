//! Redis checker: `target` = `[redis|rediss]://host[:port][/db]`.
//!
//! Connects (TLS for `rediss://`), authenticates when a password is set
//! (`AUTH user pass` with a user name, `AUTH pass` without), selects the
//! database from the path, and sends `PING`. The server version from
//! `INFO server` is shown when the server allows it. Response time = connect
//! + auth + `PING`.

use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::conn::{self, Io};
use super::endpoint;
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

/// Largest bulk string accepted (the `INFO` reply is a few KB).
const MAX_BULK: usize = 256 * 1024;
const MAX_LINE: u64 = 4096;

#[derive(Debug, PartialEq, Eq)]
enum Value {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<Vec<u8>>),
}

fn encode(args: &[&str]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

async fn read_line<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<String, String> {
    let mut raw = Vec::new();
    let n = r.take(MAX_LINE).read_until(b'\n', &mut raw).await.map_err(|e| util::tidy_io_error(&e))?;
    if n == 0 {
        return Err("Connection closed by the server".into());
    }
    if !raw.ends_with(b"\r\n") {
        return Err("Not a Redis server (bad reply)".into());
    }
    raw.truncate(raw.len() - 2);
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

async fn read_value<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<Value, String> {
    let line = read_line(r).await?;
    let (kind, rest) = line.split_at_checked(1).ok_or("Not a Redis server (empty reply)")?;
    match kind {
        "+" => Ok(Value::Simple(rest.to_string())),
        "-" => Ok(Value::Error(rest.to_string())),
        ":" => rest.parse().map(Value::Int).map_err(|_| "Not a Redis server (bad integer)".to_string()),
        "$" => {
            let len: i64 = rest.parse().map_err(|_| "Not a Redis server (bad length)".to_string())?;
            if len < 0 {
                return Ok(Value::Bulk(None));
            }
            let len = len as usize;
            if len > MAX_BULK {
                return Err("Reply from the server is too large".into());
            }
            let mut buf = vec![0u8; len + 2];
            r.read_exact(&mut buf).await.map_err(|e| util::tidy_io_error(&e))?;
            buf.truncate(len);
            Ok(Value::Bulk(Some(buf)))
        }
        _ => Err("Not a Redis server (unexpected reply)".into()),
    }
}

async fn request<S: Io>(r: &mut BufReader<S>, args: &[&str]) -> Result<Value, String> {
    r.get_mut().write_all(&encode(args)).await.map_err(|e| util::tidy_io_error(&e))?;
    read_value(r).await
}

/// Friendly text for the error replies a monitor commonly meets.
fn explain(err: &str) -> String {
    let upper = err.to_ascii_uppercase();
    if upper.starts_with("NOAUTH") {
        "Authentication required: set a password".into()
    } else if upper.starts_with("WRONGPASS") || upper.contains("INVALID PASSWORD") || upper.contains("WRONG NUMBER OF ARGUMENTS FOR 'AUTH'") {
        "Authentication failed: wrong user name or password".into()
    } else if upper.contains("WITHOUT ANY PASSWORD CONFIGURED") {
        "The server has no password: remove the password from the monitor".into()
    } else if upper.starts_with("NOPERM") {
        format!("Permission denied: {}", util::truncate(err, 100))
    } else if upper.contains("DB INDEX IS OUT OF RANGE") {
        "The database index is out of range".into()
    } else {
        format!("Server error: {}", util::truncate(err, 100))
    }
}

fn version_from_info(info: &[u8]) -> Option<String> {
    String::from_utf8_lossy(info)
        .lines()
        .find_map(|l| l.strip_prefix("redis_version:").or_else(|| l.strip_prefix("valkey_version:")))
        .map(|v| v.trim().to_string())
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Redis, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let db = match ep.path.as_str() {
        "" => None,
        p => match p.parse::<u32>() {
            Ok(n) => Some(n),
            Err(_) => return CheckOutcome::down(format!("The database \"{}\" must be a number", util::truncate(p, 20))),
        },
    };
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    let probe = async {
        let connected = conn::connect(&ep, monitor.ignore_tls, timeout).await?;
        let mut r = BufReader::new(connected.io);
        let fail = |msg: String| CheckOutcome::down(msg);
        if let Some(pass) = monitor.basic_auth_pass.as_deref() {
            let reply = match monitor.basic_auth_user.as_deref() {
                Some(user) => request(&mut r, &["AUTH", user, pass]).await,
                None => request(&mut r, &["AUTH", pass]).await,
            };
            match reply.map_err(fail)? {
                Value::Simple(_) => {}
                Value::Error(e) => return Err(fail(explain(&e))),
                _ => return Err(fail("Unexpected reply to AUTH".into())),
            }
        }
        if let Some(n) = db {
            match request(&mut r, &["SELECT", &n.to_string()]).await.map_err(fail)? {
                Value::Simple(_) => {}
                Value::Error(e) => return Err(fail(explain(&e))),
                _ => return Err(fail("Unexpected reply to SELECT".into())),
            }
        }
        match request(&mut r, &["PING"]).await.map_err(fail)? {
            Value::Simple(s) if s.eq_ignore_ascii_case("PONG") => {}
            Value::Error(e) => return Err(fail(explain(&e))),
            other => return Err(fail(format!("Unexpected reply to PING: {}", util::truncate(&format!("{other:?}"), 60)))),
        }
        let ms = util::elapsed_ms(start);
        let version = match request(&mut r, &["INFO", "server"]).await {
            Ok(Value::Bulk(Some(info))) => version_from_info(&info),
            _ => None,
        };
        let _ = request(&mut r, &["QUIT"]).await;
        let mut message = match version {
            Some(v) => format!("PONG · Redis {v}"),
            None => "PONG".to_string(),
        };
        if let Some(n) = db {
            message.push_str(&format!(" · db {n}"));
        }
        Ok(conn::up(ms, message, connected.cert.as_ref(), connected.tls_version))
    };
    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(outcome)) | Ok(Err(outcome)) => outcome,
        Err(_) => conn::timed_out(&format!("Redis on {}:{}", ep.host, ep.port), timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::HeartbeatStatus;
    use crate::monitor::checkers::test_support::{monitor, tls_acceptor};
    use tokio::io::{AsyncRead, AsyncWrite};

    /// A scripted Redis. `password` (if any) is required for everything
    /// but AUTH; databases above 15 don't exist.
    async fn handle<S: AsyncRead + AsyncWrite + Unpin>(stream: S, password: Option<&'static str>) {
        let mut r = BufReader::new(stream);
        let mut authed = password.is_none();
        loop {
            let mut head = String::new();
            if r.read_line(&mut head).await.unwrap_or(0) == 0 {
                return;
            }
            let n: usize = head.trim().trim_start_matches('*').parse().unwrap_or(0);
            let mut args = Vec::new();
            for _ in 0..n {
                let mut len = String::new();
                r.read_line(&mut len).await.unwrap();
                let len: usize = len.trim().trim_start_matches('$').parse().unwrap();
                let mut buf = vec![0u8; len + 2];
                r.read_exact(&mut buf).await.unwrap();
                buf.truncate(len);
                args.push(String::from_utf8(buf).unwrap());
            }
            let cmd = args.first().map(|s| s.to_ascii_uppercase()).unwrap_or_default();
            let reply: Vec<u8> = match cmd.as_str() {
                "AUTH" => {
                    let given = args.last().map(String::as_str);
                    if given == password {
                        authed = true;
                        b"+OK\r\n".to_vec()
                    } else {
                        b"-WRONGPASS invalid username-password pair or user is disabled.\r\n".to_vec()
                    }
                }
                _ if !authed => b"-NOAUTH Authentication required.\r\n".to_vec(),
                "SELECT" if args[1].parse::<u32>().unwrap() > 15 => b"-ERR DB index is out of range\r\n".to_vec(),
                "SELECT" => b"+OK\r\n".to_vec(),
                "PING" => b"+PONG\r\n".to_vec(),
                "INFO" => {
                    let body = "# Server\r\nredis_version:7.2.4\r\nredis_mode:standalone\r\n";
                    format!("${}\r\n{body}\r\n", body.len()).into_bytes()
                }
                "QUIT" => {
                    r.get_mut().write_all(b"+OK\r\n").await.unwrap();
                    return;
                }
                _ => b"-ERR unknown command\r\n".to_vec(),
            };
            r.get_mut().write_all(&reply).await.unwrap();
        }
    }

    async fn serve(password: Option<&'static str>, tls: bool) -> std::net::SocketAddr {
        let acceptor = tls_acceptor();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if tls {
                        if let Ok(stream) = acceptor.accept(tcp).await {
                            handle(stream, password).await;
                        }
                    } else {
                        handle(tcp, password).await;
                    }
                });
            }
        });
        addr
    }

    fn redis(target: &str) -> Monitor {
        monitor(MonitorType::Redis, target)
    }

    #[tokio::test]
    async fn open_redis_answers_pong_with_version() {
        let addr = serve(None, false).await;
        let out = check(&redis(&addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("PONG · Redis 7.2.4"));
    }

    #[tokio::test]
    async fn password_and_database_are_used() {
        let addr = serve(Some("s3cret"), false).await;
        let mut m = redis(&format!("redis://{addr}/3"));
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert_eq!(out.message.as_deref(), Some("Authentication required: set a password"));

        m.basic_auth_pass = Some("wrong".into());
        let out = check(&m).await;
        assert_eq!(out.message.as_deref(), Some("Authentication failed: wrong user name or password"));

        m.basic_auth_pass = Some("s3cret".into());
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("PONG · Redis 7.2.4 · db 3"));

        m.target = format!("redis://{addr}/99");
        let out = check(&m).await;
        assert_eq!(out.message.as_deref(), Some("The database index is out of range"));
        m.target = format!("redis://{addr}/main");
        assert!(check(&m).await.message.unwrap().contains("must be a number"));
    }

    #[tokio::test]
    async fn acl_user_name_is_sent() {
        let addr = serve(Some("pw"), false).await;
        let mut m = redis(&addr.to_string());
        m.basic_auth_user = Some("monitor".into());
        m.basic_auth_pass = Some("pw".into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    }

    #[tokio::test]
    async fn rediss_checks_the_certificate() {
        let addr = serve(None, true).await;
        let target = format!("rediss://localhost:{}", addr.port());
        let out = check(&redis(&target)).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.as_deref().unwrap().contains("not trusted"), "{:?}", out.message);

        let mut m = redis(&target);
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert!(out.message.unwrap().ends_with("· TLS 1.3"));
        assert!(out.expiry.is_some());
    }

    #[tokio::test]
    async fn other_servers_and_closed_ports_are_down() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });
        let out = check(&redis(&addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Not a Redis server"));

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);
        let out = check(&redis(&addr.to_string())).await;
        assert!(out.message.unwrap().contains("Connection refused"));
    }

    #[test]
    fn commands_are_encoded_as_resp_arrays() {
        assert_eq!(encode(&["AUTH", "u", "p"]), b"*3\r\n$4\r\nAUTH\r\n$1\r\nu\r\n$1\r\np\r\n");
    }

    /// Against a real server: `VV_TEST_REDIS=redis://host:port/db,user,password` (user and password may be empty).
    #[tokio::test]
    #[ignore = "needs a Redis server (VV_TEST_REDIS)"]
    async fn live_server() {
        let spec = std::env::var("VV_TEST_REDIS").expect("VV_TEST_REDIS");
        let [target, user, pass] = spec.splitn(3, ',').collect::<Vec<_>>()[..] else { panic!("target,user,password") };
        let mut m = redis(target);
        m.basic_auth_user = (!user.is_empty()).then(|| user.to_string());
        m.basic_auth_pass = (!pass.is_empty()).then(|| pass.to_string());
        let out = check(&m).await;
        println!("{:?}", out.message);
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        m.basic_auth_pass = Some("wrong".into());
        println!("{:?}", check(&m).await.message);
        m.basic_auth_pass = None;
        println!("{:?}", check(&m).await.message);
    }
}
