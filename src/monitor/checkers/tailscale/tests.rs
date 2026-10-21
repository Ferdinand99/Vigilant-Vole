use super::*;

/// A status like tailscaled's (trimmed; field names as Go writes them).
const STATUS: &str = r#"{
	"Version": "1.90.6-t1234",
	"BackendState": "Running",
	"Self": {"HostName": "vole-host", "DNSName": "vole-host.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"], "Online": true},
	"Health": [],
	"Peer": {
		"nodekey:aa": {"HostName": "NAS", "DNSName": "nas.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.5", "fd7a:115c:a1e0::5"], "Online": true, "LastSeen": "0001-01-01T00:00:00Z"},
		"nodekey:bb": {"HostName": "raspberrypi", "DNSName": "garden.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.9"], "Online": false, "LastSeen": "2026-10-19T21:04:05.123456Z"},
		"nodekey:cc": {"HostName": "garden", "DNSName": "garden-1.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.10"], "Online": true},
		"nodekey:dd": {"HostName": "router", "DNSName": "", "TailscaleIPs": null, "Online": true}
	}
}"#;

fn status() -> Status {
    serde_json::from_str(STATUS).unwrap()
}

#[test]
fn targets_parse_with_an_optional_type() {
    assert_eq!(
        Target::parse(" nas ").unwrap(),
        Target {
            peer: "nas".into(),
            kind: PingType::Disco
        }
    );
    assert_eq!(
        Target::parse("nas.tail1234.ts.net.?type=TSMP").unwrap(),
        Target {
            peer: "nas.tail1234.ts.net".into(),
            kind: PingType::Tsmp
        }
    );
    assert_eq!(
        Target::parse("100.64.0.5?type=icmp").unwrap().kind,
        PingType::Icmp
    );
    assert_eq!(
        Target::parse("nas?type=disco").unwrap().kind,
        PingType::Disco
    );
    for bad in [
        "",
        "?type=tsmp",
        "https://nas",
        "nas/x",
        "my nas",
        "nas?type=peerapi",
        "nas?count=3",
    ] {
        assert!(Target::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn peers_resolve_by_ip_dns_name_label_or_host_name() {
    let st = status();
    let r = resolve(&st, "nas").unwrap();
    assert_eq!(
        (r.ip.as_str(), r.name.as_str(), r.online),
        ("100.64.0.5", "nas", Some(true))
    );
    assert_eq!(
        resolve(&st, "NAS.tail1234.ts.net").unwrap().ip,
        "100.64.0.5"
    );
    assert_eq!(resolve(&st, "100.64.0.5").unwrap().name, "nas");
    // "garden" is one machine's MagicDNS label and another's host name: the DNS label wins.
    let r = resolve(&st, "garden").unwrap();
    assert_eq!(
        (r.ip.as_str(), r.online, r.last_seen.as_deref()),
        (
            "100.64.0.9",
            Some(false),
            Some("last seen 2026-10-19 21:04 UTC")
        )
    );
    assert_eq!(resolve(&st, "raspberrypi").unwrap().ip, "100.64.0.9");
    // An IP outside the peer list is pinged as is (subnet routes).
    let r = resolve(&st, "192.168.1.20").unwrap();
    assert_eq!((r.name.as_str(), r.online), ("192.168.1.20", None));
    assert!(
        resolve(&st, "router")
            .unwrap_err()
            .contains("has no Tailscale IP")
    );
    assert!(
        resolve(&st, "printer")
            .unwrap_err()
            .starts_with("No peer named \"printer\"")
    );
    assert!(
        resolve(&st, "vole-host")
            .unwrap_err()
            .contains("this host itself")
    );
    assert!(
        resolve(&st, "100.64.0.1")
            .unwrap_err()
            .contains("this host itself")
    );
    assert_eq!(
        resolve(&Status::default(), "100.64.0.5").unwrap().online,
        None
    );
}

#[test]
fn backend_states_explain_themselves() {
    assert_eq!(backend_problem("Running"), None);
    assert_eq!(backend_problem("Starting"), None);
    assert!(
        backend_problem("NeedsLogin")
            .unwrap()
            .contains("logged out")
    );
    assert!(
        backend_problem("NeedsMachineAuth")
            .unwrap()
            .contains("approve")
    );
    assert!(backend_problem("Stopped").unwrap().contains("stopped"));
    assert!(backend_problem("").unwrap().contains("no state"));
}

fn pong(json: &str, kind: PingType) -> CheckOutcome {
    let result: PingResult = serde_json::from_str(json).unwrap();
    let peer = Resolved {
        ip: "100.64.0.5".into(),
        name: "nas".into(),
        online: Some(false),
        last_seen: Some("last seen 2026-10-19 21:04 UTC".into()),
    };
    ping_outcome(&result, &peer, kind, 99)
}

#[test]
fn ping_answers_map_to_outcomes() {
    let derp = pong(
        r#"{"IP":"100.64.0.5","NodeIP":"100.64.0.5","NodeName":"nas.tail1234.ts.net.","Err":"","LatencySeconds":0.0234,"Endpoint":"","PeerRelay":"","DERPRegionID":4,"DERPRegionCode":"fra"}"#,
        PingType::Disco,
    );
    assert_eq!(
        (derp.status, derp.response_time_ms),
        (HeartbeatStatus::Up, Some(23))
    );
    assert_eq!(
        derp.message.as_deref(),
        Some("pong from nas (100.64.0.5) via DERP(fra) in 23 ms")
    );

    let direct = pong(
        r#"{"NodeIP":"100.64.0.5","NodeName":"nas","LatencySeconds":0.0031,"Endpoint":"192.0.2.5:41641","DERPRegionID":0}"#,
        PingType::Disco,
    );
    assert_eq!(
        direct.message.as_deref(),
        Some("pong from nas (100.64.0.5) direct via 192.0.2.5:41641 in 3 ms")
    );
    let relay = pong(
        r#"{"NodeIP":"100.64.0.5","LatencySeconds":0.009,"PeerRelay":"10.0.0.1:7777:vni:1"}"#,
        PingType::Disco,
    );
    assert_eq!(
        relay.message.as_deref(),
        Some("pong from nas (100.64.0.5) via peer relay 10.0.0.1:7777:vni:1 in 9 ms")
    );
    let tsmp = pong(
        r#"{"NodeIP":"100.64.0.5","NodeName":"nas","LatencySeconds":0.004,"PeerAPIPort":12345}"#,
        PingType::Tsmp,
    );
    assert_eq!(
        tsmp.message.as_deref(),
        Some("pong from nas (100.64.0.5) via TSMP in 4 ms")
    );
    let unmeasured = pong(
        r#"{"NodeIP":"100.64.0.5","LatencySeconds":0}"#,
        PingType::Icmp,
    );
    assert_eq!(unmeasured.response_time_ms, Some(99));

    let failed = pong(
        r#"{"IP":"100.64.0.5","Err":"no matching peer","LatencySeconds":0}"#,
        PingType::Disco,
    );
    assert_eq!(failed.status, HeartbeatStatus::Down);
    assert_eq!(
        failed.message.as_deref(),
        Some(
            "Ping to nas (100.64.0.5) failed: no matching peer; Tailscale shows it offline (last seen 2026-10-19 21:04 UTC)"
        )
    );
    let local = pong(
        r#"{"IP":"100.64.0.1","NodeIP":"100.64.0.1","Err":"is local Tailscale IP","IsLocalIP":true}"#,
        PingType::Disco,
    );
    assert!(local.message.unwrap().contains("this host itself"));
}

#[test]
fn http_answers_are_unframed() {
    let plain = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
    assert_eq!(parse_http(plain).unwrap(), (200, b"{}".to_vec()));
    let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3;ext=1\r\n:1}\r\n0\r\n\r\n";
    assert_eq!(parse_http(chunked).unwrap(), (200, b"{\"a\":1}".to_vec()));
    let error = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\r\n{\"error\":\"no such peer\"}\n";
    let (code, body) = parse_http(error).unwrap();
    assert_eq!(code, 500);
    assert_eq!(
        api_error("ping", code, &body),
        "tailscaled refused the ping (HTTP 500): no such peer"
    );
    assert!(
        api_error("status request", 403, b"status access denied\n").ends_with(
            "status access denied (access denied: the socket's owner decides who may use it)"
        )
    );
    assert!(
        parse_http(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{}")
            .unwrap_err()
            .contains("cut short")
    );
    assert!(
        parse_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{}")
            .unwrap_err()
            .contains("cut short")
    );
    assert!(parse_http(b"SSH-2.0-OpenSSH\r\n\r\n").is_err());
}

#[tokio::test]
async fn requests_carry_the_localapi_host_and_close() {
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let serve = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        let n = server.read(&mut buf).await.unwrap();
        let head = String::from_utf8(buf[..n].to_vec()).unwrap();
        server
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n")
            .await
            .unwrap();
        server.shutdown().await.unwrap();
        head
    });
    let answer = request(client, "POST", "/localapi/v0/ping?ip=100.64.0.5&type=disco")
        .await
        .unwrap();
    assert_eq!(answer, (200, b"{}".to_vec()));
    let head = serve.await.unwrap();
    assert!(
        head.starts_with("POST /localapi/v0/ping?ip=100.64.0.5&type=disco HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(
        head.contains("\r\nHost: local-tailscaled.sock\r\n")
            && head.contains("\r\nConnection: close\r\n")
    );
    assert!(
        !head.to_ascii_lowercase().contains("origin:")
            && !head.to_ascii_lowercase().contains("referer:")
    );
}

/// A fake tailscaled on a unix socket: answers the status with `status`
/// and pings with `ping` (`None`: never answers).
#[cfg(unix)]
async fn fake_tailscaled(
    dir: &std::path::Path,
    status: &'static str,
    ping: Option<(u16, &'static str)>,
) -> String {
    let path = dir.join("tailscaled.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                assert!(head.contains("Host: local-tailscaled.sock"));
                let (code, body) = if head.starts_with("GET /localapi/v0/status ") {
                    (200, status)
                } else if head.starts_with("POST /localapi/v0/ping?ip=") {
                    match ping {
                        Some(answer) => answer,
                        None => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            return;
                        }
                    }
                } else {
                    (404, "not found\n")
                };
                // Go's server sends larger bodies chunked.
                let reply = format!(
                    "HTTP/1.1 {code} X\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes()).await;
            });
        }
    });
    path.to_string_lossy().into_owned()
}

#[cfg(unix)]
fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "vv-ts-{name}-{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(unix)]
#[tokio::test]
async fn pings_through_a_unix_socket() {
    let dir = temp_dir("ok");
    let pong = r#"{"IP":"100.64.0.5","NodeIP":"100.64.0.5","NodeName":"nas.tail1234.ts.net.","LatencySeconds":0.012,"Endpoint":"192.0.2.5:41641"}"#;
    let socket = fake_tailscaled(&dir, STATUS, Some((200, pong))).await;
    let out = check_with(
        &socket,
        &Target::parse("nas").unwrap(),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert_eq!(
        out.message.as_deref(),
        Some("pong from nas (100.64.0.5) direct via 192.0.2.5:41641 in 12 ms")
    );

    let out = check_with(
        &socket,
        &Target::parse("printer").unwrap(),
        Duration::from_secs(2),
    )
    .await;
    assert!(out.message.unwrap().starts_with("No peer named"));
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn socket_failures_and_logged_out_daemons_are_down() {
    let dir = temp_dir("fail");
    let missing = dir.join("nope.sock");
    let out = check_with(
        &missing.to_string_lossy(),
        &Target::parse("nas").unwrap(),
        Duration::from_secs(1),
    )
    .await;
    assert!(out.message.unwrap().contains("doesn't exist"));

    let logged_out = fake_tailscaled(
        &dir,
        r#"{"BackendState":"NeedsLogin","Self":null,"Peer":null}"#,
        None,
    )
    .await;
    let out = check_with(
        &logged_out,
        &Target::parse("nas").unwrap(),
        Duration::from_secs(1),
    )
    .await;
    assert!(out.message.unwrap().contains("logged out (NeedsLogin)"));
    let _ = std::fs::remove_dir_all(&dir);

    let dir = temp_dir("silent");
    let silent = fake_tailscaled(&dir, STATUS, None).await;
    let out = check_with(
        &silent,
        &Target::parse("garden").unwrap(),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert_eq!(
        out.message.as_deref(),
        Some(
            "No pong from garden (100.64.0.9) within 1s; Tailscale shows it offline (last seen 2026-10-19 21:04 UTC)"
        )
    );
    let _ = std::fs::remove_dir_all(&dir);

    let dir = temp_dir("err");
    let failing = fake_tailscaled(
        &dir,
        STATUS,
        Some((500, r#"{"error":"tailscaled is shutting down"}"#)),
    )
    .await;
    let out = check_with(
        &failing,
        &Target::parse("100.64.0.5?type=tsmp").unwrap(),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(
        out.message.as_deref(),
        Some("tailscaled refused the ping (HTTP 500): tailscaled is shutting down")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Against a real tailscaled: `VV_TEST_TAILSCALE=/path/tailscaled.sock[,peer]`
/// (a logged-out daemon is fine: it shows the NeedsLogin path).
#[cfg(unix)]
#[tokio::test]
#[ignore = "needs a tailscaled socket (VV_TEST_TAILSCALE)"]
async fn live_tailscaled() {
    let spec = std::env::var("VV_TEST_TAILSCALE").expect("VV_TEST_TAILSCALE");
    let (socket, peer) = spec
        .split_once(',')
        .unwrap_or((spec.as_str(), "100.100.100.100"));
    let (code, body) = call(socket, "GET", "/localapi/v0/status").await.unwrap();
    let status: Status = serde_json::from_slice(&body).unwrap();
    println!(
        "status HTTP {code}: BackendState {:?}, {} peers",
        status.backend_state,
        status.peer.as_ref().map_or(0, HashMap::len)
    );
    let out = check_with(
        socket,
        &Target::parse(peer).unwrap(),
        Duration::from_secs(5),
    )
    .await;
    println!(
        "{:?} {:?} {:?}",
        out.status, out.response_time_ms, out.message
    );
    assert!(out.message.is_some());
}
