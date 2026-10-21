//! MongoDB checker: `target` =
//! `mongodb://host[:port][,host2…][/database][?tls=true&authSource=…&replicaSet=…]`
//! or `mongodb+srv://name[/database][?…]`.
//!
//! Speaks the OP_MSG wire protocol (MongoDB 3.6 and later) with a small
//! BSON codec: `hello` (`isMaster` on servers that predate it) for the
//! server's role, `buildInfo` for its version, SCRAM-SHA-256 or SCRAM-SHA-1
//! authentication when the monitor has a user name (the mechanism the
//! server lists for the user via `saslSupportedMechs`, or `authMechanism`),
//! then `ping`. The seed hosts are tried in order and the first that
//! accepts a connection is checked. `mongodb+srv://` looks up the hosts
//! (SRV `_mongodb._tcp.<name>`) and the `authSource`, `replicaSet` and
//! `loadBalanced` options (TXT) like the drivers do, and turns TLS on.
//!
//! TLS (`tls=true`/`ssl=true`) verifies the certificate like the drivers do,
//! unless `tlsAllowInvalidCertificates=true` or `tlsInsecure=true` (or the
//! monitor ignores TLS errors); `tlsAllowInvalidHostnames=true` checks the
//! chain only. The certificate's expiry is reported whenever TLS is used.
//!
//! Up = reachable, logged in (with a user name) and `ping` answered. With
//! `replicaSet=…` the server must belong to that set and a primary must be
//! known. Passwords are used as typed (no SASLprep normalisation), and
//! X.509, LDAP, Kerberos and AWS authentication are not supported.

mod bson;

use std::time::{Duration, Instant};

use md5::Digest as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use self::bson::{Bson, Document};
use super::conn::{self, Io, SslMode};
use super::endpoint::{self, Endpoint};
use super::scram::{self, Hash};
use super::tls::{self, CertSummary, Verify};
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const OP_MSG: i32 = 2013;
const OP_REPLY: i32 = 1;
/// MongoDB's largest message (48 MB) is far more than any reply here.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// `CommandNotFound`: `hello` on a server older than 4.4.2.
const COMMAND_NOT_FOUND: i64 = 59;
const AUTHENTICATION_FAILED: i64 = 18;

type Stream = Box<dyn Io>;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// How the certificate is checked (before "ignore TLS errors").
pub fn verify(ep: &Endpoint) -> Verify {
    let on = |key: &str| {
        ep.query(key)
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
    };
    if on("tlsInsecure") || on("tlsAllowInvalidCertificates") || on("sslAllowInvalidCertificates") {
        Verify::None
    } else if on("tlsAllowInvalidHostnames") || on("sslAllowInvalidHostnames") {
        Verify::Ca
    } else {
        Verify::Full
    }
}

/// How well a password sent to this target is protected, on the `sslmode`
/// scale (stored passwords need re-entry when it drops).
pub fn protection(ep: &Endpoint) -> SslMode {
    match (ep.tls, verify(ep)) {
        (false, _) => SslMode::Disable,
        (true, Verify::None) => SslMode::Require,
        (true, Verify::Ca) => SslMode::VerifyCa,
        (true, Verify::Full) => SslMode::VerifyFull,
    }
}

/// The SCRAM mechanism `authMechanism` asks for (`None`: negotiate).
fn mechanism(ep: &Endpoint) -> Result<Option<Hash>, String> {
    match ep.query("authMechanism").map(str::trim) {
        None | Some("") => Ok(None),
        Some(m) if m.eq_ignore_ascii_case("SCRAM-SHA-256") => Ok(Some(Hash::Sha256)),
        Some(m) if m.eq_ignore_ascii_case("SCRAM-SHA-1") => Ok(Some(Hash::Sha1)),
        Some(m) => Err(format!(
            "authMechanism {} isn't supported (use SCRAM-SHA-256 or SCRAM-SHA-1)",
            util::truncate(m, 30)
        )),
    }
}

/// The options of a `mongodb+srv://` TXT record (`authSource=admin&replicaSet=rs0`).
/// Only the options the specification allows there are kept.
fn txt_options(text: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(text.trim().as_bytes())
        .filter(|(k, _)| {
            ["authSource", "replicaSet", "loadBalanced"]
                .iter()
                .any(|o| o.eq_ignore_ascii_case(k))
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// SRV answers must lie under the parent domain of the looked-up name (or
/// under the name itself when it has fewer than three labels), so a
/// hijacked record can't send the password elsewhere.
fn check_srv_host(name: &str, host: &str) -> Result<(), String> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let parent = if name.split('.').count() >= 3 {
        name.split_once('.').map_or(name.as_str(), |(_, p)| p)
    } else {
        name.as_str()
    };
    if host.ends_with(&format!(".{parent}")) {
        Ok(())
    } else {
        Err(format!(
            "The SRV record of {name} points outside its domain ({})",
            util::truncate(&host, 60)
        ))
    }
}

/// The hosts and TXT options of a `mongodb+srv://` name.
async fn srv_lookup(
    name: &str,
    timeout: Duration,
) -> Result<(Vec<(String, u16)>, Vec<(String, String)>), String> {
    use hickory_resolver::proto::rr::{Name, RData, RecordType};

    let resolver = super::dns::build_resolver(None, timeout)?;
    let fqdn = |n: &str| {
        Name::from_utf8(format!("{}.", n.trim_end_matches('.')))
            .map_err(|e| format!("Invalid host name \"{name}\": {e}"))
    };
    let srv = resolver
        .lookup(fqdn(&format!("_mongodb._tcp.{name}"))?, RecordType::SRV)
        .await
        .map_err(|e| format!("SRV lookup for _mongodb._tcp.{name} failed: {e}"))?;
    let mut records: Vec<(u16, String, u16)> = srv
        .answers()
        .iter()
        .filter_map(|r| match &r.data {
            RData::SRV(s) => Some((
                s.priority,
                s.target.to_ascii().trim_end_matches('.').to_string(),
                s.port,
            )),
            _ => None,
        })
        .collect();
    if records.is_empty() {
        return Err(format!("No SRV records for _mongodb._tcp.{name}"));
    }
    records.sort_by_key(|(priority, _, _)| *priority);
    for (_, host, _) in &records {
        check_srv_host(name, host)?;
    }
    let options = match resolver.lookup(fqdn(name)?, RecordType::TXT).await {
        Ok(txt) => {
            let texts: Vec<String> = txt
                .answers()
                .iter()
                .filter_map(|r| match &r.data {
                    RData::TXT(t) => Some(
                        t.txt_data
                            .iter()
                            .map(|part| String::from_utf8_lossy(part).into_owned())
                            .collect(),
                    ),
                    _ => None,
                })
                .collect();
            if texts.len() > 1 {
                return Err(format!("{name} has more than one TXT record"));
            }
            texts.first().map(|t| txt_options(t)).unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };
    Ok((
        records
            .into_iter()
            .map(|(_, host, port)| (host, port))
            .collect(),
        options,
    ))
}

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

fn io_err(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        "Connection closed by the server".into()
    } else {
        util::tidy_io_error(&e)
    }
}

/// `doc` as an OP_MSG with one body section.
fn op_msg(request_id: i32, doc: &Document) -> Vec<u8> {
    let body = doc.to_bytes();
    let mut out = Vec::with_capacity(body.len() + 21);
    out.extend_from_slice(&((body.len() + 21) as i32).to_le_bytes());
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // responseTo
    out.extend_from_slice(&OP_MSG.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // flagBits
    out.push(0); // section kind 0: the body
    out.extend(body);
    out
}

/// The body document of an OP_MSG (after its 16-byte header).
fn parse_op_msg(msg: &[u8]) -> Result<Document, String> {
    let bad = || "Not a MongoDB server (malformed reply)".to_string();
    let flags = u32::from_le_bytes(
        msg.get(..4)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?,
    );
    // A trailing CRC-32C (checksumPresent) is not part of the sections.
    let end = if flags & 1 != 0 {
        msg.len().checked_sub(4).ok_or_else(bad)?
    } else {
        msg.len()
    };
    let mut at = 4;
    while at < end {
        let kind = msg[at];
        at += 1;
        let rest = msg.get(at..end).ok_or_else(bad)?;
        match kind {
            0 => return Document::from_bytes(rest).map(|(doc, _)| doc),
            1 => {
                let size = rest.get(..4).ok_or_else(bad)?;
                let size = i32::from_le_bytes(size.try_into().map_err(|_| bad())?);
                at += usize::try_from(size)
                    .ok()
                    .filter(|s| *s >= 4)
                    .ok_or_else(bad)?;
            }
            _ => return Err(bad()),
        }
    }
    Err(bad())
}

/// Read one OP_MSG answering `request_id`.
async fn read_reply(s: &mut Stream, request_id: i32) -> Result<Document, String> {
    let mut header = [0u8; 16];
    s.read_exact(&mut header).await.map_err(io_err)?;
    let field =
        |i: usize| i32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    let (len, response_to, op) = (field(0), field(8), field(12));
    if op == OP_REPLY && response_to == request_id {
        return Err("The server is too old (MongoDB 3.6 or later is needed)".into());
    }
    let len = usize::try_from(len)
        .ok()
        .filter(|l| (21..=MAX_MESSAGE).contains(l));
    let (Some(len), true, true) = (len, op == OP_MSG, response_to == request_id) else {
        return Err("Not a MongoDB server (unexpected reply)".into());
    };
    let mut body = vec![0u8; len - 16];
    s.read_exact(&mut body).await.map_err(io_err)?;
    parse_op_msg(&body)
}

/// A command that failed: `ok: 0` with a code and message.
#[derive(Debug)]
struct CommandError {
    code: Option<i64>,
    message: String,
}

impl CommandError {
    fn text(&self) -> String {
        match self.code {
            Some(code) => format!(
                "Server error {code}: {}",
                util::truncate(&self.message, 160)
            ),
            None => format!("Server error: {}", util::truncate(&self.message, 160)),
        }
    }
}

enum Failure {
    Down(String),
    Outcome(Box<CheckOutcome>),
    Command(CommandError),
}

impl From<String> for Failure {
    fn from(msg: String) -> Self {
        Failure::Down(msg)
    }
}

impl From<&str> for Failure {
    fn from(msg: &str) -> Self {
        Failure::Down(msg.to_string())
    }
}

impl Failure {
    fn message(self) -> Result<String, Box<CheckOutcome>> {
        match self {
            Failure::Down(msg) => Ok(msg),
            Failure::Command(err) => Ok(err.text()),
            Failure::Outcome(outcome) => Err(outcome),
        }
    }
}

struct Conn {
    stream: Stream,
    next_id: i32,
}

impl Conn {
    /// Run `doc`; `ok: 0` becomes [`Failure::Command`].
    async fn command(&mut self, doc: &Document) -> Result<Document, Failure> {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        self.stream
            .write_all(&op_msg(id, doc))
            .await
            .map_err(io_err)?;
        self.stream.flush().await.map_err(io_err)?;
        let reply = read_reply(&mut self.stream, id).await?;
        if reply.number("ok") == Some(1.0) {
            return Ok(reply);
        }
        Err(Failure::Command(CommandError {
            code: reply.number("code").map(|c| c as i64),
            message: reply.str("errmsg").unwrap_or("command failed").to_string(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// MongoDB's SCRAM-SHA-1 hashes the password with the user name first.
fn sha1_password(user: &str, password: &str) -> String {
    hex::encode(md5::Md5::digest(
        format!("{user}:mongo:{password}").as_bytes(),
    ))
}

fn auth_error(err: Failure) -> Failure {
    match err {
        Failure::Command(CommandError {
            code: Some(AUTHENTICATION_FAILED),
            ..
        }) => "Authentication failed: wrong user name or password".into(),
        Failure::Command(e) => format!("Authentication failed: {}", e.text()).into(),
        other => other,
    }
}

async fn authenticate(
    c: &mut Conn,
    hash: Hash,
    user: &str,
    password: &str,
    source: &str,
) -> Result<(), Failure> {
    let nonce = scram::nonce();
    let bare = format!("n={},r={nonce}", scram::saslname(user));
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", hash.mechanism())
        .with(
            "payload",
            Bson::Binary(0, format!("n,,{bare}").into_bytes()),
        )
        .with("autoAuthorize", 1)
        .with("options", Document::new().with("skipEmptyExchange", true))
        .with("$db", source);
    let reply = c.command(&start).await.map_err(auth_error)?;
    let conversation = reply
        .get("conversationId")
        .cloned()
        .ok_or("Unexpected SCRAM reply from the server")?;
    let server_first = String::from_utf8_lossy(
        reply
            .binary("payload")
            .ok_or("Unexpected SCRAM reply from the server")?,
    )
    .into_owned();
    let secret = match hash {
        Hash::Sha256 => password.to_string(),
        Hash::Sha1 => sha1_password(user, password),
    };
    let (b, n) = (bare.clone(), nonce.clone());
    let fin = tokio::task::spawn_blocking(move || {
        scram::client_final(hash, secret.as_bytes(), &b, &server_first, &n)
    })
    .await
    .map_err(|_| "SCRAM computation failed".to_string())??;
    let cont = |payload: Vec<u8>| {
        Document::new()
            .with("saslContinue", 1)
            .with("conversationId", conversation.clone())
            .with("payload", Bson::Binary(0, payload))
            .with("$db", source)
    };
    let reply = c
        .command(&cont(fin.message.into_bytes()))
        .await
        .map_err(auth_error)?;
    let server_final =
        String::from_utf8_lossy(reply.binary("payload").unwrap_or_default()).into_owned();
    if !scram::server_final_ok(&server_final, &fin.server_signature) {
        return Err("The server failed to prove it knows the password".into());
    }
    if !reply.bool("done") {
        // Servers that ignore skipEmptyExchange want one more empty round.
        let reply = c.command(&cont(Vec::new())).await.map_err(auth_error)?;
        if !reply.bool("done") {
            return Err("The server did not finish the SCRAM conversation".into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------

/// What `hello` said about the server's place in the deployment.
fn role(hello: &Document) -> String {
    let set = hello.str("setName");
    let primary = hello.bool("isWritablePrimary") || hello.bool("ismaster");
    let of_set = |what: &str| match set {
        Some(set) => format!("{what} of {set}"),
        None => what.to_string(),
    };
    if hello.str("msg") == Some("isdbgrid") {
        "mongos".into()
    } else if primary && set.is_some() {
        of_set("primary")
    } else if primary {
        "standalone".into()
    } else if hello.bool("arbiterOnly") {
        of_set("arbiter")
    } else if hello.bool("secondary") {
        match hello.str("primary") {
            Some(p) => format!(
                "{} (primary {})",
                of_set("secondary"),
                util::truncate(p, 60)
            ),
            None => format!("{}, no primary known", of_set("secondary")),
        }
    } else if hello.bool("isreplicaset") {
        "replica set member without a configuration".into()
    } else {
        of_set("member (not primary or secondary)")
    }
}

/// With `replicaSet=…`: the server must be in that set, and the set needs a primary.
fn check_replica_set(hello: &Document, wanted: &str) -> Result<(), String> {
    match hello.str("setName") {
        None => Err(format!(
            "The server is not a member of replica set {wanted}"
        )),
        Some(set) if set != wanted => Err(format!(
            "The server belongs to replica set {set}, not {wanted}"
        )),
        Some(set) => {
            let primary = hello.bool("isWritablePrimary")
                || hello.bool("ismaster")
                || hello.get("primary").is_some();
            if primary {
                Ok(())
            } else {
                Err(format!("Replica set {set} has no primary"))
            }
        }
    }
}

struct Found {
    version: Option<String>,
    role: String,
    logged_in: bool,
    cert: Option<CertSummary>,
    tls_version: Option<&'static str>,
}

/// Connect to the first seed that accepts (TLS when asked for).
async fn connect(
    seeds: &[(String, u16)],
    tls: bool,
    verify: Verify,
    timeout: Duration,
) -> Result<(Stream, Option<CertSummary>, Option<&'static str>), Failure> {
    let mut last = String::from("No hosts to connect to");
    for (host, port) in seeds {
        let (tcp, addr) = match conn::tcp(host, *port).await {
            Ok(found) => found,
            Err(e) => {
                last = e;
                continue;
            }
        };
        if !tls {
            return Ok((Box::new(tcp), None, None));
        }
        return match tls::upgrade_with(tcp, host, verify, &[]).await {
            Ok(stream) => {
                let (cert, version) = (tls::peer_cert(&stream), tls::version_label(&stream));
                Ok((Box::new(stream), cert, Some(version)))
            }
            Err(err) => Err(Failure::Outcome(Box::new(
                conn::tls_failure(&err, host, addr, verify == Verify::None, timeout).await,
            ))),
        };
    }
    Err(Failure::Down(last))
}

async fn session(
    ep: &Endpoint,
    monitor: &Monitor,
    forced: Option<Hash>,
    timeout: Duration,
) -> Result<Found, Failure> {
    let (seeds, txt) = if ep.srv {
        srv_lookup(&ep.host, timeout).await?
    } else {
        let mut seeds = vec![(ep.host.clone(), ep.port)];
        seeds.extend(ep.more_hosts.iter().cloned());
        (seeds, Vec::new())
    };
    // Options in the address win over those from DNS.
    let option = |key: &str| {
        ep.query(key).or_else(|| {
            txt.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.as_str())
        })
    };
    let verify = verify(ep).unless_ignored(monitor.ignore_tls);
    let (stream, cert, tls_version) = connect(&seeds, ep.tls, verify, timeout).await?;
    let mut c = Conn { stream, next_id: 0 };

    let user = monitor.basic_auth_user.as_deref();
    let source = option("authSource")
        .filter(|s| !s.is_empty())
        .unwrap_or(if ep.path.is_empty() {
            "admin"
        } else {
            &ep.path
        });
    let hello = |name: &str| {
        let mut doc = Document::new().with(name, 1).with("$db", "admin");
        if let Some(user) = user {
            doc = doc.with("saslSupportedMechs", format!("{source}.{user}"));
        }
        if option("loadBalanced").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            doc = doc.with("loadBalanced", true);
        }
        // Client metadata may only come with a connection's first handshake.
        if name == "hello" {
            // The server requires driver.name, driver.version and os.type.
            let client = Document::new()
                .with("application", Document::new().with("name", "Vigilant Vole"))
                .with(
                    "driver",
                    Document::new()
                        .with("name", "vigilant-vole")
                        .with("version", env!("CARGO_PKG_VERSION")),
                )
                .with("os", Document::new().with("type", std::env::consts::OS));
            doc = doc.with("client", client);
        }
        doc
    };
    let reply = match c.command(&hello("hello")).await {
        Err(Failure::Command(CommandError {
            code: Some(COMMAND_NOT_FOUND),
            ..
        })) => c.command(&hello("isMaster")).await?,
        other => other?,
    };
    let version = match c
        .command(&Document::new().with("buildInfo", 1).with("$db", "admin"))
        .await
    {
        Ok(info) => info.str("version").map(str::to_string),
        Err(_) => None,
    };
    if let Some(user) = user {
        let hash = forced.unwrap_or_else(|| {
            let offered = reply.array("saslSupportedMechs").unwrap_or_default();
            if offered.iter().any(|m| m == &Bson::from("SCRAM-SHA-256")) {
                Hash::Sha256
            } else {
                Hash::Sha1
            }
        });
        authenticate(
            &mut c,
            hash,
            user,
            monitor.basic_auth_pass.as_deref().unwrap_or_default(),
            source,
        )
        .await?;
    }
    c.command(&Document::new().with("ping", 1).with("$db", "admin"))
        .await?;
    if let Some(set) = option("replicaSet").filter(|s| !s.is_empty()) {
        check_replica_set(&reply, set)?;
    }
    let _ = c.stream.shutdown().await;
    Ok(Found {
        version,
        role: role(&reply),
        logged_in: user.is_some(),
        cert,
        tls_version,
    })
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Mongodb, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let forced = match mechanism(&ep) {
        Ok(m) => m,
        Err(e) => return CheckOutcome::down(e),
    };
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    match tokio::time::timeout(timeout, session(&ep, monitor, forced, timeout)).await {
        Ok(Ok(found)) => {
            let ms = util::elapsed_ms(start);
            let product = match &found.version {
                Some(v) => format!("MongoDB {}", util::truncate(v, 30)),
                None => "MongoDB".to_string(),
            };
            let checked = if found.logged_in {
                "login and ping OK"
            } else {
                "ping OK"
            };
            conn::up(
                ms,
                format!("{product} · {} · {checked}", found.role),
                found.cert.as_ref(),
                found.tls_version,
            )
        }
        Ok(Err(failure)) => match failure.message() {
            Ok(msg) => CheckOutcome::down(msg),
            Err(outcome) => *outcome,
        },
        Err(_) => conn::timed_out(&format!("MongoDB on {}:{}", ep.host, ep.port), timeout),
    }
}

#[cfg(test)]
mod tests;
