use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use super::*;
use crate::db::models::HeartbeatStatus;
use crate::monitor::checkers::test_support::{foreign_server, monitor, tls_acceptor};

/// What the fake server is.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Standalone,
    Primary,
    /// A secondary that knows its primary.
    Secondary,
    /// A secondary of a set that has lost its primary.
    Orphan,
    /// Predates `hello` (answers only `isMaster`).
    Old,
}

#[derive(Clone, Copy)]
struct Fake {
    role: Role,
    /// Mechanisms listed for user `vole` (password `pw`, auth database `admin`).
    mechs: &'static [&'static str],
    tls: bool,
}

const BOTH: &[&str] = &["SCRAM-SHA-1", "SCRAM-SHA-256"];

fn ok(doc: Document) -> Document {
    doc.with("ok", 1.0)
}

fn error(code: i32, message: &str) -> Document {
    Document::new()
        .with("ok", 0.0)
        .with("errmsg", message)
        .with("code", code)
        .with("codeName", "Error")
}

fn hello_reply(fake: Fake, asked: &Document) -> Document {
    let mut d = Document::new()
        .with(
            "topologyVersion",
            Document(vec![
                ("processId".into(), Bson::ObjectId([1; 12])),
                ("counter".into(), Bson::Int64(0)),
            ]),
        )
        .with("maxWireVersion", 21)
        .with("localTime", Bson::DateTime(1_700_000_000_000));
    d = match fake.role {
        Role::Standalone | Role::Old => d.with("ismaster", true).with("isWritablePrimary", true),
        Role::Primary => d
            .with("isWritablePrimary", true)
            .with("setName", "rs0")
            .with("primary", "a:27017"),
        Role::Secondary => d
            .with("isWritablePrimary", false)
            .with("secondary", true)
            .with("setName", "rs0")
            .with("primary", "a:27017"),
        Role::Orphan => d
            .with("isWritablePrimary", false)
            .with("secondary", true)
            .with("setName", "rs0"),
    };
    if asked.str("saslSupportedMechs") == Some("admin.vole") {
        d = d.with(
            "saslSupportedMechs",
            Bson::Array(fake.mechs.iter().map(|m| Bson::from(*m)).collect()),
        );
    }
    ok(d.with(
        "$clusterTime",
        Document::new().with("clusterTime", Bson::Timestamp(7)),
    )
    .with("operationTime", Bson::Timestamp(7)))
}

/// SCRAM state between saslStart and saslContinue: (hash, client-first-bare, server-first).
type Pending = Option<(Hash, String, String)>;

fn handle(fake: Fake, cmd: &Document, pending: &mut Pending) -> Document {
    let name = cmd.0.first().map(|(k, _)| k.as_str()).unwrap_or_default();
    match name {
        "hello" if fake.role == Role::Old => error(59, "no such command: 'hello'"),
        "hello" | "isMaster" => hello_reply(fake, cmd),
        "buildInfo" => ok(Document::new().with("version", "7.0.14").with(
            "versionArray",
            Bson::Array(vec![7.into(), 0.into(), 14.into(), 0.into()]),
        )),
        "ping" => ok(Document::new()),
        "saslStart" => {
            let hash = match cmd.str("mechanism") {
                Some("SCRAM-SHA-256") if fake.mechs.contains(&"SCRAM-SHA-256") => Hash::Sha256,
                Some("SCRAM-SHA-1") if fake.mechs.contains(&"SCRAM-SHA-1") => Hash::Sha1,
                _ => return error(334, "Unsupported mechanism"),
            };
            assert_eq!(cmd.str("$db"), Some("admin"));
            let first = String::from_utf8(cmd.binary("payload").unwrap().to_vec()).unwrap();
            let bare = first.strip_prefix("n,,").unwrap().to_string();
            let nonce = bare.split("r=").nth(1).unwrap();
            let server_first = format!("r={nonce}srv,s={},i=4096", B64.encode(b"mongosalt"));
            *pending = Some((hash, bare, server_first.clone()));
            ok(Document::new()
                .with("conversationId", 1)
                .with("done", false)
                .with("payload", Bson::Binary(0, server_first.into_bytes())))
        }
        "saslContinue" => {
            let Some((hash, bare, server_first)) = pending.take() else {
                return error(17, "No SASL session state found");
            };
            let fin = String::from_utf8(cmd.binary("payload").unwrap().to_vec()).unwrap();
            let (without_proof, proof) = fin.rsplit_once(",p=").unwrap();
            let secret = match hash {
                Hash::Sha256 => "pw".to_string(),
                Hash::Sha1 => sha1_password("vole", "pw"),
            };
            let salted = scram::salted_password(hash, secret.as_bytes(), b"mongosalt", 4096);
            let client_key = hash.hmac(&salted, b"Client Key");
            let auth_message = format!("{bare},{server_first},{without_proof}");
            let signature = hash.hmac(&hash.digest(&client_key), auth_message.as_bytes());
            let recovered: Vec<u8> = B64
                .decode(proof)
                .unwrap()
                .iter()
                .zip(signature)
                .map(|(a, b)| a ^ b)
                .collect();
            if recovered != client_key || !bare.starts_with("n=vole,") {
                return error(18, "Authentication failed.");
            }
            let server_key = hash.hmac(&salted, b"Server Key");
            let v = format!(
                "v={}",
                B64.encode(hash.hmac(&server_key, auth_message.as_bytes()))
            );
            ok(Document::new()
                .with("conversationId", 1)
                .with("done", true)
                .with("payload", Bson::Binary(0, v.into_bytes())))
        }
        other => error(59, &format!("no such command: '{other}'")),
    }
}

async fn serve_conn(mut s: Stream, fake: Fake) {
    let mut pending = None;
    loop {
        let mut header = [0u8; 16];
        if s.read_exact(&mut header).await.is_err() {
            return;
        }
        let len = i32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let id = i32::from_le_bytes(header[4..8].try_into().unwrap());
        assert_eq!(i32::from_le_bytes(header[12..].try_into().unwrap()), OP_MSG);
        let mut body = vec![0u8; len - 16];
        s.read_exact(&mut body).await.unwrap();
        let cmd = parse_op_msg(&body).unwrap();
        let reply = handle(fake, &cmd, &mut pending);
        let mut msg = op_msg(id + 1000, &reply);
        msg[8..12].copy_from_slice(&id.to_le_bytes());
        if s.write_all(&msg).await.is_err() {
            return;
        }
    }
}

async fn serve(fake: Fake) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tls_acceptor();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let stream: Stream = if fake.tls {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    Box::new(tls)
                } else {
                    Box::new(tcp)
                };
                serve_conn(stream, fake).await;
            });
        }
    });
    addr
}

fn fake(role: Role) -> Fake {
    Fake {
        role,
        mechs: BOTH,
        tls: false,
    }
}

fn mongo(target: &str, pass: Option<&str>) -> Monitor {
    let mut m = monitor(MonitorType::Mongodb, target);
    m.basic_auth_user = pass.map(|_| "vole".into());
    m.basic_auth_pass = pass.map(str::to_string);
    m
}

#[tokio::test]
async fn hello_reports_version_and_role() {
    let addr = serve(fake(Role::Standalone)).await;
    let out = check(&mongo(&format!("mongodb://{addr}"), None)).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert_eq!(
        out.message.as_deref(),
        Some("MongoDB 7.0.14 · standalone · ping OK")
    );
    let addr = serve(fake(Role::Primary)).await;
    let out = check(&mongo(&format!("mongodb://{addr}/?replicaSet=rs0"), None)).await;
    assert_eq!(
        out.message.as_deref(),
        Some("MongoDB 7.0.14 · primary of rs0 · ping OK")
    );
}

#[tokio::test]
async fn servers_without_hello_are_asked_with_is_master() {
    let addr = serve(fake(Role::Old)).await;
    let out = check(&mongo(&format!("mongodb://{addr}"), Some("pw"))).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert_eq!(
        out.message.as_deref(),
        Some("MongoDB 7.0.14 · standalone · login and ping OK")
    );
}

#[tokio::test]
async fn scram_sha_256_logs_in_and_wrong_passwords_are_explained() {
    let addr = serve(fake(Role::Standalone)).await;
    let out = check(&mongo(&format!("mongodb://{addr}"), Some("pw"))).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert_eq!(
        out.message.as_deref(),
        Some("MongoDB 7.0.14 · standalone · login and ping OK")
    );
    let out = check(&mongo(&format!("mongodb://{addr}"), Some("nope"))).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert_eq!(
        out.message.as_deref(),
        Some("Authentication failed: wrong user name or password")
    );
    // The user's database is the default authSource only when no other is named.
    let out = check(&mongo(
        &format!("mongodb://{addr}/app?authSource=admin"),
        Some("pw"),
    ))
    .await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
}

#[tokio::test]
async fn scram_sha_1_is_used_when_the_user_only_has_it() {
    let addr = serve(Fake {
        mechs: &["SCRAM-SHA-1"],
        ..fake(Role::Standalone)
    })
    .await;
    let out = check(&mongo(&format!("mongodb://{addr}"), Some("pw"))).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    let out = check(&mongo(
        &format!("mongodb://{addr}?authMechanism=SCRAM-SHA-256"),
        Some("pw"),
    ))
    .await;
    assert!(
        out.message
            .unwrap()
            .starts_with("Authentication failed: Server error 334"),
        "a forced mechanism is used as is"
    );
    let out = check(&mongo(
        &format!("mongodb://{addr}?authMechanism=MONGODB-X509"),
        Some("pw"),
    ))
    .await;
    assert!(out.message.unwrap().contains("isn't supported"));
}

#[tokio::test]
async fn tls_is_verified_unless_invalid_certificates_are_allowed() {
    let addr = serve(Fake {
        tls: true,
        ..fake(Role::Standalone)
    })
    .await;
    let target = format!("mongodb://localhost:{}/?tls=true", addr.port());
    let out = check(&mongo(&target, Some("pw"))).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(
        out.message.as_deref().unwrap().contains("not trusted"),
        "{:?}",
        out.message
    );
    assert!(out.expiry.is_some());
    for t in [
        format!("{target}&tlsAllowInvalidCertificates=true"),
        format!("{target}&tlsInsecure=true"),
    ] {
        let out = check(&mongo(&t, Some("pw"))).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{t}: {:?}", out.message);
        assert_eq!(
            out.message.as_deref(),
            Some("MongoDB 7.0.14 · standalone · login and ping OK · TLS 1.3")
        );
        assert!(out.expiry.is_some());
    }
    let mut m = mongo(&target, None);
    m.ignore_tls = true;
    assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    // A plain connection to a TLS-only server fails.
    assert_eq!(
        check(&mongo(
            &format!("mongodb://localhost:{}", addr.port()),
            None
        ))
        .await
        .status,
        HeartbeatStatus::Down
    );
}

#[tokio::test]
async fn replica_sets_need_a_primary_when_named() {
    let orphan = serve(fake(Role::Orphan)).await;
    let out = check(&mongo(&format!("mongodb://{orphan}/?replicaSet=rs0"), None)).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert_eq!(
        out.message.as_deref(),
        Some("Replica set rs0 has no primary")
    );
    let out = check(&mongo(&format!("mongodb://{orphan}"), None)).await;
    assert_eq!(
        out.message.as_deref(),
        Some("MongoDB 7.0.14 · secondary of rs0, no primary known · ping OK")
    );

    let secondary = serve(fake(Role::Secondary)).await;
    let out = check(&mongo(
        &format!("mongodb://{secondary}/?replicaSet=rs0"),
        Some("pw"),
    ))
    .await;
    assert_eq!(
        out.message.as_deref(),
        Some("MongoDB 7.0.14 · secondary of rs0 (primary a:27017) · login and ping OK")
    );
    let out = check(&mongo(
        &format!("mongodb://{secondary}/?replicaSet=prod"),
        None,
    ))
    .await;
    assert_eq!(
        out.message.as_deref(),
        Some("The server belongs to replica set rs0, not prod")
    );
    let standalone = serve(fake(Role::Standalone)).await;
    let out = check(&mongo(
        &format!("mongodb://{standalone}/?replicaSet=rs0"),
        None,
    ))
    .await;
    assert_eq!(
        out.message.as_deref(),
        Some("The server is not a member of replica set rs0")
    );
}

#[tokio::test]
async fn the_first_seed_that_answers_is_checked() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = closed.local_addr().unwrap();
    drop(closed);
    let addr = serve(fake(Role::Primary)).await;
    let out = check(&mongo(
        &format!("mongodb://{dead},{addr}/?replicaSet=rs0"),
        None,
    ))
    .await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    let out = check(&mongo(&format!("mongodb://{dead}"), None)).await;
    assert!(out.message.unwrap().contains("Connection refused"));
}

#[tokio::test]
async fn other_servers_are_down() {
    let addr = foreign_server(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
    let out = check(&mongo(&format!("mongodb://{addr}"), None)).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(out.message.unwrap().starts_with("Not a MongoDB server"));
}

#[test]
fn op_msg_round_trips() {
    let doc = Document::new().with("ping", 1).with("$db", "admin");
    let msg = op_msg(7, &doc);
    assert_eq!(
        i32::from_le_bytes(msg[..4].try_into().unwrap()) as usize,
        msg.len()
    );
    assert_eq!(msg[12..16], OP_MSG.to_le_bytes());
    assert_eq!(parse_op_msg(&msg[16..]).unwrap(), doc);
    // A document sequence (kind 1) before the body and a checksum after it are skipped.
    let mut with_extras = vec![1, 0, 0, 0, 1, 8, 0, 0, 0, b'x', 0, 0, 0, 0];
    with_extras.extend(doc.to_bytes());
    with_extras.extend_from_slice(&[9, 9, 9, 9]);
    assert_eq!(parse_op_msg(&with_extras).unwrap(), doc);
    assert!(parse_op_msg(&[0, 0, 0, 0, 7]).is_err());
}

#[test]
fn sha1_password_matches_the_driver_spec() {
    // MongoDB auth spec: user "user", password "pencil".
    assert_eq!(
        sha1_password("user", "pencil"),
        "1c33006ec1ffd90f9cadcbcc0e118200"
    );
    let fin = scram::client_final(
        Hash::Sha1,
        sha1_password("user", "pencil").as_bytes(),
        "n=user,r=fyko+d2lbbFgONRv9qkxdawL",
        "r=fyko+d2lbbFgONRv9qkxdawLHo+Vgk7qvUOKUwuWLIWg4l/9SraGMHEE,s=rQ9ZY3MntBeuP3E1TDVC4w==,i=10000",
        "fyko+d2lbbFgONRv9qkxdawL",
    )
    .unwrap();
    assert_eq!(
        fin.message,
        "c=biws,r=fyko+d2lbbFgONRv9qkxdawLHo+Vgk7qvUOKUwuWLIWg4l/9SraGMHEE,p=MC2T8BvbmWRckDw8oWl5IVghwCY="
    );
    assert!(scram::server_final_ok(
        "v=UMWeI25JD1yNYZRMpZ4VHvhZ9e0=",
        &fin.server_signature
    ));
}

#[test]
fn srv_answers_must_stay_in_the_domain() {
    assert!(check_srv_host("cluster0.example.net", "shard-00.cluster0.example.net.").is_ok());
    assert!(check_srv_host("cluster0.example.net", "node.example.net").is_ok());
    assert!(check_srv_host("cluster0.example.net", "evil.com").is_err());
    assert!(check_srv_host("cluster0.example.net", "example.net").is_err());
    assert!(check_srv_host("example.net", "db.example.net").is_ok());
    assert!(check_srv_host("example.net", "evil-example.net").is_err());
}

#[test]
fn txt_records_only_carry_allowed_options() {
    let opts = txt_options("authSource=admin&replicaSet=atlas-abc-shard-0&w=majority");
    assert_eq!(
        opts,
        vec![
            ("authSource".into(), "admin".into()),
            ("replicaSet".into(), "atlas-abc-shard-0".into())
        ]
    );
}

#[test]
fn options_and_their_protection() {
    let ep = |t: &str| endpoint::parse(MonitorType::Mongodb, t).unwrap();
    assert_eq!(protection(&ep("mongodb://h")), SslMode::Disable);
    assert_eq!(
        protection(&ep("mongodb://h/?tls=true")),
        SslMode::VerifyFull
    );
    assert_eq!(
        protection(&ep("mongodb+srv://c.example.net")),
        SslMode::VerifyFull
    );
    assert_eq!(
        protection(&ep("mongodb://h/?tls=true&tlsAllowInvalidHostnames=true")),
        SslMode::VerifyCa
    );
    assert_eq!(
        protection(&ep("mongodb://h/?tls=true&tlsInsecure=true")),
        SslMode::Require
    );
    assert_eq!(
        mechanism(&ep("mongodb://h/?authMechanism=scram-sha-1")),
        Ok(Some(Hash::Sha1))
    );
    assert_eq!(mechanism(&ep("mongodb://h")), Ok(None));
}

/// Against a real server: `VV_TEST_MONGODB=mongodb://host:port/…,user,password`.
#[tokio::test]
#[ignore = "needs a MongoDB server (VV_TEST_MONGODB)"]
async fn live_server() {
    let spec = std::env::var("VV_TEST_MONGODB").expect("VV_TEST_MONGODB");
    let [target, user, pass] = spec.splitn(3, ',').collect::<Vec<_>>()[..] else {
        panic!("target,user,password")
    };
    let mut m = mongo(target, Some(pass));
    m.basic_auth_user = Some(user.into());
    let out = check(&m).await;
    println!(
        "{:?} {:?} expiry={}",
        out.status,
        out.message,
        out.expiry.is_some()
    );
    m.basic_auth_pass = Some("wrong".into());
    println!("wrong password: {:?}", check(&m).await.message);
    println!("no user: {:?}", check(&mongo(target, None)).await.message);
}
