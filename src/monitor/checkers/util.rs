//! Small helpers shared by the checkers (formatting, target parsing, resolution).

use std::error::Error as StdError;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

/// Milliseconds since `start`, as stored in `heartbeats.response_time_ms`.
pub fn elapsed_ms(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// "10s", "1.5s", "250ms".
pub fn format_secs(d: Duration) -> String {
    if d.as_millis() < 1000 {
        format!("{}ms", d.as_millis())
    } else if d.subsec_millis() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Truncate to at most `max` characters (not bytes), appending `…` when cut.
pub fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.char_indices();
    match chars.nth(max) {
        None => s.to_string(),
        Some((idx, _)) => format!("{}…", s[..idx].trim_end()),
    }
}

/// A single-line preview of a response body: whitespace runs collapsed,
/// control characters dropped, cut to `max` characters.
pub fn snippet(body: &str, max: usize) -> String {
    let mut out = String::with_capacity(max.min(body.len()) + 4);
    let mut count = 0usize;
    let mut last_space = true;
    for c in body.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if c.is_control() {
            continue;
        }
        if c == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        if count == max {
            return format!("{}…", out.trim_end());
        }
        out.push(c);
        count += 1;
    }
    out.trim_end().to_string()
}

/// Format a unix timestamp as UTC `YYYY-MM-DD HH:MM:SS` (the storage format).
pub fn format_utc(unix: i64) -> Option<String> {
    let dt = time::OffsetDateTime::from_unix_timestamp(unix).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    ))
}

/// Current unix time in seconds.
pub fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// "in 12 days" / "today" / "3 days ago" for an expiry timestamp.
pub fn relative_days(expires_unix: i64, now_unix: i64) -> String {
    let diff = expires_unix - now_unix;
    let days = diff.div_euclid(86_400);
    if diff >= 0 {
        match days {
            0 => "in less than a day".to_string(),
            1 => "in 1 day".to_string(),
            n => format!("in {n} days"),
        }
    } else {
        let ago = (-diff) / 86_400;
        match ago {
            0 => "less than a day ago".to_string(),
            1 => "1 day ago".to_string(),
            n => format!("{n} days ago"),
        }
    }
}

/// Split `host:port`, `[v6]:port`, bare `host` or bare IPv6 into host and port.
/// Also tolerates a URL-ish prefix (`udp://host:port`, trailing `/`).
pub fn split_host_port(target: &str, default_port: Option<u16>) -> Result<(String, u16), String> {
    let mut t = target.trim();
    if let Some(idx) = t.find("://") {
        t = &t[idx + 3..];
    }
    let t = t.trim_end_matches('/');
    if t.is_empty() {
        return Err("Target is empty".into());
    }
    let missing_port = || match default_port {
        Some(p) => Ok(p),
        None => Err(format!("Target \"{t}\" needs a port (host:port)")),
    };
    let parse_port = |p: &str| {
        p.parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| format!("Invalid port \"{p}\""))
    };

    if let Some(rest) = t.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("Invalid address \"{t}\""))?;
        let port = match after.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if after.is_empty() => missing_port()?,
            None => return Err(format!("Invalid address \"{t}\"")),
        };
        return Ok((host.to_string(), port));
    }
    if t.parse::<IpAddr>().is_ok() {
        // Bare IPv4 or IPv6 without brackets.
        return Ok((t.to_string(), missing_port()?));
    }
    match t.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            if host.is_empty() {
                return Err(format!("Target \"{t}\" has no host"));
            }
            Ok((host.to_string(), parse_port(port)?))
        }
        Some(_) => Err(format!(
            "Invalid address \"{t}\" (wrap IPv6 in brackets: [::1]:port)"
        )),
        None => Ok((t.to_string(), missing_port()?)),
    }
}

/// Resolve `host:port` to the first socket address. For UDP probes, which
/// can't tell a dead address from a slow one; TCP checkers use
/// [`resolve_all`] + [`connect_any`].
pub async fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    resolve_all(host, port).await.map(|addrs| addrs[0])
}

/// Resolve `host:port` to every socket address, in the resolver's order
/// (never empty on success).
pub async fn resolve_all(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => {
            let mut list: Vec<SocketAddr> = Vec::new();
            for addr in addrs {
                if !list.contains(&addr) {
                    list.push(addr);
                }
            }
            if list.is_empty() {
                Err(format!("Could not resolve \"{host}\": no addresses"))
            } else {
                Ok(list)
            }
        }
        Err(err) => Err(format!(
            "Could not resolve \"{host}\": {}",
            tidy_io_error(&err)
        )),
    }
}

/// How long an address gets before the next one is tried alongside it.
const CONNECT_STAGGER: Duration = Duration::from_millis(250);

/// Open a TCP connection to the first of `addrs` that accepts, in their
/// order. `localhost` often resolves to `::1` first while a service listens
/// on IPv4 only, so the next address starts as soon as one fails, or after
/// [`CONNECT_STAGGER`] if it hasn't answered yet (Happy Eyeballs, RFC 8305:
/// Windows takes about two seconds to report a refused connection, and a
/// black-holed address never does); the first connection wins and the
/// others are dropped. The caller's timeout bounds the whole thing. `Err` =
/// the last address that failed and its error.
pub async fn connect_any(
    addrs: &[SocketAddr],
) -> Result<(tokio::net::TcpStream, SocketAddr), (SocketAddr, std::io::Error)> {
    use futures_util::StreamExt as _;
    use futures_util::stream::FuturesUnordered;

    let attempt =
        |addr: SocketAddr| async move { (addr, tokio::net::TcpStream::connect(addr).await) };
    let mut rest = addrs.iter().copied();
    let mut running = FuturesUnordered::new();
    running.extend(rest.next().map(attempt));
    let mut last = None;
    while !running.is_empty() {
        tokio::select! {
            Some((addr, result)) = running.next() => match result {
                Ok(tcp) => return Ok((tcp, addr)),
                Err(err) => {
                    last = Some((addr, err));
                    running.extend(rest.next().map(attempt));
                }
            },
            () = tokio::time::sleep(CONNECT_STAGGER), if rest.len() > 0 => running.extend(rest.next().map(attempt)),
        }
    }
    Err(last.unwrap_or_else(|| {
        let none = SocketAddr::from(([0, 0, 0, 0], 0));
        (
            none,
            std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no addresses"),
        )
    }))
}

/// Friendly text for common I/O errors.
pub fn tidy_io_error(err: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::ConnectionRefused => "Connection refused".into(),
        ErrorKind::ConnectionReset => "Connection reset by peer".into(),
        ErrorKind::ConnectionAborted => "Connection aborted".into(),
        ErrorKind::TimedOut => "Timed out".into(),
        ErrorKind::PermissionDenied => "Permission denied".into(),
        ErrorKind::AddrNotAvailable => "Address not available".into(),
        ErrorKind::NetworkUnreachable => "Network unreachable".into(),
        ErrorKind::HostUnreachable => "Host unreachable".into(),
        _ => {
            let text = err.to_string();
            // Strip the "(os error N)" suffix the OS adds.
            match text.find(" (os error") {
                Some(idx) => text[..idx].to_string(),
                None => text,
            }
        }
    }
}

/// Friendly text for a failed UDP receive: on a connected socket an ICMP
/// "port unreachable" surfaces as refused (Linux) or reset (Windows).
pub fn udp_recv_error(err: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset => {
            "port closed (ICMP port unreachable)".into()
        }
        _ => tidy_io_error(err),
    }
}

/// The innermost error message of a source chain.
pub fn root_cause(err: &(dyn StdError + 'static)) -> String {
    let mut cur: &(dyn StdError + 'static) = err;
    while let Some(next) = cur.source() {
        cur = next;
    }
    if let Some(io) = cur.downcast_ref::<std::io::Error>() {
        return tidy_io_error(io);
    }
    cur.to_string()
}

/// Every message of a source chain joined with ": " (for substring matching).
pub fn full_chain(err: &(dyn StdError + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut cur = err.source();
    while let Some(e) = cur {
        parts.push(e.to_string());
        cur = e.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("héllo wörld", 5), "héllo…");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn snippet_collapses_whitespace() {
        assert_eq!(
            snippet("  <html>\n\n  <body>  hi\t</body>", 80),
            "<html> <body> hi </body>"
        );
        assert_eq!(snippet("abcdef", 3), "abc…");
    }

    #[test]
    fn format_utc_matches_storage_format() {
        assert_eq!(format_utc(0).as_deref(), Some("1970-01-01 00:00:00"));
        assert_eq!(
            format_utc(1_700_000_000).as_deref(),
            Some("2023-11-14 22:13:20")
        );
    }

    #[test]
    fn relative_days_reads_naturally() {
        assert_eq!(relative_days(10 * 86_400 + 5, 0), "in 10 days");
        assert_eq!(relative_days(100, 0), "in less than a day");
        assert_eq!(relative_days(0, 3 * 86_400 + 10), "3 days ago");
    }

    #[test]
    fn split_host_port_variants() {
        assert_eq!(
            split_host_port("example.com:80", None),
            Ok(("example.com".into(), 80))
        );
        assert_eq!(
            split_host_port("example.com", Some(123)),
            Ok(("example.com".into(), 123))
        );
        assert_eq!(
            split_host_port("[::1]:5353", None),
            Ok(("::1".into(), 5353))
        );
        assert_eq!(split_host_port("::1", Some(53)), Ok(("::1".into(), 53)));
        assert_eq!(
            split_host_port("udp://10.0.0.1:27015/", None),
            Ok(("10.0.0.1".into(), 27015))
        );
        assert!(split_host_port("example.com", None).is_err());
        assert!(split_host_port("example.com:0", None).is_err());
        assert!(split_host_port("example.com:http", None).is_err());
        assert!(split_host_port("", Some(1)).is_err());
    }

    #[test]
    fn format_secs_variants() {
        assert_eq!(format_secs(Duration::from_secs(10)), "10s");
        assert_eq!(format_secs(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_secs(Duration::from_millis(250)), "250ms");
    }

    fn closed_port() -> SocketAddr {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    }

    #[tokio::test]
    async fn connect_any_moves_past_refused_addresses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        let dead = closed_port();
        let (_, used) = connect_any(&[dead, open]).await.unwrap();
        assert_eq!(used, open);

        let other = closed_port();
        let (addr, err) = connect_any(&[dead, other]).await.unwrap_err();
        assert_eq!(addr, other, "the last error is reported");
        assert_eq!(tidy_io_error(&err), "Connection refused");
    }

    #[tokio::test]
    async fn localhost_reaches_an_ipv4_only_listener() {
        // `localhost` resolves to `::1` first on Windows and in Alpine.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let addrs = resolve_all("localhost", port).await.unwrap();
        let start = Instant::now();
        let (_, used) = connect_any(&addrs).await.unwrap();
        assert_eq!(used, SocketAddr::from(([127, 0, 0, 1], port)));
        // Not after waiting out a slow refusal from `::1` (about 2 s on Windows).
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn a_silent_address_does_not_hold_up_the_next() {
        // 10.255.255.1 is unroutable here: the attempt hangs until dropped.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        let silent = SocketAddr::from(([10, 255, 255, 1], open.port()));
        let start = Instant::now();
        let (_, used) = tokio::time::timeout(Duration::from_secs(5), connect_any(&[silent, open]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(used, open);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }
}
