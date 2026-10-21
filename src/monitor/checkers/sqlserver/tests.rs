use super::*;
use crate::db::models::HeartbeatStatus;
use crate::monitor::checkers::test_support::{
    foreign_server, monitor, tls_acceptor, tls_acceptor_alpn,
};

/// How the fake server treats encryption.
#[derive(Clone, Copy, PartialEq)]
enum Server {
    /// No certificate (`ENCRYPT_NOT_SUP`).
    Plain,
    /// Encrypts when asked (`ENCRYPT_OFF`), and takes TDS 8.0 strict connections.
    Optional,
    /// Force Encryption (`ENCRYPT_REQ`).
    Forced,
}

fn server_prelogin(encryption: u8) -> Vec<u8> {
    prelogin_options(&[
        (PL_VERSION, vec![16, 0, 0x03, 0xE8, 0, 0]),
        (PL_ENCRYPTION, vec![encryption]),
        (PL_INSTOPT, vec![0]),
        (PL_THREADID, Vec::new()),
        (PL_MARS, vec![0]),
    ])
}

fn deobfuscate(b: &[u8]) -> Vec<u8> {
    b.iter().map(|x| (x ^ 0xA5).rotate_left(4)).collect()
}

/// The `index`-th string of LOGIN7's offset table (raw bytes).
fn login_bytes(login: &[u8], index: usize) -> Vec<u8> {
    let at = 36 + 4 * index;
    let offset = usize::from(u16::from_le_bytes([login[at], login[at + 1]]));
    let chars = usize::from(u16::from_le_bytes([login[at + 2], login[at + 3]]));
    login[offset..offset + chars * 2].to_vec()
}

fn text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    String::from_utf16_lossy(&units)
}

fn done(status: u16, rows: u64) -> Vec<u8> {
    let mut b = vec![0xFD];
    b.extend_from_slice(&status.to_le_bytes());
    b.extend_from_slice(&0xC1u16.to_le_bytes());
    b.extend_from_slice(&rows.to_le_bytes());
    b
}

fn error_token(number: i32, message: &str) -> Vec<u8> {
    let mut t = number.to_le_bytes().to_vec();
    t.extend_from_slice(&[1, 14]);
    t.extend_from_slice(&(message.encode_utf16().count() as u16).to_le_bytes());
    t.extend(utf16(message));
    t.push(4);
    t.extend(utf16("test"));
    t.push(0);
    t.extend_from_slice(&1i32.to_le_bytes());
    let mut b = vec![0xAA];
    b.extend_from_slice(&(t.len() as u16).to_le_bytes());
    b.extend(t);
    b
}

fn login_ack() -> Vec<u8> {
    // ENVCHANGE (database), LOGINACK, DONE.
    let mut env = vec![1, 6];
    env.extend(utf16("master"));
    env.push(0);
    let mut b = vec![0xE3];
    b.extend_from_slice(&(env.len() as u16).to_le_bytes());
    b.extend(env);
    let mut ack = vec![1];
    ack.extend_from_slice(&TDS_74.to_be_bytes());
    ack.push(20);
    ack.extend(utf16("Microsoft SQL Server"));
    ack.extend_from_slice(&[16, 0, 0x03, 0xE8]);
    b.push(0xAD);
    b.extend_from_slice(&(ack.len() as u16).to_le_bytes());
    b.extend(ack);
    b.extend(done(0, 0));
    b
}

fn select_one_reply() -> Vec<u8> {
    // COLMETADATA: one nameless INTN(4); ROW: 1; DONE with a row count.
    let mut b = vec![0x81, 1, 0, 0, 0, 0, 0, 0x20, 0, 0x26, 4, 0];
    b.extend_from_slice(&[0xD1, 4, 1, 0, 0, 0]);
    b.extend(done(0x10, 1));
    b
}

/// LOGIN7 for user `vole`, password `pw`, database `app` (or none).
async fn after_login(mut s: Stream, login: Vec<u8>) {
    let (user, password, database) = (
        text(&login_bytes(&login, 1)),
        text(&deobfuscate(&login_bytes(&login, 2))),
        text(&login_bytes(&login, 8)),
    );
    if user != "vole" || password != "pw" {
        let mut reply = error_token(18456, &format!("Login failed for user '{user}'."));
        reply.extend(done(2, 0));
        let _ = send(&mut s, REPLY, &reply).await;
        return;
    }
    if !database.is_empty() && database != "app" {
        let mut reply = error_token(
            4060,
            &format!(
                "Cannot open database \"{database}\" requested by the login. The login failed."
            ),
        );
        reply.extend(done(2, 0));
        let _ = send(&mut s, REPLY, &reply).await;
        return;
    }
    send(&mut s, REPLY, &login_ack()).await.unwrap();
    while let Ok(batch) = read_message(&mut s, SQL_BATCH, false).await {
        assert_eq!(text(&batch[22..]), "SELECT 1");
        send(&mut s, REPLY, &select_one_reply()).await.unwrap();
    }
}

async fn login_then_session(mut s: Stream) {
    if let Ok(login) = read_message(&mut s, LOGIN7, false).await {
        after_login(s, login).await;
    }
}

async fn serve_conn(mut tcp: TcpStream, server: Server) {
    let mut first = [0u8; 1];
    if tcp.peek(&mut first).await.is_err() {
        return;
    }
    if first[0] == 0x16 {
        // TDS 8.0: TLS first, then PRELOGIN inside it.
        let Ok(tls) = tls_acceptor_alpn(&[ALPN_TDS8]).accept(tcp).await else {
            return;
        };
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(ALPN_TDS8));
        let mut s: Stream = Box::new(tls);
        let Ok(_) = read_message(&mut s, PRELOGIN, false).await else {
            return;
        };
        send(&mut s, REPLY, &server_prelogin(ENCRYPT_NOT_SUP))
            .await
            .unwrap();
        login_then_session(s).await;
        return;
    }
    let Ok(body) = read_message(&mut tcp, PRELOGIN, false).await else {
        return;
    };
    let client = parse_prelogin(&body).unwrap().encryption;
    let answer = match (server, client) {
        (Server::Plain, _) => ENCRYPT_NOT_SUP,
        (Server::Optional, ENCRYPT_ON) => ENCRYPT_ON,
        (Server::Optional, ENCRYPT_OFF) => ENCRYPT_OFF,
        (Server::Optional, _) => ENCRYPT_NOT_SUP,
        (Server::Forced, _) => ENCRYPT_REQ,
    };
    send(&mut tcp, REPLY, &server_prelogin(answer))
        .await
        .unwrap();
    if answer == ENCRYPT_NOT_SUP {
        return login_then_session(Box::new(tcp)).await;
    }
    if client == ENCRYPT_NOT_SUP {
        return; // forced encryption: the server hangs up
    }
    let Ok(mut tls) = tls_acceptor().accept(PreloginTls::new(tcp)).await else {
        return;
    };
    tls.get_mut().0.handshake = false;
    if answer == ENCRYPT_OFF {
        // Login only: LOGIN7 through TLS, everything after in the clear.
        let Ok(login) = read_message(&mut tls, LOGIN7, false).await else {
            return;
        };
        after_login(Box::new(tls.into_inner().0), login).await;
    } else {
        login_then_session(Box::new(tls)).await;
    }
}

async fn serve(server: Server) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(serve_conn(tcp, server));
        }
    });
    addr
}

fn mssql(target: &str, pass: Option<&str>) -> Monitor {
    let mut m = monitor(MonitorType::SqlServer, target);
    m.basic_auth_user = pass.map(|_| "vole".into());
    m.basic_auth_pass = pass.map(str::to_string);
    m
}

#[tokio::test]
async fn without_a_user_only_prelogin_runs() {
    let addr = serve(Server::Plain).await;
    let out = check(&mssql(&format!("sqlserver://{addr}"), None)).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert_eq!(
        out.message.as_deref(),
        Some("SQL Server 16.0.1000 · pre-login OK (set a user name to check the login)")
    );
    assert!(out.expiry.is_none());
}

#[tokio::test]
async fn login_and_select_one() {
    let addr = serve(Server::Plain).await;
    for target in [
        format!("sqlserver://{addr}"),
        format!("mssql://{addr}/app"),
        format!("{addr}?encrypt=disable"),
    ] {
        let out = check(&mssql(&target, Some("pw"))).await;
        assert_eq!(
            out.status,
            HeartbeatStatus::Up,
            "{target}: {:?}",
            out.message
        );
        assert_eq!(
            out.message.as_deref(),
            Some("SQL Server 16.0.1000 · SELECT 1 OK")
        );
    }
}

#[tokio::test]
async fn login_failures_report_the_server_error() {
    let addr = serve(Server::Plain).await;
    let out = check(&mssql(&format!("sqlserver://{addr}"), Some("nope"))).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert_eq!(
        out.message.as_deref(),
        Some("Server error 18456: Login failed for user 'vole'.")
    );
    let out = check(&mssql(&format!("sqlserver://{addr}/other"), Some("pw"))).await;
    assert!(
        out.message
            .unwrap()
            .starts_with("Server error 4060: Cannot open database \"other\"")
    );
}

#[tokio::test]
async fn tls_inside_tds_is_used_and_verified_only_when_asked() {
    let addr = serve(Server::Optional).await;
    let target = format!("sqlserver://localhost:{}", addr.port());
    // The default and encrypt=true encrypt the session without checking the certificate.
    for t in [
        target.clone(),
        format!("{target}?encrypt=true"),
        format!("{target}?encrypt=true&trustServerCertificate=true"),
    ] {
        let out = check(&mssql(&t, Some("pw"))).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{t}: {:?}", out.message);
        assert_eq!(
            out.message.as_deref(),
            Some("SQL Server 16.0.1000 · SELECT 1 OK · TLS 1.3"),
            "{t}"
        );
        assert!(out.expiry.is_some(), "the certificate's expiry is reported");
    }
    // encrypt=false: only the login packet is encrypted.
    let out = check(&mssql(&format!("{target}?encrypt=false"), Some("pw"))).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert_eq!(
        out.message.as_deref(),
        Some("SQL Server 16.0.1000 · SELECT 1 OK · TLS 1.3 (login only)")
    );
    // Without a user the handshake still runs, for the certificate.
    let out = check(&mssql(&target, None)).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert!(out.message.unwrap().ends_with("· TLS 1.3") && out.expiry.is_some());

    let verify = format!("{target}?encrypt=true&trustServerCertificate=false");
    let out = check(&mssql(&verify, Some("pw"))).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(
        out.message.as_deref().unwrap().contains("not trusted"),
        "{:?}",
        out.message
    );
    assert!(
        out.expiry.is_some(),
        "the certificate is fetched again for its expiry"
    );
    let mut m = mssql(&verify, Some("pw"));
    m.ignore_tls = true;
    assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
}

#[tokio::test]
async fn strict_encryption_speaks_tls_first() {
    let addr = serve(Server::Optional).await;
    let target = format!("sqlserver://localhost:{}?encrypt=strict", addr.port());
    let out = check(&mssql(&target, Some("pw"))).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert_eq!(
        out.message.as_deref(),
        Some("SQL Server 16.0.1000 · SELECT 1 OK · TLS 1.3")
    );
    let out = check(&mssql(
        &format!("{target}&trustServerCertificate=false"),
        Some("pw"),
    ))
    .await;
    assert!(
        out.message.as_deref().unwrap().contains("not trusted"),
        "{:?}",
        out.message
    );
    assert!(out.expiry.is_some());
}

#[tokio::test]
async fn encryption_requirements_on_either_side_are_explained() {
    let forced = serve(Server::Forced).await;
    let out = check(&mssql(
        &format!("sqlserver://localhost:{}?encrypt=disable", forced.port()),
        Some("pw"),
    ))
    .await;
    assert_eq!(
        out.message.as_deref(),
        Some("The server requires encryption (remove encrypt=disable)")
    );
    let out = check(&mssql(
        &format!("sqlserver://localhost:{}?encrypt=false", forced.port()),
        Some("pw"),
    ))
    .await;
    assert_eq!(
        out.message.as_deref(),
        Some("SQL Server 16.0.1000 · SELECT 1 OK · TLS 1.3"),
        "a forced server encrypts everything"
    );

    let plain = serve(Server::Plain).await;
    let out = check(&mssql(
        &format!("sqlserver://{plain}?encrypt=true"),
        Some("pw"),
    ))
    .await;
    assert_eq!(
        out.message.as_deref(),
        Some("The server doesn't support encryption (encrypt=true)")
    );
    let out = check(&mssql(
        &format!("sqlserver://{plain}?encrypt=maybe"),
        Some("pw"),
    ))
    .await;
    assert!(out.message.unwrap().starts_with("Unsupported encrypt"));
}

#[tokio::test]
async fn windows_logins_and_other_servers_are_down() {
    let mut m = mssql("sqlserver://db.local", Some("pw"));
    m.basic_auth_user = Some("CORP\\vole".into());
    assert!(
        check(&m)
            .await
            .message
            .unwrap()
            .contains("Windows authentication")
    );
    let addr = foreign_server(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
    let out = check(&mssql(&format!("sqlserver://{addr}"), Some("pw"))).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(out.message.unwrap().starts_with("Not a SQL Server"), "");
}

#[test]
fn prelogin_layout_matches_the_spec_example() {
    // MS-TDS 4.1, the client's PRELOGIN (after its 8-byte header).
    let spec: [u8; 39] = [
        0x00, 0x00, 0x1A, 0x00, 0x06, 0x01, 0x00, 0x20, 0x00, 0x01, 0x02, 0x00, 0x21, 0x00, 0x01,
        0x03, 0x00, 0x22, 0x00, 0x04, 0x04, 0x00, 0x26, 0x00, 0x01, 0xFF, 0x09, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x01, 0x00, 0xB8, 0x0D, 0x00, 0x00, 0x01,
    ];
    let ours = prelogin_options(&[
        (PL_VERSION, vec![9, 0, 0, 0, 0, 0]),
        (PL_ENCRYPTION, vec![1]),
        (PL_INSTOPT, vec![0]),
        (PL_THREADID, vec![0xB8, 0x0D, 0, 0]),
        (PL_MARS, vec![1]),
    ]);
    assert_eq!(ours, spec);
    assert_eq!(
        parse_prelogin(&spec),
        Ok(ServerPrelogin {
            version: Some("9.0.0".into()),
            encryption: ENCRYPT_ON
        })
    );
    assert_eq!(
        frame(PRELOGIN, &spec, &mut 0)[..8],
        [0x12, 0x01, 0x00, 0x2F, 0x00, 0x00, 0x01, 0x00]
    );
    assert!(
        parse_prelogin(&[0x01, 0x00, 0x40, 0x00, 0x01, 0xFF]).is_err(),
        "options pointing outside are refused"
    );
}

#[test]
fn passwords_are_obfuscated_per_spec() {
    // 'p' 0x70 -> nibbles swapped 0x07 -> ^0xA5 = 0xA2; the high byte 0x00 -> 0xA5.
    assert_eq!(obfuscate("pw"), [0xA2, 0xA5, 0xD2, 0xA5]);
    assert_eq!(obfuscate("sa"), [0x92, 0xA5, 0xB3, 0xA5]);
    assert_eq!(deobfuscate(&obfuscate("pässwörd€")), utf16("pässwörd€"));
}

#[test]
fn login7_points_at_its_strings() {
    let login = login7(&Login {
        user: "vole",
        password: "pw",
        database: "app",
        server: "db.local",
    });
    assert_eq!(
        u32::from_le_bytes(login[..4].try_into().unwrap()) as usize,
        login.len()
    );
    assert_eq!(login[4..8], [0x04, 0x00, 0x00, 0x74], "TDS 7.4");
    assert_eq!(
        login[25] & 0x80,
        0,
        "SQL Server authentication, not integrated security"
    );
    assert_eq!(text(&login_bytes(&login, 1)), "vole");
    assert_eq!(login_bytes(&login, 2), obfuscate("pw"));
    assert_eq!(text(&login_bytes(&login, 4)), "db.local");
    assert_eq!(text(&login_bytes(&login, 8)), "app");
}

#[test]
fn reply_tokens_are_read() {
    let mut body = login_ack();
    body.extend(select_one_reply());
    let reply = parse_reply(&body).unwrap();
    assert_eq!(reply.login_version.as_deref(), Some("16.0.1000"));
    assert_eq!(
        (reply.values, reply.failed, reply.error),
        (vec![1], false, None)
    );

    let mut body = error_token(18456, "Login failed for user 'sa'.");
    body.extend(done(2, 0));
    let reply = parse_reply(&body).unwrap();
    assert_eq!(
        reply.error,
        Some((18456, "Login failed for user 'sa'.".into()))
    );
    assert!(reply.failed && reply.login_version.is_none());
    assert!(
        parse_reply(&[0x42])
            .unwrap_err()
            .contains("unexpected token")
    );
    assert!(
        parse_reply(&[0xAD, 0x40, 0x00, 1])
            .unwrap_err()
            .contains("truncated")
    );
}

#[test]
fn encrypt_options_and_their_protection() {
    let settings = |t: &str| Settings::of(&endpoint::parse(MonitorType::SqlServer, t).unwrap());
    let s = settings("db").unwrap();
    assert_eq!(
        (s.encrypt, s.verify, s.protection()),
        (Encrypt::Prefer, Verify::None, SslMode::Prefer)
    );
    let s = settings("db?encrypt=true&TrustServerCertificate=false").unwrap();
    assert_eq!(
        (s.encrypt, s.verify, s.protection()),
        (Encrypt::Require, Verify::Full, SslMode::VerifyFull)
    );
    let s = settings("db?encrypt=strict").unwrap();
    assert_eq!((s.verify, s.protection()), (Verify::None, SslMode::Require));
    let s = settings("db?encrypt=false&trustServerCertificate=no").unwrap();
    assert_eq!(
        (s.encrypt, s.verify),
        (Encrypt::Login, Verify::None),
        "nothing to verify without full encryption"
    );
    assert_eq!(
        settings("db?encrypt=disable").unwrap().protection(),
        SslMode::Disable
    );
    assert!(
        settings("db?trustServerCertificate=maybe")
            .unwrap_err()
            .contains("trustServerCertificate")
    );
    assert!(
        Encrypt::Disable < Encrypt::Login
            && Encrypt::Prefer < Encrypt::Require
            && Encrypt::Require < Encrypt::Strict
    );
}

#[test]
fn long_messages_are_split_into_packets() {
    let payload = vec![7u8; PACKET_SIZE * 2];
    let framed = frame(SQL_BATCH, &payload, &mut 0);
    assert_eq!(framed.len(), payload.len() + 3 * HEADER);
    assert_eq!((framed[1], framed[PACKET_SIZE + 1]), (0, 0));
    assert_eq!(framed[2 * PACKET_SIZE + 1], EOM);
}

/// Against a real server: `VV_TEST_SQLSERVER=sqlserver://host:port[?…],user,password`.
#[tokio::test]
#[ignore = "needs a SQL Server (VV_TEST_SQLSERVER)"]
async fn live_server() {
    let spec = std::env::var("VV_TEST_SQLSERVER").expect("VV_TEST_SQLSERVER");
    let [target, user, pass] = spec.splitn(3, ',').collect::<Vec<_>>()[..] else {
        panic!("target,user,password")
    };
    let sep = if target.contains('?') { '&' } else { '?' };
    for options in [
        "",
        "encrypt=disable",
        "encrypt=false",
        "encrypt=true",
        "encrypt=strict",
        "encrypt=true&trustServerCertificate=false",
    ] {
        let t = if options.is_empty() {
            target.to_string()
        } else {
            format!("{target}{sep}{options}")
        };
        let mut m = mssql(&t, Some(pass));
        m.basic_auth_user = Some(user.into());
        let out = check(&m).await;
        println!(
            "{t}: {:?} {:?} expiry={}",
            out.status,
            out.message,
            out.expiry.is_some()
        );
    }
    let mut m = mssql(target, Some("wrong"));
    m.basic_auth_user = Some(user.into());
    println!("wrong password: {:?}", check(&m).await.message);
    println!("no user: {:?}", check(&mssql(target, None)).await.message);
}
