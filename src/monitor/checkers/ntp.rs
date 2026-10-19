//! NTP checker: one SNTPv4 (RFC 4330) query over UDP.
//!
//! `target`: `host` or `host:port` (default 123). Up when the server answers
//! with a valid, synchronised reply (stratum 1–15, leap indicator ≠ 3).
//! Message: "offset +3.2 ms, stratum 2"; response time = round-trip time.
//! Down on kiss-o'-death (stratum 0, e.g. RATE/DENY), unsynchronised
//! servers, or no response.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::net::UdpSocket;

use super::util;
use crate::db::models::{CheckOutcome, Monitor};

/// Seconds between 1900-01-01 (NTP era 0) and 1970-01-01.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
const PACKET_LEN: usize = 48;

/// Current time as a 64-bit NTP timestamp (32.32 fixed point since 1900).
fn ntp_now() -> u64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs().wrapping_add(NTP_UNIX_OFFSET);
    let frac = (u64::from(d.subsec_nanos()) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// A client request: LI=0, VN=4, Mode=3, with our transmit timestamp.
pub fn encode_request(transmit: u64) -> [u8; PACKET_LEN] {
    let mut buf = [0u8; PACKET_LEN];
    buf[0] = 0b00_100_011;
    buf[40..48].copy_from_slice(&transmit.to_be_bytes());
    buf
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtpReply {
    pub leap: u8,
    pub version: u8,
    pub mode: u8,
    pub stratum: u8,
    pub ref_id: [u8; 4],
    pub originate: u64,
    pub receive: u64,
    pub transmit: u64,
}

fn be_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_be_bytes(a)
}

pub fn decode_reply(buf: &[u8]) -> Result<NtpReply, String> {
    if buf.len() < PACKET_LEN {
        return Err(format!("Short NTP reply ({} bytes)", buf.len()));
    }
    Ok(NtpReply {
        leap: buf[0] >> 6,
        version: (buf[0] >> 3) & 0b111,
        mode: buf[0] & 0b111,
        stratum: buf[1],
        ref_id: [buf[12], buf[13], buf[14], buf[15]],
        originate: be_u64(&buf[24..32]),
        receive: be_u64(&buf[32..40]),
        transmit: be_u64(&buf[40..48]),
    })
}

/// Clock offset and round-trip delay in milliseconds from the four
/// timestamps (t1 client send, t2 server receive, t3 server send, t4 client
/// receive), as 32.32 fixed-point NTP values.
pub fn offset_and_delay(t1: u64, t2: u64, t3: u64, t4: u64) -> (f64, f64) {
    let (t1, t2, t3, t4) = (
        i128::from(t1),
        i128::from(t2),
        i128::from(t3),
        i128::from(t4),
    );
    let offset = ((t2 - t1) + (t3 - t4)) / 2;
    let delay = (t4 - t1) - (t3 - t2);
    let to_ms = |v: i128| v as f64 * 1000.0 / 4_294_967_296.0;
    (to_ms(offset), to_ms(delay))
}

/// Validate a reply against our request. `Err` = Down message.
pub fn validate(reply: &NtpReply, sent_transmit: u64) -> Result<(), String> {
    if reply.mode != 4 && reply.mode != 5 {
        return Err(format!("Not an NTP server reply (mode {})", reply.mode));
    }
    // Checked first: an off-path spoofer can't know our transmit timestamp,
    // so a kiss-o'-death that doesn't echo it is not from the server.
    if reply.originate != sent_transmit {
        return Err("Reply does not match our request (originate timestamp mismatch)".into());
    }
    if reply.stratum == 0 {
        let code: String = reply
            .ref_id
            .iter()
            .filter(|b| b.is_ascii_graphic())
            .map(|&b| char::from(b))
            .collect();
        return Err(format!(
            "Kiss-o'-death from server: {}",
            if code.is_empty() { "?" } else { &code }
        ));
    }
    if reply.stratum >= 16 {
        return Err(format!(
            "Server is unsynchronised (stratum {})",
            reply.stratum
        ));
    }
    if reply.leap == 3 {
        return Err("Server clock is not synchronised (leap indicator 3)".into());
    }
    if reply.transmit == 0 {
        return Err("Reply has no transmit timestamp".into());
    }
    Ok(())
}

/// "offset +3.2 ms, stratum 2".
pub fn describe(offset_ms: f64, stratum: u8) -> String {
    format!("offset {offset_ms:+.1} ms, stratum {stratum}")
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let (host, port) = match util::split_host_port(&monitor.target, Some(123)) {
        Ok(hp) => hp,
        Err(e) => return CheckOutcome::down(e),
    };
    let addr = match util::resolve(&host, port).await {
        Ok(a) => a,
        Err(e) => return CheckOutcome::down(e),
    };
    let bind = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = match UdpSocket::bind(bind).await {
        Ok(s) => s,
        Err(e) => {
            return CheckOutcome::down(format!(
                "Could not open UDP socket: {}",
                util::tidy_io_error(&e)
            ));
        }
    };
    if let Err(e) = socket.connect(addr).await {
        return CheckOutcome::down(format!(
            "Could not reach {addr}: {}",
            util::tidy_io_error(&e)
        ));
    }

    let t1 = ntp_now();
    let start = Instant::now();
    if let Err(e) = socket.send(&encode_request(t1)).await {
        return CheckOutcome::down(format!(
            "Could not send NTP request: {}",
            util::tidy_io_error(&e)
        ));
    }
    let mut buf = [0u8; 512];
    loop {
        let n = match socket.recv(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                return CheckOutcome::down(format!("No NTP reply: {}", util::udp_recv_error(&e)));
            }
        };
        let t4 = ntp_now();
        let rtt = util::elapsed_ms(start);
        let reply = match decode_reply(&buf[..n]) {
            Ok(r) => r,
            Err(e) => return CheckOutcome::down(e),
        };
        if reply.originate != t1 {
            // Stray/late/spoofed datagram (kiss-o'-death included); keep
            // waiting (the overall timeout applies).
            continue;
        }
        if let Err(e) = validate(&reply, t1) {
            return CheckOutcome::down(e).with_response_time(rtt);
        }
        let (offset, _delay) = offset_and_delay(t1, reply.receive, reply.transmit, t4);
        return CheckOutcome::up(rtt).with_message(describe(offset, reply.stratum));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured-style server reply (stratum 2, mode 4, VN 4, refid 192.0.2.1).
    fn sample_reply(originate: u64) -> [u8; 48] {
        let mut b = [0u8; 48];
        b[0] = 0x24; // LI 0, VN 4, mode 4
        b[1] = 2; // stratum
        b[2] = 3; // poll
        b[3] = 0xE9; // precision
        b[12..16].copy_from_slice(&[192, 0, 2, 1]);
        b[24..32].copy_from_slice(&originate.to_be_bytes());
        b[32..40].copy_from_slice(&0xE8A1_2B3C_8000_0000u64.to_be_bytes());
        b[40..48].copy_from_slice(&0xE8A1_2B3C_8010_0000u64.to_be_bytes());
        b
    }

    #[test]
    fn request_encoding() {
        let req = encode_request(0x0102_0304_0506_0708);
        assert_eq!(req[0], 0x23);
        assert_eq!(&req[40..48], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(req[1..40].iter().all(|&b| b == 0));
    }

    #[test]
    fn reply_decoding() {
        let r = decode_reply(&sample_reply(42)).unwrap();
        assert_eq!((r.leap, r.version, r.mode, r.stratum), (0, 4, 4, 2));
        assert_eq!(r.ref_id, [192, 0, 2, 1]);
        assert_eq!(r.originate, 42);
        assert_eq!(r.receive, 0xE8A1_2B3C_8000_0000);
        assert!(validate(&r, 42).is_ok());
        assert!(decode_reply(&[0u8; 47]).is_err());
    }

    #[test]
    fn offset_math() {
        let s = 1u64 << 32; // one second
        // Server 500 ms ahead, 100 ms each way, 0 processing time.
        let t1 = 1000 * s;
        let t2 = t1 + s / 10 + s / 2;
        let t3 = t2;
        let t4 = t1 + s / 5;
        let (offset, delay) = offset_and_delay(t1, t2, t3, t4);
        assert!((offset - 500.0).abs() < 0.01, "{offset}");
        assert!((delay - 200.0).abs() < 0.01, "{delay}");
        assert_eq!(describe(3.24, 2), "offset +3.2 ms, stratum 2");
        assert_eq!(describe(-0.5, 1), "offset -0.5 ms, stratum 1");
    }

    #[test]
    fn kiss_of_death_and_unsynchronised() {
        let mut b = sample_reply(7);
        b[1] = 0;
        b[12..16].copy_from_slice(b"RATE");
        assert_eq!(
            validate(&decode_reply(&b).unwrap(), 7),
            Err("Kiss-o'-death from server: RATE".into())
        );
        let mut b = sample_reply(7);
        b[1] = 16;
        assert!(
            validate(&decode_reply(&b).unwrap(), 7)
                .unwrap_err()
                .contains("stratum 16")
        );
        let mut b = sample_reply(7);
        b[0] = 0xE4; // LI 3
        assert!(validate(&decode_reply(&b).unwrap(), 7).is_err());
        assert!(validate(&decode_reply(&sample_reply(8)).unwrap(), 7).is_err());
    }

    #[test]
    fn kiss_of_death_must_echo_our_timestamp() {
        let mut b = sample_reply(99);
        b[1] = 0;
        b[12..16].copy_from_slice(b"DENY");
        let err = validate(&decode_reply(&b).unwrap(), 7).unwrap_err();
        assert!(err.contains("originate timestamp mismatch"), "{err}");
    }

    #[test]
    fn now_is_after_2020() {
        // 2020-01-01 in NTP seconds.
        assert!(ntp_now() >> 32 > 3_786_825_600);
    }

    #[tokio::test]
    async fn local_server_round_trip() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 48];
            if let Ok((_, peer)) = server.recv_from(&mut buf).await {
                let t1 = be_u64(&buf[40..48]);
                let mut reply = sample_reply(t1);
                let now = ntp_now();
                reply[32..40].copy_from_slice(&now.to_be_bytes());
                reply[40..48].copy_from_slice(&now.to_be_bytes());
                let _ = server.send_to(&reply, peer).await;
            }
        });
        let m = crate::monitor::checkers::test_support::monitor(
            crate::db::models::MonitorType::Ntp,
            &addr.to_string(),
        );
        let out = check(&m).await;
        assert_eq!(
            out.status,
            crate::db::models::HeartbeatStatus::Up,
            "{:?}",
            out.message
        );
        assert!(out.message.unwrap().ends_with("stratum 2"));
    }

    #[tokio::test]
    #[ignore = "needs internet"]
    async fn live_ntp() {
        let m = crate::monitor::checkers::test_support::monitor(
            crate::db::models::MonitorType::Ntp,
            "pool.ntp.org",
        );
        let out = check(&m).await;
        eprintln!("{:?}", out);
        assert_eq!(
            out.status,
            crate::db::models::HeartbeatStatus::Up,
            "{:?}",
            out.message
        );
    }
}
