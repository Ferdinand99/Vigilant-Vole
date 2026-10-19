//! DNS checker (hickory-resolver).
//!
//! - `target`: hostname (for PTR also an IP address, converted to its
//!   `in-addr.arpa` / `ip6.arpa` name).
//! - `dns_record_type`: one of [`DNS_RECORD_TYPES`].
//! - `dns_resolver`: optional `ip[:port]` (default: the system configuration,
//!   falling back to Cloudflare 1.1.1.1 when that cannot be read).
//! - `dns_expected`: optional comma-separated values; the check passes when
//!   any returned record matches any value. Names are compared without case
//!   and trailing dots, IPs semantically, MX/SRV may be matched on the host
//!   alone. For TXT the whole field is one value and matches when a record
//!   *contains* it (e.g. `v=spf1`).
//!
//! Up message lists the returned records; response time = query time.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use hickory_resolver::config::{CLOUDFLARE, NameServerConfig, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{Name, RecordType};
use hickory_resolver::{Resolver, TokioResolver};

use super::util;
use crate::db::models::{CheckOutcome, HeartbeatStatus, Monitor};

/// Record types offered in the monitor form.
pub const DNS_RECORD_TYPES: &[&str] = &[
    "A", "AAAA", "CNAME", "MX", "NS", "TXT", "SOA", "SRV", "CAA", "PTR",
];

const MESSAGE_CHARS: usize = 200;

/// Parse a custom resolver `ip`, `ip:port`, `[v6]` or `[v6]:port` (default port 53).
pub fn parse_dns_resolver(s: &str) -> Result<SocketAddr, String> {
    let (host, port) = util::split_host_port(s, Some(53))?;
    let ip: IpAddr = host
        .parse()
        .map_err(|_| format!("DNS resolver must be an IP address, got \"{host}\""))?;
    Ok(SocketAddr::new(ip, port))
}

fn record_type(s: &str) -> Option<RecordType> {
    Some(match s {
        "A" => RecordType::A,
        "AAAA" => RecordType::AAAA,
        "CNAME" => RecordType::CNAME,
        "MX" => RecordType::MX,
        "NS" => RecordType::NS,
        "TXT" => RecordType::TXT,
        "SOA" => RecordType::SOA,
        "SRV" => RecordType::SRV,
        "CAA" => RecordType::CAA,
        "PTR" => RecordType::PTR,
        _ => return None,
    })
}

fn build_resolver(custom: Option<&str>, timeout: Duration) -> Result<TokioResolver, String> {
    let mut builder = match custom.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            let addr = parse_dns_resolver(s)?;
            let mut ns = NameServerConfig::udp_and_tcp(addr.ip());
            for conn in &mut ns.connections {
                conn.port = addr.port();
            }
            Resolver::builder_with_config(
                ResolverConfig::from_name_servers(vec![ns]),
                TokioRuntimeProvider::default(),
            )
        }
        None => TokioResolver::builder_tokio().unwrap_or_else(|err| {
            tracing::debug!(%err, "system DNS config unavailable; using 1.1.1.1");
            Resolver::builder_with_config(
                ResolverConfig::udp_and_tcp(&CLOUDFLARE),
                TokioRuntimeProvider::default(),
            )
        }),
    };
    let opts = builder.options_mut();
    opts.timeout = (timeout / 2).max(Duration::from_millis(500));
    opts.attempts = 2;
    builder
        .build()
        .map_err(|e| format!("Could not set up DNS resolver: {e}"))
}

/// Normalise a DNS name/value for comparison: lowercase, single spaces,
/// no trailing dots on any token.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .map(|t| t.trim_end_matches('.').to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Does one returned record (display form) match one expected value?
pub fn record_matches(rtype: &str, record: &str, expected: &str) -> bool {
    let expected = expected.trim();
    if expected.is_empty() {
        return false;
    }
    if rtype == "TXT" {
        return record.contains(expected.trim_matches('"'));
    }
    if let (Ok(a), Ok(b)) = (record.trim().parse::<IpAddr>(), expected.parse::<IpAddr>()) {
        return a == b;
    }
    let (r, e) = (normalize(record), normalize(expected));
    if r == e {
        return true;
    }
    matches!(rtype, "MX" | "SRV") && r.rsplit(' ').next() == Some(e.as_str())
}

/// Split `dns_expected` into values (TXT: the whole field is one value).
pub fn expected_values(rtype: &str, expected: &str) -> Vec<String> {
    let expected = expected.trim();
    if expected.is_empty() {
        return Vec::new();
    }
    if rtype == "TXT" {
        return vec![expected.to_string()];
    }
    expected
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Strip trailing dots from names in a record's display form.
fn display_record(text: &str) -> String {
    text.split(' ')
        .map(|t| {
            if t.len() > 1 {
                t.trim_end_matches('.')
            } else {
                t
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let rtype_name = match monitor.dns_record_type.trim().to_ascii_uppercase() {
        s if s.is_empty() => "A".to_string(),
        s => s,
    };
    let Some(rtype) = record_type(&rtype_name) else {
        return CheckOutcome::down(format!("Unsupported DNS record type \"{rtype_name}\""));
    };
    let host = monitor.target.trim();
    if host.is_empty() {
        return CheckOutcome::down("No hostname configured");
    }
    let name = match (rtype, host.parse::<IpAddr>()) {
        (RecordType::PTR, Ok(ip)) => Name::from(ip),
        _ => {
            // Fully qualify so search domains are never appended.
            let fqdn = if host.ends_with('.') {
                host.to_string()
            } else {
                format!("{host}.")
            };
            match Name::from_utf8(&fqdn) {
                Ok(n) => n,
                Err(e) => return CheckOutcome::down(format!("Invalid hostname \"{host}\": {e}")),
            }
        }
    };
    let resolver = match build_resolver(monitor.dns_resolver.as_deref(), super::timeout_of(monitor))
    {
        Ok(r) => r,
        Err(e) => return CheckOutcome::down(e),
    };

    let start = Instant::now();
    let result = resolver.lookup(name, rtype).await;
    let ms = util::elapsed_ms(start);
    let lookup = match result {
        Ok(l) => l,
        Err(e) if e.is_nx_domain() => {
            return CheckOutcome::down(format!("NXDOMAIN: {host} does not exist"))
                .with_response_time(ms);
        }
        Err(e) if e.is_no_records_found() => {
            return CheckOutcome::down(format!("No {rtype_name} records for {host}"))
                .with_response_time(ms);
        }
        Err(e) => return CheckOutcome::down(format!("DNS query failed: {e}")),
    };

    let mut records: Vec<String> = lookup
        .answers()
        .iter()
        .filter(|r| r.record_type() == rtype)
        .map(|r| {
            let text = r.data.to_string();
            if rtype == RecordType::TXT {
                text
            } else {
                display_record(&text)
            }
        })
        .collect();
    if records.is_empty() {
        return CheckOutcome::down(format!("No {rtype_name} records for {host}"))
            .with_response_time(ms);
    }
    if matches!(rtype, RecordType::MX | RecordType::SRV) {
        // Lowest preference / priority first, like the client would use them.
        records.sort_by_key(|r| {
            r.split(' ')
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(u32::MAX)
        });
    }
    let listed = util::truncate(&records.join(", "), MESSAGE_CHARS);

    let expected = expected_values(&rtype_name, monitor.dns_expected.as_deref().unwrap_or(""));
    if !expected.is_empty()
        && !records
            .iter()
            .any(|r| expected.iter().any(|e| record_matches(&rtype_name, r, e)))
    {
        let noun = if records.len() == 1 {
            "record"
        } else {
            "records"
        };
        return CheckOutcome {
            status: HeartbeatStatus::Down,
            response_time_ms: Some(ms),
            message: Some(format!(
                "DNS {rtype_name} {noun} {listed} ≠ expected {}",
                util::truncate(&expected.join(", "), 80)
            )),
            expiry: None,
        };
    }
    CheckOutcome::up(ms).with_message(format!("{rtype_name}: {listed}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_parsing() {
        assert_eq!(
            parse_dns_resolver("1.1.1.1"),
            Ok("1.1.1.1:53".parse().unwrap())
        );
        assert_eq!(
            parse_dns_resolver("9.9.9.9:5353"),
            Ok("9.9.9.9:5353".parse().unwrap())
        );
        assert_eq!(
            parse_dns_resolver("[2606:4700::1111]"),
            Ok("[2606:4700::1111]:53".parse().unwrap())
        );
        assert_eq!(
            parse_dns_resolver("2606:4700::1111"),
            Ok("[2606:4700::1111]:53".parse().unwrap())
        );
        assert!(parse_dns_resolver("dns.google").is_err());
    }

    #[test]
    fn every_listed_type_is_supported() {
        for t in DNS_RECORD_TYPES {
            assert!(record_type(t).is_some(), "{t}");
        }
        assert!(record_type("ANY").is_none());
    }

    #[test]
    fn normalisation_ignores_case_and_dots() {
        assert!(record_matches(
            "CNAME",
            "Target.Example.COM",
            "target.example.com."
        ));
        assert!(record_matches("NS", "ns1.example.com", " NS1.example.com "));
        assert!(!record_matches("CNAME", "a.example.com", "b.example.com"));
    }

    #[test]
    fn ip_comparison_is_semantic() {
        assert!(record_matches(
            "AAAA",
            "2001:db8::1",
            "2001:0db8:0000::0001"
        ));
        assert!(!record_matches("A", "1.2.3.4", "5.6.7.8"));
    }

    #[test]
    fn mx_and_srv_match_on_host() {
        assert!(record_matches(
            "MX",
            "10 mail.example.com",
            "mail.example.com."
        ));
        assert!(record_matches(
            "MX",
            "10 mail.example.com",
            "10 mail.example.com"
        ));
        assert!(record_matches(
            "SRV",
            "0 5 5060 sip.example.com",
            "sip.example.com"
        ));
        assert!(!record_matches("MX", "10 mail.example.com", "10"));
    }

    #[test]
    fn txt_matches_by_substring_and_is_not_split() {
        assert_eq!(
            expected_values("TXT", "v=spf1 a, mx"),
            vec!["v=spf1 a, mx".to_string()]
        );
        assert_eq!(
            expected_values("A", "1.1.1.1, 1.0.0.1,"),
            vec!["1.1.1.1".to_string(), "1.0.0.1".to_string()]
        );
        assert!(record_matches(
            "TXT",
            "v=spf1 include:_spf.google.com ~all",
            "\"v=spf1\""
        ));
        assert!(!record_matches("TXT", "v=spf1", "V=SPF1"));
    }

    #[test]
    fn display_strips_trailing_dots() {
        assert_eq!(
            display_record("10 mail.example.com."),
            "10 mail.example.com"
        );
        assert_eq!(display_record("."), ".");
    }

    #[tokio::test]
    async fn unsupported_type_is_down() {
        let mut m = crate::monitor::checkers::test_support::monitor(
            crate::db::models::MonitorType::Dns,
            "example.com",
        );
        m.dns_record_type = "HINFO".into();
        assert_eq!(
            check(&m).await.message.as_deref(),
            Some("Unsupported DNS record type \"HINFO\"")
        );
    }

    #[tokio::test]
    #[ignore = "needs internet"]
    async fn live_lookup() {
        let mut m = crate::monitor::checkers::test_support::monitor(
            crate::db::models::MonitorType::Dns,
            "one.one.one.one",
        );
        m.dns_resolver = Some("1.1.1.1".into());
        m.dns_expected = Some("1.1.1.1".into());
        let out = check(&m).await;
        eprintln!("{:?}", out);
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);

        // System resolver configuration.
        m.dns_resolver = None;
        m.dns_record_type = "MX".into();
        m.target = "gmail.com".into();
        m.dns_expected = Some("gmail-smtp-in.l.google.com".into());
        let out = check(&m).await;
        eprintln!("{out:?}");
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    }
}
