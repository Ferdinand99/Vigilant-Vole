//! Mumble: the UDP ping every server answers without a login. Request:
//! 4 zero bytes and an 8-byte ident; reply (24 bytes, big-endian):
//! version (0, major, minor, patch), the ident, users, max users and the
//! bandwidth per user (not shown).

use std::time::Duration;

use super::{Status, Target, Verdict, udp_exchange};

pub fn ping(ident: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[4..].copy_from_slice(&ident.to_be_bytes());
    out
}

pub(super) fn parse_pong(buf: &[u8], ident: u64) -> Verdict<Status> {
    if buf.len() < 24 {
        return Verdict::Done(Err(format!("Not a Mumble ping reply ({} bytes)", buf.len())));
    }
    if buf[4..12] != ident.to_be_bytes() {
        return Verdict::Ignore;
    }
    let word = |at: usize| u64::from(u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]));
    Verdict::Done(Ok(Status {
        product: format!("Mumble {}.{}.{}", buf[1], buf[2], buf[3]),
        players: Some((word(12), word(16))),
        name: String::new(),
        detail: None,
    }))
}

pub async fn query(target: &Target, timeout: Duration) -> Result<(Status, i64), String> {
    let ident = super::nonce();
    udp_exchange(&target.host, target.port, &ping(ident), timeout, |buf| parse_pong(buf, ident)).await
}
