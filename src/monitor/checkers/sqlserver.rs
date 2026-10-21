//! SQL Server checker: `target` =
//! `sqlserver://host[:port][/database][?encrypt=…&trustServerCertificate=…]`.
//!
//! Speaks TDS 7.4 directly: PRELOGIN (server version, encryption
//! negotiation), the TLS handshake carried inside PRELOGIN packets as SQL
//! Server requires, LOGIN7 with SQL Server authentication, then `SELECT 1`
//! as a SQL batch. Without a user name only PRELOGIN runs (and the TLS
//! handshake, when the server encrypts, so the certificate is still
//! checked). The user name and password come from the monitor's
//! credentials; Windows authentication (NTLM/Kerberos) is not supported.
//!
//! `encrypt` follows go-mssqldb's names with libpq-like defaults:
//! - absent (default): encrypt the whole session when the server can, like
//!   `sslmode=prefer`; plain when it can't;
//! - `false`: only the login packet is encrypted (unless the server forces
//!   encryption), the rest of the session is plain;
//! - `disable`: no TLS at all (fails against a server that forces it);
//! - `true`: encrypt or fail;
//! - `strict`: TDS 8.0, TLS from the first byte (ALPN `tds/8.0`); the
//!   server must be set up for strict connections (SQL Server 2022+).
//!
//! The certificate is only verified when asked for with
//! `trustServerCertificate=false` together with `encrypt=true|strict` (and not
//! when the monitor ignores TLS errors); its expiry is reported whenever TLS
//! is used. Named instances (`host\instance`, SQL Browser) are not resolved:
//! give the instance's port. Response time = connect + login + query.

use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use super::conn::{self, Io, SslMode};
use super::endpoint::{self, Endpoint};
use super::tls::{self, CertSummary, Verify};
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

/// TDS packet types.
const SQL_BATCH: u8 = 0x01;
const REPLY: u8 = 0x04;
const LOGIN7: u8 = 0x10;
const PRELOGIN: u8 = 0x12;
/// Packet status: last packet of the message.
const EOM: u8 = 0x01;
const PACKET_SIZE: usize = 4096;
const HEADER: usize = 8;
const MAX_MESSAGE: usize = 1024 * 1024;

/// PRELOGIN `ENCRYPTION` values.
const ENCRYPT_OFF: u8 = 0x00;
const ENCRYPT_ON: u8 = 0x01;
const ENCRYPT_NOT_SUP: u8 = 0x02;
const ENCRYPT_REQ: u8 = 0x03;

/// TDS 7.4 (SQL Server 2012 and later), as LOGIN7 sends it (little-endian).
const TDS_74: u32 = 0x7400_0004;
const ALPN_TDS8: &[u8] = b"tds/8.0";

type Stream = Box<dyn Io>;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// The `encrypt` option, from least to most protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Encrypt {
    /// `disable`: no TLS at all.
    Disable,
    /// `false`: only the login is encrypted.
    Login,
    /// Default: the whole session when the server can.
    Prefer,
    /// `true`: the whole session, or fail.
    Require,
    /// `strict`: TDS 8.0, TLS before anything else.
    Strict,
}

/// What the target asks for: encryption and certificate verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub encrypt: Encrypt,
    /// Verification before "ignore TLS errors" is applied.
    pub verify: Verify,
}

fn yes_no(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

impl Settings {
    pub fn of(ep: &Endpoint) -> Result<Self, String> {
        let encrypt = match ep.query("encrypt") {
            None => Encrypt::Prefer,
            Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "disable" => Encrypt::Disable,
                "false" | "no" | "optional" => Encrypt::Login,
                "true" | "yes" | "mandatory" => Encrypt::Require,
                "strict" => Encrypt::Strict,
                _ => {
                    return Err(format!(
                        "Unsupported encrypt \"{}\" (use disable, false, true or strict)",
                        util::truncate(raw, 20)
                    ));
                }
            },
        };
        let trust = match ep.query("trustServerCertificate") {
            None => None,
            Some(raw) => Some(yes_no(raw).ok_or_else(|| {
                format!(
                    "Unsupported trustServerCertificate \"{}\" (use true or false)",
                    util::truncate(raw, 20)
                )
            })?),
        };
        let verify = if trust == Some(false) && encrypt >= Encrypt::Require {
            Verify::Full
        } else {
            Verify::None
        };
        Ok(Settings { encrypt, verify })
    }

    /// How well a password sent to this target is protected, on the
    /// `sslmode` scale (stored passwords need re-entry when it drops).
    pub fn protection(self) -> SslMode {
        match (self.encrypt, self.verify) {
            (Encrypt::Disable, _) => SslMode::Disable,
            (Encrypt::Login | Encrypt::Prefer, _) => SslMode::Prefer,
            (_, Verify::None) => SslMode::Require,
            (_, Verify::Ca) => SslMode::VerifyCa,
            (_, Verify::Full) => SslMode::VerifyFull,
        }
    }

    /// The PRELOGIN `ENCRYPTION` value the client sends.
    fn prelogin_value(self) -> u8 {
        match self.encrypt {
            Encrypt::Disable => ENCRYPT_NOT_SUP,
            Encrypt::Login => ENCRYPT_OFF,
            Encrypt::Prefer | Encrypt::Require => ENCRYPT_ON,
            // TLS is already up; SqlClient sends NOT_SUP so nothing is negotiated twice.
            Encrypt::Strict => ENCRYPT_NOT_SUP,
        }
    }
}

/// What PRELOGIN settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Plain,
    /// Only LOGIN7 travels encrypted.
    LoginOnly,
    Full,
}

/// The client's `ENCRYPTION` against the server's answer (MS-TDS 2.2.6.5).
fn negotiate(settings: Settings, server: u8) -> Result<Mode, String> {
    let sent = settings.prelogin_value();
    match server {
        ENCRYPT_NOT_SUP if settings.encrypt == Encrypt::Require => {
            Err("The server doesn't support encryption (encrypt=true)".into())
        }
        ENCRYPT_NOT_SUP => Ok(Mode::Plain),
        ENCRYPT_ON | ENCRYPT_REQ if sent == ENCRYPT_NOT_SUP => {
            Err("The server requires encryption (remove encrypt=disable)".into())
        }
        ENCRYPT_ON | ENCRYPT_REQ => Ok(Mode::Full),
        ENCRYPT_OFF if sent == ENCRYPT_NOT_SUP => Ok(Mode::Plain),
        ENCRYPT_OFF if sent == ENCRYPT_OFF => Ok(Mode::LoginOnly),
        ENCRYPT_OFF => Ok(Mode::Full),
        other => Err(format!(
            "Not a SQL Server (unknown encryption option {other:#04x})"
        )),
    }
}

// ---------------------------------------------------------------------------
// Packets
// ---------------------------------------------------------------------------

fn io_err(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        "Connection closed by the server".into()
    } else {
        util::tidy_io_error(&e)
    }
}

/// `payload` as one TDS message (split into packets of [`PACKET_SIZE`]).
fn frame(kind: u8, payload: &[u8], packet_id: &mut u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + HEADER);
    let mut chunks: Vec<&[u8]> = payload.chunks(PACKET_SIZE - HEADER).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let last = chunks.len() - 1;
    for (i, chunk) in chunks.into_iter().enumerate() {
        let len = (chunk.len() + HEADER) as u16;
        *packet_id = packet_id.wrapping_add(1);
        out.extend_from_slice(&[kind, if i == last { EOM } else { 0 }]);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&[0, 0, *packet_id, 0]);
        out.extend_from_slice(chunk);
    }
    out
}

async fn send<S: AsyncWrite + Unpin + ?Sized>(
    s: &mut S,
    kind: u8,
    payload: &[u8],
) -> Result<(), String> {
    s.write_all(&frame(kind, payload, &mut 0))
        .await
        .map_err(io_err)?;
    s.flush().await.map_err(io_err)
}

/// Read one message of type `kind`. `skip_tls` drops whole TLS records
/// found between packets (after a login-only handshake a TLS 1.3 server may
/// still send session tickets the client never reads through TLS).
async fn read_message<S: AsyncRead + Unpin + ?Sized>(
    s: &mut S,
    kind: u8,
    skip_tls: bool,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let mut h = [0u8; HEADER];
        s.read_exact(&mut h).await.map_err(io_err)?;
        if skip_tls && (0x14..=0x17).contains(&h[0]) && h[1] == 0x03 {
            let len = usize::from(u16::from_be_bytes([h[3], h[4]]));
            let mut rest = vec![0u8; len.saturating_sub(HEADER - 5)];
            s.read_exact(&mut rest).await.map_err(io_err)?;
            continue;
        }
        if h[0] != kind {
            return Err("Not a SQL Server (unexpected reply)".into());
        }
        let len = usize::from(u16::from_be_bytes([h[2], h[3]]));
        if len < HEADER || body.len() + len > MAX_MESSAGE {
            return Err("Not a SQL Server (bad packet length)".into());
        }
        let at = body.len();
        body.resize(at + len - HEADER, 0);
        s.read_exact(&mut body[at..]).await.map_err(io_err)?;
        if h[1] & EOM != 0 {
            return Ok(body);
        }
    }
}

/// SQL Server carries the TLS handshake inside PRELOGIN packets. While
/// `handshake` is set, writes are buffered and sent as one PRELOGIN packet
/// on flush. Afterwards TLS records go on the wire as they are. Reads take
/// TLS out of PRELOGIN packets until the first packet boundary that starts
/// with anything else (a TLS 1.3 server can send its session tickets in a
/// last PRELOGIN packet after the client's handshake is over), then pass
/// everything through.
pub struct PreloginTls<S> {
    inner: S,
    pub handshake: bool,
    unwrap_reads: bool,
    /// Payload bytes left in the PRELOGIN packet being read.
    read_left: usize,
    header: [u8; HEADER],
    header_len: usize,
    /// Handshake bytes written but not framed yet.
    staged: Vec<u8>,
    /// Framed bytes not on the wire yet.
    out: Vec<u8>,
    out_pos: usize,
    packet_id: u8,
}

impl<S> PreloginTls<S> {
    pub fn new(inner: S) -> Self {
        PreloginTls {
            inner,
            handshake: true,
            unwrap_reads: true,
            read_left: 0,
            header: [0; HEADER],
            header_len: 0,
            staged: Vec::new(),
            out: Vec::new(),
            out_pos: 0,
            packet_id: 0,
        }
    }
}

impl<S: AsyncWrite + Unpin> PreloginTls<S> {
    /// Frame what the handshake wrote and push it out.
    fn poll_write_out(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.out_pos == self.out.len() && !self.staged.is_empty() {
            self.out = frame(PRELOGIN, &self.staged, &mut self.packet_id);
            self.out_pos = 0;
            self.staged.clear();
        }
        while self.out_pos < self.out.len() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.out[self.out_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
            }
            self.out_pos += n;
        }
        self.out.clear();
        self.out_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for PreloginTls<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // A handshake message must leave before its answer can come.
        if !this.staged.is_empty() || this.out_pos < this.out.len() {
            ready!(this.poll_write_out(cx))?;
        }
        loop {
            if !this.unwrap_reads {
                return Pin::new(&mut this.inner).poll_read(cx, buf);
            }
            if this.read_left > 0 {
                let mut tmp = [0u8; 2048];
                let max = this.read_left.min(buf.remaining()).min(tmp.len());
                let mut part = ReadBuf::new(&mut tmp[..max]);
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut part))?;
                if part.filled().is_empty() {
                    return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into()));
                }
                this.read_left -= part.filled().len();
                buf.put_slice(part.filled());
                return Poll::Ready(Ok(()));
            }
            // A packet header; its first byte alone decides whether there is one.
            while this.header_len < HEADER {
                let want = if this.header_len == 0 { 1 } else { HEADER };
                let mut part = ReadBuf::new(&mut this.header[this.header_len..want]);
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut part))?;
                let n = part.filled().len();
                if n == 0 {
                    return if this.header_len == 0 {
                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into()))
                    };
                }
                if this.header_len == 0 && this.header[0] != PRELOGIN {
                    // Raw TLS from here on (or garbage the TLS layer rejects).
                    // The peer may finish its handshake and send data before
                    // this side's handshake has noticed it is over.
                    this.unwrap_reads = false;
                    buf.put_slice(&this.header[..1]);
                    return Poll::Ready(Ok(()));
                }
                this.header_len += n;
            }
            this.header_len = 0;
            let len = usize::from(u16::from_be_bytes([this.header[2], this.header[3]]));
            if len < HEADER {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bad PRELOGIN packet",
                )));
            }
            this.read_left = len - HEADER;
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PreloginTls<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.handshake {
            this.staged.extend_from_slice(data);
            return Poll::Ready(Ok(data.len()));
        }
        ready!(this.poll_write_out(cx))?;
        Pin::new(&mut this.inner).poll_write(cx, data)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_write_out(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_write_out(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// PRELOGIN
// ---------------------------------------------------------------------------

/// PRELOGIN option tokens.
const PL_VERSION: u8 = 0x00;
const PL_ENCRYPTION: u8 = 0x01;
const PL_INSTOPT: u8 = 0x02;
const PL_THREADID: u8 = 0x03;
const PL_MARS: u8 = 0x04;
const PL_END: u8 = 0xFF;

/// Encode PRELOGIN options (`(token, data)`, in order).
fn prelogin_options(options: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut head = Vec::new();
    let mut data = Vec::new();
    let start = options.len() * 5 + 1;
    for (token, value) in options {
        head.push(*token);
        head.extend_from_slice(&((start + data.len()) as u16).to_be_bytes());
        head.extend_from_slice(&(value.len() as u16).to_be_bytes());
        data.extend_from_slice(value);
    }
    head.push(PL_END);
    head.extend(data);
    head
}

fn client_prelogin(encryption: u8) -> Vec<u8> {
    let (major, minor) = (
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
    );
    let version = [
        major.parse().unwrap_or(0),
        minor.parse().unwrap_or(0),
        0,
        0,
        0,
        0,
    ];
    prelogin_options(&[
        (PL_VERSION, version.to_vec()),
        (PL_ENCRYPTION, vec![encryption]),
        (PL_INSTOPT, vec![0]),
        (PL_THREADID, std::process::id().to_be_bytes().to_vec()),
        (PL_MARS, vec![0]),
    ])
}

/// The server's PRELOGIN answer: its version and `ENCRYPTION` value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerPrelogin {
    version: Option<String>,
    encryption: u8,
}

fn parse_prelogin(body: &[u8]) -> Result<ServerPrelogin, String> {
    let bad = || "Not a SQL Server (bad PRELOGIN answer)".to_string();
    let (mut version, mut encryption) = (None, None);
    let mut at = 0;
    loop {
        let token = *body.get(at).ok_or_else(bad)?;
        if token == PL_END {
            break;
        }
        let entry = body.get(at + 1..at + 5).ok_or_else(bad)?;
        let offset = usize::from(u16::from_be_bytes([entry[0], entry[1]]));
        let len = usize::from(u16::from_be_bytes([entry[2], entry[3]]));
        let value = body.get(offset..offset + len).ok_or_else(bad)?;
        match token {
            PL_VERSION if len >= 4 => {
                version = Some(format!(
                    "{}.{}.{}",
                    value[0],
                    value[1],
                    u16::from_be_bytes([value[2], value[3]])
                ));
            }
            PL_ENCRYPTION if len >= 1 => encryption = Some(value[0]),
            _ => {}
        }
        at += 5;
    }
    Ok(ServerPrelogin {
        version,
        encryption: encryption.ok_or_else(bad)?,
    })
}

// ---------------------------------------------------------------------------
// LOGIN7
// ---------------------------------------------------------------------------

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// MS-TDS 2.2.6.4: every byte of the UTF-16LE password has its nibbles
/// swapped and is XORed with 0xA5.
fn obfuscate(password: &str) -> Vec<u8> {
    utf16(password)
        .into_iter()
        .map(|b| b.rotate_left(4) ^ 0xA5)
        .collect()
}

struct Login<'a> {
    user: &'a str,
    password: &'a str,
    database: &'a str,
    server: &'a str,
}

/// Size of LOGIN7's fixed part (up to and including `cbSSPILong`).
const LOGIN7_FIXED: usize = 94;

fn login7(login: &Login<'_>) -> Vec<u8> {
    let mut fixed = Vec::with_capacity(LOGIN7_FIXED);
    let mut data = Vec::new();
    fixed.extend_from_slice(&[0; 4]); // Length, filled in below
    fixed.extend_from_slice(&TDS_74.to_le_bytes());
    fixed.extend_from_slice(&(PACKET_SIZE as u32).to_le_bytes());
    fixed.extend_from_slice(&0x0700_0000u32.to_le_bytes()); // ClientProgVer
    fixed.extend_from_slice(&std::process::id().to_le_bytes());
    fixed.extend_from_slice(&0u32.to_le_bytes()); // ConnectionID
    // OptionFlags1: USE_DB_ON | INIT_DB_FATAL | SET_LANG_ON; OptionFlags2:
    // INIT_LANG_FATAL | ODBC (and integrated security off); TypeFlags; OptionFlags3.
    fixed.extend_from_slice(&[0xE0, 0x03, 0x00, 0x00]);
    fixed.extend_from_slice(&0i32.to_le_bytes()); // ClientTimeZone
    fixed.extend_from_slice(&0x0409u32.to_le_bytes()); // ClientLCID
    let hostname = "vigilant-vole";
    let fields: [(Vec<u8>, usize); 9] = [
        (utf16(hostname), hostname.encode_utf16().count()),
        (utf16(login.user), login.user.encode_utf16().count()),
        (
            obfuscate(login.password),
            login.password.encode_utf16().count(),
        ),
        (utf16("Vigilant Vole"), 13),
        (utf16(login.server), login.server.encode_utf16().count()),
        (Vec::new(), 0),                                    // extension
        (utf16(hostname), hostname.encode_utf16().count()), // CltIntName
        (Vec::new(), 0),                                    // language
        (utf16(login.database), login.database.encode_utf16().count()),
    ];
    for (bytes, chars) in fields {
        fixed.extend_from_slice(&((LOGIN7_FIXED + data.len()) as u16).to_le_bytes());
        fixed.extend_from_slice(&(chars as u16).to_le_bytes());
        data.extend(bytes);
    }
    fixed.extend_from_slice(&[0; 6]); // ClientID
    // SSPI, AtchDBFile, ChangePassword: empty.
    for _ in 0..3 {
        fixed.extend_from_slice(&((LOGIN7_FIXED + data.len()) as u16).to_le_bytes());
        fixed.extend_from_slice(&0u16.to_le_bytes());
    }
    fixed.extend_from_slice(&0u32.to_le_bytes()); // cbSSPILong
    debug_assert_eq!(fixed.len(), LOGIN7_FIXED);
    let total = (fixed.len() + data.len()) as u32;
    fixed[..4].copy_from_slice(&total.to_le_bytes());
    fixed.extend(data);
    fixed
}

// ---------------------------------------------------------------------------
// Token stream
// ---------------------------------------------------------------------------

/// What a reply's token stream said.
#[derive(Debug, Default, PartialEq, Eq)]
struct Reply {
    /// LOGINACK's server version.
    login_version: Option<String>,
    /// The first ERROR token: (number, message).
    error: Option<(i32, String)>,
    /// The first column of every row, when it is an integer.
    values: Vec<i64>,
    /// A DONE token flagged an error.
    failed: bool,
}

/// A result column's type: how its values are laid out in a ROW.
#[derive(Debug, Clone, Copy)]
enum Column {
    /// Fixed length, `bool` = it is an integer.
    Fixed(usize, bool),
    /// One length byte before the value, `bool` = it is an integer.
    ByteLen(bool),
}

struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let out = self
            .buf
            .get(self.at..self.at + n)
            .ok_or("Not a SQL Server (truncated reply)")?;
        self.at += n;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// A string of `chars` UTF-16 code units.
    fn text(&mut self, chars: usize) -> Result<String, String> {
        let units: Vec<u16> = self
            .take(chars * 2)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        Ok(String::from_utf16_lossy(&units))
    }
}

fn int_value(bytes: &[u8]) -> Option<i64> {
    Some(match *bytes {
        [a] => i64::from(a),
        [a, b] => i64::from(i16::from_le_bytes([a, b])),
        [a, b, c, d] => i64::from(i32::from_le_bytes([a, b, c, d])),
        [a, b, c, d, e, f, g, h] => i64::from_le_bytes([a, b, c, d, e, f, g, h]),
        _ => return None,
    })
}

fn parse_reply(body: &[u8]) -> Result<Reply, String> {
    let mut reply = Reply::default();
    let mut columns: Vec<Column> = Vec::new();
    let mut c = Cursor { buf: body, at: 0 };
    while c.at < body.len() {
        match c.u8()? {
            // COLMETADATA
            0x81 => {
                let count = c.u16()?;
                columns.clear();
                if count == 0xFFFF {
                    continue;
                }
                for _ in 0..count {
                    c.take(6)?; // UserType, Flags
                    let column = match c.u8()? {
                        0x1F => Column::Fixed(0, false),
                        0x30 | 0x32 => Column::Fixed(1, true),
                        0x34 => Column::Fixed(2, true),
                        0x38 => Column::Fixed(4, true),
                        0x7F => Column::Fixed(8, true),
                        0x3A | 0x3B | 0x7A => Column::Fixed(4, false),
                        0x3C..=0x3E => Column::Fixed(8, false),
                        kind @ (0x26 | 0x68 | 0x6D | 0x6E | 0x6F | 0x24) => {
                            c.u8()?; // maximum length
                            Column::ByteLen(matches!(kind, 0x26 | 0x68))
                        }
                        other => {
                            return Err(format!(
                                "Unexpected column type {other:#04x} in the reply"
                            ));
                        }
                    };
                    let name_len = usize::from(c.u8()?);
                    c.take(name_len * 2)?;
                    columns.push(column);
                }
            }
            // ROW
            0xD1 => {
                for (i, column) in columns.iter().enumerate() {
                    let (bytes, int) = match *column {
                        Column::Fixed(n, int) => (c.take(n)?, int),
                        Column::ByteLen(int) => {
                            let n = usize::from(c.u8()?);
                            (c.take(n)?, int)
                        }
                    };
                    if i == 0
                        && int
                        && let Some(v) = int_value(bytes)
                    {
                        reply.values.push(v);
                    }
                }
            }
            // ERROR, INFO
            kind @ (0xAA | 0xAB) => {
                let len = usize::from(c.u16()?);
                let mut t = Cursor {
                    buf: c.take(len)?,
                    at: 0,
                };
                let number = t.u32()? as i32;
                t.take(2)?; // State, Class
                let chars = usize::from(t.u16()?);
                let message = t.text(chars)?;
                if kind == 0xAA && reply.error.is_none() {
                    reply.error = Some((number, message));
                }
            }
            // LOGINACK
            0xAD => {
                let len = usize::from(c.u16()?);
                let mut t = Cursor {
                    buf: c.take(len)?,
                    at: 0,
                };
                t.take(5)?; // Interface, TDSVersion
                let name_len = usize::from(t.u8()?);
                t.take(name_len * 2)?;
                let v = t.take(4)?;
                reply.login_version = Some(format!(
                    "{}.{}.{}",
                    v[0],
                    v[1],
                    u16::from_be_bytes([v[2], v[3]])
                ));
            }
            // DONE, DONEPROC, DONEINPROC
            0xFD..=0xFF => {
                let status = c.u16()?;
                c.take(10)?; // CurCmd, DoneRowCount
                reply.failed |= status & 0x0002 != 0;
            }
            // ENVCHANGE, ORDER, TABNAME, COLINFO, SSPI: length-prefixed, skipped.
            0xE3 | 0xA9 | 0xA4 | 0xA5 | 0xED => {
                let len = usize::from(c.u16()?);
                c.take(len)?;
            }
            // SESSIONSTATE
            0xE4 => {
                let len = c.u32()? as usize;
                c.take(len)?;
            }
            // RETURNSTATUS
            0x79 => {
                c.take(4)?;
            }
            // FEATUREEXTACK
            0xAE => loop {
                if c.u8()? == 0xFF {
                    break;
                }
                let len = c.u32()? as usize;
                c.take(len)?;
            },
            other => return Err(format!("Not a SQL Server (unexpected token {other:#04x})")),
        }
    }
    Ok(reply)
}

fn server_error(number: i32, message: &str) -> String {
    format!(
        "Server error {number}: {}",
        util::truncate(message.trim(), 160)
    )
}

/// `SELECT 1` as a SQL batch: ALL_HEADERS (a transaction descriptor, as
/// TDS 7.2+ requires) followed by the UTF-16 text.
fn select_one_batch() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&22u32.to_le_bytes()); // TotalLength
    b.extend_from_slice(&18u32.to_le_bytes()); // HeaderLength
    b.extend_from_slice(&2u16.to_le_bytes()); // transaction descriptor
    b.extend_from_slice(&0u64.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes()); // OutstandingRequestCount
    b.extend(utf16("SELECT 1"));
    b
}

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------

enum Failure {
    Down(String),
    Outcome(Box<CheckOutcome>),
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

/// What a successful check found.
struct Found {
    version: Option<String>,
    logged_in: bool,
    cert: Option<CertSummary>,
    /// "TLS 1.2", or "TLS 1.2 (login only)".
    tls: Option<String>,
}

/// Where the session goes on after PRELOGIN.
enum Channel {
    /// Plain, or encrypted throughout.
    Stream(Stream),
    /// Encrypted login only: LOGIN7 goes through this TLS stream, the rest
    /// of the session over its (plain) transport.
    LoginOnly(Box<tokio_rustls::client::TlsStream<PreloginTls<TcpStream>>>),
}

struct Opened {
    channel: Channel,
    version: Option<String>,
    mode: Mode,
    cert: Option<CertSummary>,
    tls_version: Option<&'static str>,
}

/// Run PRELOGIN over `s`.
async fn prelogin<S: AsyncRead + AsyncWrite + Unpin + ?Sized>(
    s: &mut S,
    settings: Settings,
) -> Result<ServerPrelogin, String> {
    send(s, PRELOGIN, &client_prelogin(settings.prelogin_value())).await?;
    parse_prelogin(&read_message(s, REPLY, false).await?)
}

/// The certificate of a server whose verified handshake failed, fetched
/// again without verification (PRELOGIN replayed first).
async fn fetch_cert(
    addr: std::net::SocketAddr,
    host: String,
    settings: Settings,
) -> Option<CertSummary> {
    let mut tcp = TcpStream::connect(addr).await.ok()?;
    if settings.encrypt == Encrypt::Strict {
        return tls::peer_cert(
            &tls::upgrade_with(tcp, &host, Verify::None, &[ALPN_TDS8])
                .await
                .ok()?,
        );
    }
    let answer = prelogin(&mut tcp, settings).await.ok()?;
    (negotiate(settings, answer.encryption).ok()? != Mode::Plain).then_some(())?;
    tls::peer_cert(
        &tls::upgrade_with(PreloginTls::new(tcp), &host, Verify::None, &[])
            .await
            .ok()?,
    )
}

async fn open(
    ep: &Endpoint,
    settings: Settings,
    ignore_tls: bool,
    timeout: std::time::Duration,
) -> Result<Opened, Failure> {
    let (mut tcp, addr) = conn::tcp(&ep.host, ep.port).await.map_err(Failure::Down)?;
    let verify = settings.verify.unless_ignored(ignore_tls);
    let tls_failed = |err: std::io::Error| async move {
        let fetch = || fetch_cert(addr, ep.host.clone(), settings);
        Failure::Outcome(Box::new(
            conn::tls_failure_with(&err, verify == Verify::None, timeout, fetch).await,
        ))
    };
    if settings.encrypt == Encrypt::Strict {
        let tls = match tls::upgrade_with(tcp, &ep.host, verify, &[ALPN_TDS8]).await {
            Ok(tls) => tls,
            Err(err) => return Err(tls_failed(err).await),
        };
        let (cert, tls_version) = (tls::peer_cert(&tls), Some(tls::version_label(&tls)));
        let mut stream: Stream = Box::new(tls);
        let answer = prelogin(&mut stream, settings).await?;
        return Ok(Opened {
            channel: Channel::Stream(stream),
            version: answer.version,
            mode: Mode::Full,
            cert,
            tls_version,
        });
    }
    let answer = prelogin(&mut tcp, settings).await?;
    let mode = negotiate(settings, answer.encryption)?;
    if mode == Mode::Plain {
        return Ok(Opened {
            channel: Channel::Stream(Box::new(tcp)),
            version: answer.version,
            mode,
            cert: None,
            tls_version: None,
        });
    }
    let mut tls = match tls::upgrade_with(PreloginTls::new(tcp), &ep.host, verify, &[]).await {
        Ok(tls) => tls,
        Err(err) => return Err(tls_failed(err).await),
    };
    tls.get_mut().0.handshake = false;
    let (cert, tls_version) = (tls::peer_cert(&tls), Some(tls::version_label(&tls)));
    let channel = if mode == Mode::LoginOnly {
        Channel::LoginOnly(Box::new(tls))
    } else {
        Channel::Stream(Box::new(tls))
    };
    Ok(Opened {
        channel,
        version: answer.version,
        mode,
        cert,
        tls_version,
    })
}

async fn session(
    ep: &Endpoint,
    settings: Settings,
    monitor: &Monitor,
    timeout: std::time::Duration,
) -> Result<Found, Failure> {
    let opened = open(ep, settings, monitor.ignore_tls, timeout).await?;
    let tls = opened.tls_version.map(|v| {
        if opened.mode == Mode::LoginOnly {
            format!("{v} (login only)")
        } else {
            v.to_string()
        }
    });
    let Some(user) = monitor.basic_auth_user.as_deref() else {
        return Ok(Found {
            version: opened.version,
            logged_in: false,
            cert: opened.cert,
            tls,
        });
    };
    let login = login7(&Login {
        user,
        password: monitor.basic_auth_pass.as_deref().unwrap_or_default(),
        database: &ep.path,
        server: &ep.host,
    });
    let mut stream: Stream = match opened.channel {
        Channel::LoginOnly(mut tls) => {
            send(&mut tls, LOGIN7, &login).await?;
            Box::new((*tls).into_inner().0)
        }
        Channel::Stream(mut stream) => {
            send(&mut stream, LOGIN7, &login).await?;
            stream
        }
    };
    let skip_tls = opened.mode == Mode::LoginOnly;
    let reply = parse_reply(&read_message(&mut stream, REPLY, skip_tls).await?)?;
    if let Some((number, message)) = &reply.error {
        return Err(server_error(*number, message).into());
    }
    let Some(version) = reply.login_version else {
        return Err("The server did not accept the login".into());
    };
    send(&mut stream, SQL_BATCH, &select_one_batch()).await?;
    let reply = parse_reply(&read_message(&mut stream, REPLY, false).await?)?;
    if let Some((number, message)) = &reply.error {
        return Err(server_error(*number, message).into());
    }
    if reply.failed || reply.values.first() != Some(&1) {
        return Err("SELECT 1 did not return 1".into());
    }
    let _ = stream.shutdown().await;
    Ok(Found {
        version: Some(version),
        logged_in: true,
        cert: opened.cert,
        tls,
    })
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::SqlServer, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let settings = match Settings::of(&ep) {
        Ok(s) => s,
        Err(e) => return CheckOutcome::down(e),
    };
    if let Some(user) = monitor.basic_auth_user.as_deref()
        && user.contains('\\')
    {
        return CheckOutcome::down(
            "Windows authentication (DOMAIN\\user) isn't supported: use a SQL Server login",
        );
    }
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    match tokio::time::timeout(timeout, session(&ep, settings, monitor, timeout)).await {
        Ok(Ok(found)) => {
            let ms = util::elapsed_ms(start);
            let product = match &found.version {
                Some(v) => format!("SQL Server {v}"),
                None => "SQL Server".to_string(),
            };
            let message = if found.logged_in {
                format!("{product} · SELECT 1 OK")
            } else {
                format!("{product} · pre-login OK (set a user name to check the login)")
            };
            conn::up(ms, message, found.cert.as_ref(), found.tls.as_deref())
        }
        Ok(Err(Failure::Down(msg))) => CheckOutcome::down(msg),
        Ok(Err(Failure::Outcome(outcome))) => *outcome,
        Err(_) => conn::timed_out(&format!("SQL Server on {}:{}", ep.host, ep.port), timeout),
    }
}

#[cfg(test)]
mod tests;
