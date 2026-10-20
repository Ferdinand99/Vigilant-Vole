use std::net::SocketAddr;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

use super::*;
use crate::db::models::{HeartbeatStatus, MonitorType};
use crate::monitor::checkers::test_support::monitor;

fn game(target: &str) -> Monitor {
    monitor(MonitorType::GameServer, target)
}

async fn run(m: &Monitor) -> CheckOutcome {
    crate::monitor::checkers::run_check(m).await
}

#[track_caller]
fn assert_up(out: &CheckOutcome) -> String {
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    out.message.clone().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Targets and text
// ---------------------------------------------------------------------------

#[test]
fn targets_pick_the_game_and_its_default_port() {
    for (target, game, host, port, explicit) in [
        ("minecraft://mc.example.com", Game::Minecraft, "mc.example.com", 25565, false),
        ("Minecraft://mc.example.com:25570", Game::Minecraft, "mc.example.com", 25570, true),
        ("bedrock://10.0.0.5", Game::Bedrock, "10.0.0.5", 19132, false),
        ("satisfactory://factory.lan:15777", Game::Satisfactory, "factory.lan", 15777, true),
        ("fivem://[2001:db8::7]", Game::FiveM, "2001:db8::7", 30120, false),
        (" mumble://voice.example.org ", Game::Mumble, "voice.example.org", 64738, false),
    ] {
        let t = parse_target(target).unwrap();
        assert_eq!((t.game, t.host.as_str(), t.port, t.explicit_port), (game, host, port, explicit), "{target}");
    }
}

#[test]
fn bad_targets_are_explained() {
    assert!(parse_target("").unwrap_err().contains("empty"));
    assert!(parse_target("mc.example.com:25565").unwrap_err().contains("minecraft://"));
    let err = parse_target("csgo://1.2.3.4").unwrap_err();
    assert!(err.contains("Steam type") && err.contains("csgo"), "{err}");
    assert!(parse_target("minecraft://mc.example.com/world").unwrap_err().contains("no path"));
    assert!(parse_target("minecraft://op:pw@mc.example.com").unwrap_err().contains("credentials"));
    assert!(parse_target("minecraft://").is_err());
    let ep = endpoint("fivem://rp.example.com").unwrap();
    assert_eq!((ep.host.as_str(), ep.port, ep.tls), ("rp.example.com", 30120, false));
}

#[test]
fn section_codes_and_whitespace_are_removed() {
    assert_eq!(strip_section_codes("§aA §lVole§r  SMP\n§7Since 2011"), "A Vole SMP Since 2011");
    assert_eq!(strip_section_codes("trailing §"), "trailing");
    assert_eq!(fivem::strip_caret_codes("^1Red ^7Vole^9 RP ^ x"), "Red Vole RP ^ x");
}

#[test]
fn chat_components_are_walked_in_order() {
    let motd = json!({
        "text": "",
        "extra": [
            {"text": "A ", "color": "gold"},
            {"text": "Vole", "bold": true, "extra": [{"text": " S"}, {"extra": ["M", {"text": "P"}]}]},
            "\n§7welcome",
            {"translate": "ignored"}
        ]
    });
    assert_eq!(minecraft::chat_text(&motd), "A Vole SMP welcome");
    assert_eq!(minecraft::chat_text(&json!("§6Plain §lMOTD")), "Plain MOTD");
    assert_eq!(minecraft::chat_text(&json!([{"text": "a"}, "b", 3])), "ab3");
    // Absurd nesting is cut off instead of overflowing the stack.
    let mut deep = json!({"text": "x"});
    for _ in 0..200 {
        deep = json!({"extra": [deep]});
    }
    assert_eq!(minecraft::chat_text(&deep), "");
}

#[test]
fn java_status_json_is_parsed() {
    let s = minecraft::parse_status(r#"{"version":{"name":"1.21.4","protocol":769},"players":{"max":100,"online":12,"sample":[]},"description":{"text":"§aA Vole SMP"},"favicon":"data:image/png;base64,AA=="}"#.as_bytes()).unwrap();
    assert_eq!((s.product.as_str(), s.players, s.name.as_str()), ("Minecraft 1.21.4", Some((12, 100)), "A Vole SMP"));
    assert_eq!(describe(Game::Minecraft, &s), "Minecraft 1.21.4 · 12/100 players · A Vole SMP");
    let proxy = minecraft::parse_status(r#"{"version":{"name":"§cVelocity 3.4.0"},"description":"hub"}"#.as_bytes()).unwrap();
    assert_eq!((proxy.product.as_str(), proxy.players), ("Velocity 3.4.0", None));
    assert!(minecraft::parse_status(b"[1]").is_err());
    assert!(minecraft::parse_status(b"not json").unwrap_err().contains("JSON"));
}

#[test]
fn varints_round_trip() {
    for (v, bytes) in [(0u32, vec![0x00]), (1, vec![0x01]), (127, vec![0x7F]), (128, vec![0x80, 0x01]), (25565, vec![0xDD, 0xC7, 0x01]), (u32::MAX, vec![0xFF, 0xFF, 0xFF, 0xFF, 0x0F])] {
        let mut out = Vec::new();
        minecraft::write_varint(v, &mut out);
        assert_eq!(out, bytes, "{v}");
        let mut slice = &out[..];
        assert_eq!(minecraft::take_varint(&mut slice), Ok(v));
        assert!(slice.is_empty());
    }
    assert!(minecraft::take_varint(&mut &[0x80u8, 0x80][..]).is_err());
    assert!(minecraft::take_varint(&mut &[0xFFu8; 6][..]).is_err());
}

#[test]
fn status_request_matches_the_protocol() {
    let req = minecraft::status_request("localhost", 25565);
    // Length 19, id 0, protocol -1, "localhost", port 25565, next state 1; then the status request.
    let mut want = vec![19, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 9];
    want.extend_from_slice(b"localhost");
    want.extend_from_slice(&[0x63, 0xDD, 0x01, 0x01, 0x00]);
    assert_eq!(req, want);
}

#[test]
fn srv_answers_pick_priority_then_weight() {
    let rec = |p, w, h: &str, port| (p, w, h.to_string(), port);
    assert_eq!(minecraft::pick_srv(&[]), None);
    assert_eq!(minecraft::pick_srv(&[rec(0, 0, ".", 0)]), None, "\".\" means no service");
    let picked = minecraft::pick_srv(&[rec(10, 5, "b.example.", 25570), rec(5, 1, "a.example.", 25566), rec(5, 9, "c.example.", 25567)]);
    assert_eq!(picked, Some(("c.example".into(), 25567)));
}

#[test]
fn describe_skips_what_is_missing() {
    let s = Status { product: "Mumble 1.5.735".into(), players: Some((3, 50)), name: String::new(), detail: None };
    assert_eq!(describe(Game::Mumble, &s), "Mumble 1.5.735 · 3/50 users");
    let s = Status { product: "Satisfactory build 1".into(), players: None, name: "x".repeat(100), detail: Some("playing".into()) };
    let msg = describe(Game::Satisfactory, &s);
    assert_eq!(msg, format!("Satisfactory build 1 · playing · {}…", "x".repeat(80)));
}

// ---------------------------------------------------------------------------
// Minecraft Java against a fake server
// ---------------------------------------------------------------------------

const JAVA_STATUS: &str = r#"{"version":{"name":"1.21.4","protocol":769},"players":{"max":100,"online":12},"description":{"text":"","extra":[{"text":"A Vole ","color":"green"},{"text":"SMP"}]}}"#;

fn status_packet(json: &str) -> Vec<u8> {
    let mut body = vec![0x00];
    minecraft::write_varint(json.len() as u32, &mut body);
    body.extend_from_slice(json.as_bytes());
    let mut out = Vec::new();
    minecraft::write_varint(body.len() as u32, &mut out);
    out.extend_from_slice(&body);
    out
}

/// A Java server: checks the handshake, answers the status request and
/// (when `pong`) the ping. `reply` replaces the status packet when set.
async fn java_server(pong: bool, reply: Option<Vec<u8>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let reply = reply.clone();
            tokio::spawn(async move {
                let want = minecraft::status_request("127.0.0.1", addr.port());
                let mut got = vec![0u8; want.len()];
                if tcp.read_exact(&mut got).await.is_err() || got != want {
                    return;
                }
                let _ = tcp.write_all(&reply.unwrap_or_else(|| status_packet(JAVA_STATUS))).await;
                let mut ping = [0u8; 10];
                if pong && tcp.read_exact(&mut ping).await.is_ok() && ping[..2] == [9, 0x01] {
                    let _ = tcp.write_all(&ping).await;
                }
                let mut rest = [0u8; 64];
                let _ = tcp.read(&mut rest).await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn java_server_list_ping() {
    let addr = java_server(true, None).await;
    let out = run(&game(&format!("minecraft://{addr}"))).await;
    assert_eq!(assert_up(&out), "Minecraft 1.21.4 · 12/100 players · A Vole SMP");
    assert!(out.response_time_ms.is_some());
}

#[tokio::test]
async fn java_without_pong_uses_the_status_round_trip() {
    let addr = java_server(false, None).await;
    let mut m = game(&format!("minecraft://{addr}"));
    m.timeout_seconds = 2;
    let start = Instant::now();
    let out = run(&m).await;
    assert_eq!(assert_up(&out), "Minecraft 1.21.4 · 12/100 players · A Vole SMP");
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn java_keyword_matches_the_motd() {
    let addr = java_server(true, None).await;
    let mut m = game(&format!("minecraft://{addr}"));
    m.keyword = Some("Vole SMP".into());
    assert_up(&run(&m).await);
    m.keyword = Some("Creative".into());
    let out = run(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(out.message.unwrap().starts_with("Server name doesn't contain \"Creative\": Minecraft 1.21.4"));
    m.keyword_invert = true;
    assert_up(&run(&m).await);
    // The keyword looks at the name, not the version or the player count.
    m.keyword = Some("1.21".into());
    m.keyword_invert = false;
    assert_eq!(run(&m).await.status, HeartbeatStatus::Down);
}

#[tokio::test]
async fn java_failures_are_explained() {
    let legacy = java_server(true, Some(vec![0xFF, 0x00, 0x03, 0x00, 0xA7])).await;
    let out = run(&game(&format!("minecraft://{legacy}"))).await;
    assert!(out.message.unwrap().contains("pre-1.7"));

    let wrong = java_server(true, Some(vec![0x02, 0x05, 0x00])).await;
    let out = run(&game(&format!("minecraft://{wrong}"))).await;
    assert!(out.message.unwrap().contains("packet 0x05"));

    let http = crate::monitor::checkers::test_support::foreign_server(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
    let out = run(&game(&format!("minecraft://{http}"))).await;
    assert_eq!(out.status, HeartbeatStatus::Down, "{:?}", out.message);

    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let out = run(&game(&format!("minecraft://{closed}"))).await;
    assert!(out.message.unwrap().contains("failed"));
}

// ---------------------------------------------------------------------------
// UDP games against fake servers
// ---------------------------------------------------------------------------

/// A UDP server that answers each request with `answer(request)` (no reply
/// when it returns empty).
async fn udp_server(answer: impl Fn(&[u8]) -> Vec<Vec<u8>> + Send + 'static) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, peer)) = socket.recv_from(&mut buf).await {
            for reply in answer(&buf[..n]) {
                let _ = socket.send_to(&reply, peer).await;
            }
        }
    });
    addr
}

fn bedrock_pong(nonce: &[u8], status: &str) -> Vec<u8> {
    let mut out = vec![0x1C];
    out.extend_from_slice(nonce);
    out.extend_from_slice(&0x1234_5678_u64.to_be_bytes());
    out.extend_from_slice(&bedrock::MAGIC);
    out.extend_from_slice(&(status.len() as u16).to_be_bytes());
    out.extend_from_slice(status.as_bytes());
    out
}

#[tokio::test]
async fn bedrock_unconnected_ping() {
    let addr = udp_server(|req| {
        assert_eq!((req.len(), req[0], &req[9..25]), (33, 0x01, &bedrock::MAGIC[..]));
        // A stale pong for some other ping comes first and is ignored.
        vec![
            bedrock_pong(&[0; 8], "MCPE;stale;1;1;1;1;"),
            bedrock_pong(&req[1..9], "MCPE;§bVole §lBedrock;766;1.21.50;3;10;13253860892328930865;Bedrock level;Survival;1;19132;19133;"),
        ]
    })
    .await;
    let out = run(&game(&format!("bedrock://{addr}"))).await;
    assert_eq!(assert_up(&out), "Bedrock 1.21.50 · 3/10 players · Vole Bedrock");
}

#[test]
fn bedrock_replies_are_checked() {
    let nonce = 7u64;
    let ok = bedrock_pong(&nonce.to_be_bytes(), "MCEE;Class;1;1.21.0;x;y");
    let Verdict::Done(Ok(s)) = bedrock::parse_pong(&ok, nonce) else { panic!("expected a status") };
    assert_eq!((s.product.as_str(), s.players, s.name.as_str()), ("Minecraft Education 1.21.0", None, "Class"));
    assert!(matches!(bedrock::parse_pong(&ok, 8), Verdict::Ignore));
    let mut bad_magic = ok.clone();
    bad_magic[20] ^= 1;
    assert!(matches!(bedrock::parse_pong(&bad_magic, nonce), Verdict::Done(Err(_))));
    assert!(matches!(bedrock::parse_pong(&ok[..40], nonce), Verdict::Done(Err(_))));
    assert!(matches!(bedrock::parse_pong(b"\x1c short", nonce), Verdict::Done(Err(_))));
    assert!(bedrock::parse_status("MCPE;too;few").is_err());
}

fn satisfactory_reply(req: &[u8], state: u8, name: &str) -> Vec<u8> {
    let mut out = vec![0xD5, 0xF6, 0x01, 0x01];
    out.extend_from_slice(&req[4..12]);
    out.push(state);
    out.extend_from_slice(&365_306u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.push(2); // two sub-states
    out.extend_from_slice(&[0, 1, 0, 1, 7, 0]);
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0x01);
    out
}

#[tokio::test]
async fn satisfactory_lightweight_query() {
    let addr = udp_server(|req| {
        assert_eq!((req.len(), &req[..4], req[12]), (13, &[0xD5, 0xF6, 0, 1][..], 1));
        vec![satisfactory_reply(req, 3, "Vole Works")]
    })
    .await;
    let out = run(&game(&format!("satisfactory://{addr}"))).await;
    assert_eq!(assert_up(&out), "Satisfactory build 365306 · playing · Vole Works");

    let offline = udp_server(|req| vec![satisfactory_reply(req, 0, "x")]).await;
    let out = run(&game(&format!("satisfactory://{offline}"))).await;
    assert!(out.message.unwrap().contains("offline"));

    let truncated = udp_server(|req| vec![satisfactory_reply(req, 3, "Vole Works")[..25].to_vec()]).await;
    let out = run(&game(&format!("satisfactory://{truncated}"))).await;
    assert!(out.message.unwrap().contains("Truncated"));
}

/// The HTTPS API of a Satisfactory server on `port`: a 200 reply for the
/// token "good", 401 otherwise.
async fn satisfactory_api(port: u16) {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let acceptor = crate::monitor::checkers::test_support::tls_acceptor();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                while !req.ends_with(b"}") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).to_ascii_lowercase();
                assert!(text.starts_with("post /api/v1 ") && text.contains(r#""function":"queryserverstate""#), "{text}");
                let body = r#"{"data":{"serverGameState":{"activeSessionName":"Vole","numConnectedPlayers":2,"playerLimit":4,"techTier":3}}}"#;
                let reply = if text.contains("authorization: bearer good") {
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                } else {
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                };
                let _ = tls.write_all(reply.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
}

#[tokio::test]
async fn satisfactory_api_token_adds_the_player_count() {
    let addr = udp_server(|req| vec![satisfactory_reply(req, 3, "Vole Works")]).await;
    satisfactory_api(addr.port()).await;
    let mut m = game(&format!("satisfactory://{addr}"));
    // Generous: a loaded test machine makes the TLS handshake slow.
    m.timeout_seconds = 20;
    m.secret = Some("good".into());
    assert_eq!(assert_up(&run(&m).await), "Satisfactory build 365306 · playing · 2/4 players · Vole Works");
    m.secret = Some("wrong".into());
    let msg = assert_up(&run(&m).await);
    assert!(msg.contains("players unknown (HTTPS API: HTTP 401 (check the API token))"), "{msg}");
}

#[tokio::test]
async fn mumble_ping() {
    let addr = udp_server(|req| {
        assert_eq!((req.len(), &req[..4]), (12, &[0u8; 4][..]));
        let mut out = vec![0, 1, 5, 7];
        out.extend_from_slice(&req[4..12]);
        out.extend_from_slice(&3u32.to_be_bytes());
        out.extend_from_slice(&100u32.to_be_bytes());
        out.extend_from_slice(&558_000u32.to_be_bytes());
        vec![out]
    })
    .await;
    let out = run(&game(&format!("mumble://{addr}"))).await;
    assert_eq!(assert_up(&out), "Mumble 1.5.7 · 3/100 users");
    assert!(matches!(mumble::parse_pong(&[0; 10], 1), Verdict::Done(Err(_))));
    assert!(matches!(mumble::parse_pong(&[0; 24], 1), Verdict::Ignore));
}

#[tokio::test]
async fn lost_datagrams_are_sent_again() {
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = seen.clone();
    let addr = udp_server(move |req| {
        // Drop the first request, as a congested link would.
        if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Vec::new();
        }
        let mut out = vec![0, 1, 4, 0];
        out.extend_from_slice(&req[4..12]);
        out.extend_from_slice(&[0; 12]);
        vec![out]
    })
    .await;
    let out = run(&game(&format!("mumble://{addr}"))).await;
    assert_eq!(assert_up(&out), "Mumble 1.4.0 · 0/0 users");
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn silent_udp_servers_time_out_with_a_message() {
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut m = game(&format!("bedrock://{}", silent.local_addr().unwrap()));
    m.timeout_seconds = 1;
    let start = Instant::now();
    let out = run(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(out.message.unwrap().contains("No reply"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// FiveM against a fake FXServer
// ---------------------------------------------------------------------------

async fn fxserver(with_dynamic: bool) -> SocketAddr {
    use axum::routing::get;
    let info = json!({
        "server": "FXServer-master SERVER v1.0.0.7290 win32",
        "vars": {"sv_projectName": "^2Vole ^7RP", "sv_maxClients": "48", "gamename": "gta5"},
        "version": 1_234_567,
        "resources": ["mapmanager", "chat"]
    });
    let mut app = axum::Router::new()
        .route("/info.json", get(move || async move { axum::Json(info) }))
        .route("/players.json", get(|| async { axum::Json(json!([{"name": "a"}, {"name": "b"}])) }));
    if with_dynamic {
        app = app.route("/dynamic.json", get(|| async { axum::Json(json!({"clients": 12, "hostname": "^1Vole^7 Roleplay", "sv_maxclients": "64"})) }));
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr
}

#[tokio::test]
async fn fivem_info_and_dynamic() {
    let addr = fxserver(true).await;
    assert_eq!(assert_up(&run(&game(&format!("fivem://{addr}"))).await), "FiveM build 7290 · 12/64 players · Vole Roleplay");
    let addr = fxserver(false).await;
    assert_eq!(assert_up(&run(&game(&format!("fivem://{addr}"))).await), "FiveM build 7290 · 2/48 players · Vole RP");
}

#[tokio::test]
async fn fivem_needs_an_fxserver() {
    let app = axum::Router::new().route("/", axum::routing::get(|| async { "hello" }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let out = run(&game(&format!("fivem://{addr}"))).await;
    assert_eq!(out.message.as_deref(), Some("/info.json: HTTP 404"));
}

#[test]
fn fivem_products() {
    assert_eq!(fivem::product(&json!({"vars": {"gamename": "rdr3"}, "server": "FXServer-master v1.0.0.1491 linux"})).unwrap(), "RedM build 1491");
    assert_eq!(fivem::product(&json!({"server": "something else"})).unwrap(), "FiveM");
    assert!(fivem::product(&json!({"hello": 1})).is_err());
    assert_eq!(satisfactory::players_of(&json!({"data": {"serverGameState": {"numConnectedPlayers": 1, "playerLimit": 4}}})), Some((1, 4)));
}

// ---------------------------------------------------------------------------
// Public servers (run with --ignored; needs internet)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs internet"]
async fn live_public_servers() {
    for target in ["minecraft://mc.hypixel.net", "minecraft://play.cubecraft.net:25565", "bedrock://play.cubecraft.net", "bedrock://geo.hivebedrock.network"] {
        let mut m = game(target);
        m.timeout_seconds = 10;
        let out = run(&m).await;
        println!("{target}: {:?} {:?} ms {:?}", out.status, out.response_time_ms, out.message);
        assert_eq!(out.status, HeartbeatStatus::Up, "{target}: {:?}", out.message);
    }
}
