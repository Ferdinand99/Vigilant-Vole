use super::*;
use crate::db::models::HeartbeatStatus;
use crate::monitor::checkers::test_support::monitor;

fn response(version: i64, id: i64, status: i64, value: Vec<u8>) -> Vec<u8> {
    let varbind = tlv(SEQUENCE, &[tlv(OID, &encode_oid(DEFAULT_OID).unwrap()), value].concat());
    let pdu = [encode_int(id), encode_int(status), encode_int(0), tlv(SEQUENCE, &varbind)].concat();
    tlv(SEQUENCE, &[encode_int(version), tlv(OCTET_STRING, b"public"), tlv(GET_RESPONSE, &pdu)].concat())
}

/// A fake agent: community `public`, answers `value` for the default OID.
/// `drop_first` swallows the first request (packet loss).
async fn agent(value: Vec<u8>, status: i64, drop_first: bool) -> std::net::SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut first = true;
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else { return };
            if std::mem::take(&mut first) && drop_first {
                continue;
            }
            let (_, msg, _) = read_tlv(&buf[..n]).unwrap();
            let (_, version, rest) = read_tlv(msg).unwrap();
            let (_, community, rest) = read_tlv(rest).unwrap();
            if community != b"public" {
                continue; // agents stay silent on a wrong community
            }
            let (_, pdu, _) = read_tlv(rest).unwrap();
            let (_, id, _) = read_tlv(pdu).unwrap();
            let reply = response(decode_int(version).unwrap(), decode_int(id).unwrap(), status, value.clone());
            socket.send_to(&reply, from).await.unwrap();
        }
    });
    addr
}

fn snmp(target: &str) -> Monitor {
    let mut m = monitor(MonitorType::Snmp, target);
    m.timeout_seconds = 2;
    m
}

#[test]
fn oids_round_trip_through_ber() {
    let enc = encode_oid("1.3.6.1.2.1.1.3.0").unwrap();
    assert_eq!(enc, [0x2B, 6, 1, 2, 1, 1, 3, 0]);
    assert_eq!(decode_oid(&enc), "1.3.6.1.2.1.1.3.0");
    assert_eq!(decode_oid(&encode_oid(".1.3.6.1.4.1.8072.1.3.2.3.1.1").unwrap()), "1.3.6.1.4.1.8072.1.3.2.3.1.1");
    assert_eq!(encode_oid("1.3.6.1.4.1.2021.10.1.5.1").unwrap().len(), 11);
    assert!(encode_oid("1").is_err() && encode_oid("1.x").is_err() && encode_oid("3.1").is_err() && encode_oid("1.40").is_err());
}

#[test]
fn integers_use_the_shortest_signed_form() {
    assert_eq!(encode_int(0), [2, 1, 0]);
    assert_eq!(encode_int(127), [2, 1, 127]);
    assert_eq!(encode_int(128), [2, 2, 0, 128]);
    assert_eq!(encode_int(-1), [2, 1, 255]);
    assert_eq!(decode_int(&[0, 128]), Some(128));
    assert_eq!(decode_int(&[255]), Some(-1));
}

#[test]
fn request_matches_a_known_packet() {
    // A v2c GET for sysUpTime.0, request id 1: net-snmp's snmpd answers it.
    let want = hex::decode("302602010104067075626c6963a019020101020100020100300e300c06082b060102010103000500").unwrap();
    assert_eq!(get_request(1, "public", 1, &encode_oid(DEFAULT_OID).unwrap()), want);
}

#[tokio::test]
async fn value_is_read_and_shown() {
    let addr = agent(tlv(0x43, &[0x00, 0xA0, 0x0F, 0x40]), 0, false).await; // 10_489_664 ticks
    let out = check(&snmp(&addr.to_string())).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert_eq!(out.message.as_deref(), Some("1.3.6.1.2.1.1.3.0 = 10489664 (1d 05:08:16)"));
}

#[tokio::test]
async fn keyword_and_invert_apply_to_the_value() {
    let addr = agent(tlv(OCTET_STRING, b"Linux router 6.1"), 0, false).await;
    let mut m = snmp(&addr.to_string());
    m.keyword = Some("Linux".into());
    assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    m.keyword = Some("Windows".into());
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(out.message.unwrap().ends_with("doesn't contain \"Windows\""));
    m.keyword_invert = true;
    assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    m.keyword = Some("Linux".into());
    assert!(check(&m).await.message.unwrap().ends_with("contains \"Linux\""));
}

#[tokio::test]
async fn community_version_and_missing_values() {
    let addr = agent(tlv(INTEGER, &[42]), 0, false).await;
    let mut m = snmp(&format!("snmp://{addr}?version=1"));
    assert_eq!(check(&m).await.message.as_deref(), Some("1.3.6.1.2.1.1.3.0 = 42"));
    m.secret = Some("private".into());
    let out = check(&m).await;
    assert!(out.message.unwrap().starts_with("No SNMP answer from 127.0.0.1"));

    let addr = agent(tlv(0x81, &[]), 0, false).await;
    let out = check(&snmp(&addr.to_string())).await;
    assert_eq!(out.message.as_deref(), Some("The device has no value for 1.3.6.1.2.1.1.3.0"));
    let addr = agent(tlv(NULL, &[]), 2, false).await;
    let out = check(&snmp(&format!("{addr}/1.3.6.1.9.9"))).await;
    assert_eq!(out.message.as_deref(), Some("The device has no value for 1.3.6.1.9.9"));
    let addr = agent(tlv(NULL, &[]), 16, false).await;
    assert!(check(&snmp(&addr.to_string())).await.message.unwrap().contains("authorizationError"));
    assert!(check(&snmp("127.0.0.1?version=4")).await.message.unwrap().starts_with("Unsupported SNMP version"));
    assert!(check(&snmp("127.0.0.1/1.x")).await.message.unwrap().contains("not a valid OID"));
}

#[tokio::test]
async fn a_lost_datagram_is_retried() {
    let addr = agent(tlv(INTEGER, &[7]), 0, true).await;
    let mut m = snmp(&addr.to_string());
    m.timeout_seconds = 3;
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
}

#[test]
fn garbage_is_not_an_answer() {
    assert!(parse_response(&[], 1).is_err());
    assert!(parse_response(&[0x30, 0x84, 0xFF, 0xFF, 0xFF, 0xFF], 1).is_err());
    assert!(parse_response(b"HTTP/1.1 400", 1).is_err());
}

#[test]
fn options_come_from_the_query() {
    let opts = |t: &str| options(&endpoint::parse(MonitorType::Snmp, t).unwrap());
    let o = opts("router").unwrap();
    assert_eq!((o.version, o.auth, o.privacy, o.context.as_str()), (Version::V2c, usm::Auth::Sha1, usm::Priv::Aes128, ""));
    let o = opts("snmp://router/1.3.6.1.2.1.1.5.0?version=3&auth=SHA-256&priv=aes256&context=vlan-5").unwrap();
    assert_eq!((o.version, o.auth, o.privacy, o.context.as_str()), (Version::V3, usm::Auth::Sha256, usm::Priv::Aes256, "vlan-5"));
    assert_eq!(opts("router?version=1").unwrap().version, Version::V1);
    assert!(opts("router?version=3&auth=sha3").unwrap_err().contains("authentication protocol"));
    assert!(opts("router?version=3&priv=aes256c").unwrap_err().contains("privacy protocol"));
    assert!(is_v3("snmp://router?version=3") && !is_v3("router?version=2c") && !is_v3("router") && !is_v3("http://x?version=3"));
}

#[test]
fn credentials_imply_the_security_level() {
    let opts = Options { version: Version::V3, auth: usm::Auth::Md5, privacy: usm::Priv::Des, context: String::new() };
    let mut m = snmp("router?version=3");
    assert_eq!(credentials(&m, &opts).unwrap_err(), "SNMPv3 needs a user name");
    m.basic_auth_user = Some("watcher".into());
    let c = credentials(&m, &opts).unwrap();
    assert_eq!((c.auth.is_none(), c.privacy.is_none()), (true, true));
    m.secret = Some("privpass123".into());
    assert!(credentials(&m, &opts).unwrap_err().contains("needs an authentication password"));
    m.basic_auth_pass = Some("short".into());
    assert!(credentials(&m, &opts).unwrap_err().contains("at least 8"));
    m.basic_auth_pass = Some("authpass123".into());
    let c = credentials(&m, &opts).unwrap();
    assert_eq!(c.auth, Some((usm::Auth::Md5, "authpass123".to_string())));
    assert_eq!(c.privacy, Some((usm::Priv::Des, "privpass123".to_string())));
}

/// How the fake v3 agent misbehaves.
#[derive(Clone, Copy, PartialEq)]
enum Quirk {
    None,
    /// Drops the first GET.
    LoseFirstGet,
    /// Answers the first GET with notInTimeWindow (and a later clock).
    ClockJump,
    /// GETs are refused with error-status 16.
    Unauthorized,
    /// Doesn't answer GETs (but does the discovery).
    Mute,
}

const ENGINE_ID: &[u8] = b"\x80\x00\x1f\x88\x80vole-test";

/// A fake SNMPv3 agent for `agent_creds`, answering `value` for any OID.
async fn agent_v3(agent_creds: usm::Credentials, value: Vec<u8>, quirk: Quirk) -> std::net::SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let keys = agent_creds.keys(ENGINE_ID);
        let mut engine = usm::Engine { id: ENGINE_ID.to_vec(), boots: 3, time: 1_000 };
        let mut gets = 0;
        let mut buf = vec![0u8; 4096];
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else { return };
            let packet = &buf[..n];
            let report = |msg_id: i32, oid: &str, signed: bool, engine: &usm::Engine| {
                let counter = tlv(SEQUENCE, &tlv(SEQUENCE, &[tlv(OID, &encode_oid(oid).unwrap()), tlv(0x41, &[1])].concat()));
                let pdu = tlv(usm::REPORT, &[encode_int(0), encode_int(0), encode_int(0), counter].concat());
                let req = usm::Request {
                    msg_id,
                    request_id: 0,
                    engine,
                    user: &agent_creds.user,
                    keys: signed.then_some(&keys),
                    context: "",
                    oid: None,
                    salt: 9,
                };
                usm::reply(&req, &pdu)
            };
            let msg_id = usm::msg_id(packet).unwrap() as i32;
            let request = match usm::parse(packet, Some(&keys)) {
                Ok(r) => r,
                Err(e) if e.contains("authentication code") => {
                    let _ = socket.send_to(&report(msg_id, "1.3.6.1.6.3.15.1.1.5.0", false, &engine), from).await;
                    continue;
                }
                // A wrong privacy key decrypts to garbage: net-snmp drops it.
                Err(_) => continue,
            };
            let answer = if request.engine.id.is_empty() {
                report(msg_id, "1.3.6.1.6.3.15.1.1.4.0", false, &engine)
            } else if request.user != agent_creds.user.as_bytes() {
                report(msg_id, "1.3.6.1.6.3.15.1.1.3.0", false, &engine)
            } else if request.authenticated != keys.authenticates() || request.encrypted != keys.encrypts() {
                report(msg_id, "1.3.6.1.6.3.15.1.1.1.0", false, &engine)
            } else {
                gets += 1;
                match quirk {
                    Quirk::Mute => continue,
                    Quirk::LoseFirstGet if gets == 1 => continue,
                    Quirk::ClockJump if gets == 1 => {
                        engine.time += 600;
                        let _ = socket.send_to(&report(msg_id, "1.3.6.1.6.3.15.1.1.2.0", true, &engine), from).await;
                        continue;
                    }
                    _ => {}
                }
                if quirk == Quirk::ClockJump && request.engine.time != engine.time {
                    continue;
                }
                let (_, id, rest) = read_tlv(&request.pdu).unwrap();
                let (_, _, rest) = read_tlv(rest).unwrap();
                let (_, _, rest) = read_tlv(rest).unwrap();
                let (_, varbinds, _) = read_tlv(rest).unwrap();
                let (_, varbind, _) = read_tlv(varbinds).unwrap();
                let (_, name, _) = read_tlv(varbind).unwrap();
                let status = if quirk == Quirk::Unauthorized { 16 } else { 0 };
                let vb = tlv(SEQUENCE, &tlv(SEQUENCE, &[tlv(OID, name), value.clone()].concat()));
                let pdu = tlv(GET_RESPONSE, &[tlv(INTEGER, id), encode_int(status), encode_int(0), vb].concat());
                let req = usm::Request {
                    msg_id,
                    request_id: 0,
                    engine: &engine,
                    user: &agent_creds.user,
                    keys: Some(&keys),
                    context: "",
                    oid: None,
                    salt: 0x0102_0304_0506_0708,
                };
                usm::reply(&req, &pdu)
            };
            let _ = socket.send_to(&answer, from).await;
        }
    });
    addr
}

fn creds(user: &str, auth: Option<(usm::Auth, &str)>, privacy: Option<(usm::Priv, &str)>) -> usm::Credentials {
    usm::Credentials {
        user: user.into(),
        auth: auth.map(|(a, pw)| (a, pw.to_string())),
        privacy: privacy.map(|(p, pw)| (p, pw.to_string())),
    }
}

fn v3_monitor(addr: std::net::SocketAddr, query: &str, user: &str, auth_pw: Option<&str>, priv_pw: Option<&str>) -> Monitor {
    let mut m = snmp(&format!("snmp://{addr}/1.3.6.1.2.1.1.5.0?version=3{query}"));
    m.basic_auth_user = Some(user.into());
    m.basic_auth_pass = auth_pw.map(Into::into);
    m.secret = priv_pw.map(Into::into);
    m
}

#[tokio::test]
async fn v3_reads_values_at_every_security_level() {
    let value = tlv(OCTET_STRING, b"core-switch");
    let cases = [
        ("", None, None),
        ("&auth=md5", Some(usm::Auth::Md5), None),
        ("&auth=sha&priv=des", Some(usm::Auth::Sha1), Some(usm::Priv::Des)),
        ("&auth=sha224&priv=aes", Some(usm::Auth::Sha224), Some(usm::Priv::Aes128)),
        ("&auth=md5&priv=aes256", Some(usm::Auth::Md5), Some(usm::Priv::Aes256)),
        ("&auth=sha384&priv=aes192", Some(usm::Auth::Sha384), Some(usm::Priv::Aes192)),
        ("&auth=sha512&priv=aes256", Some(usm::Auth::Sha512), Some(usm::Priv::Aes256)),
    ];
    for (query, auth, privacy) in cases {
        let agent_creds = creds("vole", auth.map(|a| (a, "authpass123")), privacy.map(|p| (p, "privpass123")));
        let addr = agent_v3(agent_creds, value.clone(), Quirk::None).await;
        let m = v3_monitor(addr, query, "vole", auth.map(|_| "authpass123"), privacy.map(|_| "privpass123"));
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{query}: {:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("1.3.6.1.2.1.1.5.0 = core-switch"), "{query}");
    }
}

#[tokio::test]
async fn v3_failures_name_their_cause() {
    let value = tlv(INTEGER, &[1]);
    let agent_creds = creds("vole", Some((usm::Auth::Sha256, "authpass123")), Some((usm::Priv::Aes128, "privpass123")));
    let addr = agent_v3(agent_creds.clone(), value.clone(), Quirk::None).await;
    let msg = |m: Monitor| async move { check(&m).await.message.unwrap() };

    let wrong_auth = msg(v3_monitor(addr, "&auth=sha256", "vole", Some("authpass124"), Some("privpass123"))).await;
    assert!(wrong_auth.starts_with("Wrong authentication password"), "{wrong_auth}");
    let wrong_proto = msg(v3_monitor(addr, "&auth=sha", "vole", Some("authpass123"), Some("privpass123"))).await;
    assert!(wrong_proto.starts_with("Wrong authentication password or protocol"), "{wrong_proto}");
    let unknown = msg(v3_monitor(addr, "&auth=sha256", "mole", Some("authpass123"), Some("privpass123"))).await;
    assert_eq!(unknown, "The device doesn't know the SNMPv3 user \"mole\"");
    let level = msg(v3_monitor(addr, "&auth=sha256", "vole", Some("authpass123"), None)).await;
    assert!(level.starts_with("The device doesn't accept this security level"), "{level}");
    let wrong_priv = msg(v3_monitor(addr, "&auth=sha256", "vole", Some("authpass123"), Some("privpass124"))).await;
    assert!(wrong_priv.contains("answered the SNMPv3 discovery but not the request") && wrong_priv.contains("privacy password"), "{wrong_priv}");

    let addr = agent_v3(agent_creds.clone(), value.clone(), Quirk::Unauthorized).await;
    let denied = msg(v3_monitor(addr, "&auth=sha256", "vole", Some("authpass123"), Some("privpass123"))).await;
    assert!(denied.contains("authorizationError"), "{denied}");

    let addr = agent_v3(creds("vole", None, None), value, Quirk::Mute).await;
    let mute = msg(v3_monitor(addr, "", "vole", None, None)).await;
    assert!(mute.ends_with("answered the SNMPv3 discovery but not the request within 2s"), "{mute}");
}

#[tokio::test]
async fn v3_resyncs_its_clock_and_retries_lost_requests() {
    let agent_creds = creds("vole", Some((usm::Auth::Sha1, "authpass123")), Some((usm::Priv::Des, "privpass123")));
    for quirk in [Quirk::ClockJump, Quirk::LoseFirstGet] {
        let addr = agent_v3(agent_creds.clone(), tlv(INTEGER, &[5]), quirk).await;
        let mut m = v3_monitor(addr, "&auth=sha&priv=des", "vole", Some("authpass123"), Some("privpass123"));
        m.timeout_seconds = 4;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    }
}

#[tokio::test]
async fn v3_without_an_agent_times_out_with_a_hint() {
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut m = v3_monitor(silent.local_addr().unwrap(), "", "vole", None, None);
    m.timeout_seconds = 1;
    let out = check(&m).await;
    assert!(out.message.unwrap().starts_with("No SNMPv3 answer from 127.0.0.1"));
    m.basic_auth_user = None;
    assert_eq!(check(&m).await.message.as_deref(), Some("SNMPv3 needs a user name"));
}

/// Against a real agent: `VV_TEST_SNMP=snmp://host:port/oid,community`.
#[tokio::test]
#[ignore = "needs an SNMP agent (VV_TEST_SNMP)"]
async fn live_agent() {
    let spec = std::env::var("VV_TEST_SNMP").expect("VV_TEST_SNMP");
    let (target, community) = spec.split_once(',').expect("target,community");
    let mut m = snmp(target);
    m.secret = Some(community.into());
    let out = check(&m).await;
    println!("{:?}", out.message);
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    m.secret = Some("wrong".into());
    println!("{:?}", check(&m).await.message);
}

/// Against a net-snmp agent with the users `noauth` and `<auth>_<priv>` for
/// every combination (auth md5, sha, sha224 … sha512; priv none, des, aes,
/// aes192, aes256), passwords authpass123 / privpass123, all `rouser` at
/// their level: `VV_TEST_SNMPV3=127.0.0.1:16161`.
#[tokio::test]
#[ignore = "needs the net-snmp test agent (VV_TEST_SNMPV3)"]
async fn live_agent_v3() {
    let addr = std::env::var("VV_TEST_SNMPV3").expect("VV_TEST_SNMPV3");
    let monitor = |user: &str, query: &str, auth: Option<&str>, privacy: Option<&str>| {
        let mut m = snmp(&format!("snmp://{addr}/1.3.6.1.2.1.1.1.0?version=3{query}"));
        m.basic_auth_user = Some(user.into());
        m.basic_auth_pass = auth.map(Into::into);
        m.secret = privacy.map(Into::into);
        m
    };
    let mut failed = Vec::new();
    let run = |label: String, m: Monitor, want_up: bool| async move {
        let out = check(&m).await;
        println!("{label:<26} {:?} {:?}ms {:?}", out.status, out.response_time_ms.unwrap_or(-1), out.message.unwrap_or_default());
        (out.status == HeartbeatStatus::Up) == want_up
    };
    if !run("noauth".into(), monitor("noauth", "", None, None), true).await {
        failed.push("noauth".to_string());
    }
    for auth in ["md5", "sha", "sha224", "sha256", "sha384", "sha512"] {
        for privacy in ["none", "des", "aes", "aes192", "aes256"] {
            let user = format!("{auth}_{privacy}");
            let (query, priv_pw) = if privacy == "none" { (format!("&auth={auth}"), None) } else { (format!("&auth={auth}&priv={privacy}"), Some("privpass123")) };
            let mut ok = run(user.clone(), monitor(&user, &query, Some("authpass123"), priv_pw), true).await;
            ok &= run(format!("{user} wrong auth"), monitor(&user, &query, Some("wrongpass1"), priv_pw), false).await;
            if priv_pw.is_some() {
                ok &= run(format!("{user} wrong priv"), monitor(&user, &query, Some("authpass123"), Some("wrongpass1")), false).await;
            }
            if !ok {
                failed.push(user);
            }
        }
    }
    run("unknown user".into(), monitor("nobody", "", None, None), false).await;
    run("level too low".into(), monitor("sha_aes", "&auth=sha", Some("authpass123"), None), false).await;
    run("no auth for an auth user".into(), monitor("sha_none", "", None, None), false).await;
    assert!(failed.is_empty(), "{failed:?}");
}
