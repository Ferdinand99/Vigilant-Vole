//! Address rules for outbound connections.
//!
//! Two rules, for two kinds of destination:
//! - [`is_public`]: a globally routable unicast address. For destinations
//!   chosen by strangers (a subscriber's webhook), which may only reach the
//!   public internet.
//! - [`is_forbidden`]: addresses nobody legitimately sends a notification to
//!   and that lead to the host's own plumbing: link-local (where cloud
//!   metadata services live), unspecified, multicast and the well-known
//!   metadata addresses, also written as IPv4-mapped, NAT64 or 6to4 IPv6.
//!   Private and loopback addresses stay allowed, because a homelab's ntfy,
//!   Gotify or Home Assistant is on the LAN. This is the rule for what an
//!   administrator configures (notification channels).
//!
//! [`GuardedResolver`] applies [`is_forbidden`] when a connection is made,
//! so redirects and a DNS answer that changes between check and use are
//! covered. A literal IP in a URL never reaches a resolver: check those
//! with [`check_host`].

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

fn is_public_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    !(v4.is_unspecified()
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 6to4 relay anycast
        || (o[0] == 198 && (18..20).contains(&o[1])) // benchmarking
        || o[0] >= 240)
}

/// The IPv4 address a translated or tunnelled IPv6 address leads to.
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    let o = v6.octets();
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    // NAT64, well-known prefix 64:ff9b::/96.
    if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // 6to4, 2002::/16: the IPv4 address follows the prefix.
    if seg[0] == 0x2002 {
        return Some(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    None
}

/// Whether `ip` is a globally routable unicast address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = embedded_v4(v6) {
                return is_public_v4(v4);
            }
            let seg = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || seg[..6] == [0; 6] // IPv4-compatible ::a.b.c.d (deprecated)
                || (seg[0] == 0x64 && seg[1] == 0xff9b) // local-use NAT64 64:ff9b:1::/48
                || (seg[0] == 0x2001 && seg[1] == 0) // Teredo 2001::/32
                || (seg[0] & 0xfe00) == 0xfc00 // unique local
                || (seg[0] & 0xffc0) == 0xfe80 // link local
                || (seg[0] & 0xffc0) == 0xfec0 // site local (deprecated)
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)) // documentation
        }
    }
}

/// Metadata services outside the link-local range: Alibaba Cloud, Azure's
/// wire server and Oracle's legacy address (AWS, GCP, Azure IMDS, ECS and
/// DigitalOcean are in 169.254.0.0/16).
const METADATA_V4: [Ipv4Addr; 3] = [
    Ipv4Addr::new(100, 100, 100, 200),
    Ipv4Addr::new(168, 63, 129, 16),
    Ipv4Addr::new(192, 0, 0, 192),
];

/// AWS's IPv6 metadata address.
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

fn is_forbidden_v4(v4: Ipv4Addr) -> bool {
    v4.is_unspecified()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_link_local()
        || METADATA_V4.contains(&v4)
}

/// Whether a notification may never be sent to `ip`: link-local (IPv4
/// 169.254.0.0/16, IPv6 fe80::/10), the cloud metadata addresses,
/// unspecified and multicast, including those written as IPv4-mapped,
/// NAT64, 6to4 or IPv4-compatible IPv6. Private and loopback addresses are
/// allowed.
pub fn is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = embedded_v4(v6) {
                return is_forbidden_v4(v4);
            }
            let seg = v6.segments();
            if seg[..6] == [0; 6] && !v6.is_unspecified() && !v6.is_loopback() {
                let o = v6.octets();
                return is_forbidden_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
            }
            v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xffc0) == 0xfe80
                || v6 == METADATA_V6
        }
    }
}

/// Why a destination was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Forbidden;

impl std::fmt::Display for Forbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the address is not allowed (link-local, cloud metadata, unspecified or multicast)",
        )
    }
}

impl std::error::Error for Forbidden {}

/// Refuses a host that is a forbidden IP literal (`169.254.169.254`,
/// `[fe80::1]`). A name passes: it is checked when it is resolved.
pub fn check_host(host: &str) -> Result<(), Forbidden> {
    match host.trim_matches(['[', ']']).parse::<IpAddr>() {
        Ok(ip) if is_forbidden(ip) => Err(Forbidden),
        _ => Ok(()),
    }
}

/// Resolves `host` and returns the addresses that are not [`is_forbidden`],
/// or an error when the name doesn't resolve or all of its addresses are
/// forbidden. A literal IP is returned as it is, if allowed.
pub async fn resolve_allowed(
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error + Send + Sync>> {
    check_host(host)?;
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
        .await?
        .collect();
    let found = !addrs.is_empty();
    let allowed: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| !is_forbidden(a.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(if found {
            Box::new(Forbidden)
        } else {
            "the host name has no address".into()
        });
    }
    Ok(allowed)
}

/// A `reqwest` DNS resolver that never hands out a [`is_forbidden`]
/// address. `reqwest` asks it for every connection, redirects included.
#[derive(Debug, Clone, Copy, Default)]
pub struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = resolve_allowed(&host, 0).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn link_local_metadata_unspecified_and_multicast_are_forbidden() {
        for bad in [
            "169.254.169.254",
            "169.254.170.2",
            "169.254.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "239.255.255.250",
            "255.255.255.255",
            "100.100.100.200",
            "168.63.129.16",
            "192.0.0.192",
            "fe80::1",
            "febf::1",
            "::",
            "ff02::1",
            "fd00:ec2::254",
            // The same addresses written as IPv6.
            "::ffff:169.254.169.254",
            "::ffff:a9fe:a9fe",
            "::ffff:0.0.0.0",
            "::ffff:224.0.0.1",
            "::ffff:100.100.100.200",
            "64:ff9b::a9fe:a9fe",
            "2002:a9fe:a9fe::1",
            "::169.254.169.254",
        ] {
            assert!(is_forbidden(ip(bad)), "{bad} must be refused");
        }
    }

    #[test]
    fn lan_loopback_and_public_addresses_stay_allowed() {
        for good in [
            "127.0.0.1",
            "10.0.3.26",
            "172.16.0.5",
            "192.168.1.10",
            "100.64.0.1",
            "8.8.8.8",
            "93.184.216.34",
            "::1",
            "fd12:3456::1",
            "fc00::1",
            "2606:4700::1111",
            "::ffff:127.0.0.1",
            "::ffff:192.168.1.1",
            "fec0::1",
        ] {
            assert!(!is_forbidden(ip(good)), "{good} must be allowed");
        }
    }

    #[test]
    fn literal_hosts_are_checked_names_are_not() {
        assert_eq!(check_host("169.254.169.254"), Err(Forbidden));
        assert_eq!(check_host("[fe80::1]"), Err(Forbidden));
        assert_eq!(check_host("fe80::1"), Err(Forbidden));
        assert_eq!(check_host("10.0.0.1"), Ok(()));
        assert_eq!(check_host("ntfy.lan"), Ok(()));
    }

    #[tokio::test]
    async fn the_resolver_refuses_forbidden_literals_and_keeps_the_rest() {
        assert!(resolve_allowed("169.254.169.254", 80).await.is_err());
        assert!(resolve_allowed("0.0.0.0", 80).await.is_err());
        let ok = resolve_allowed("127.0.0.1", 8080).await.unwrap();
        assert_eq!(ok, ["127.0.0.1:8080".parse::<SocketAddr>().unwrap()]);
    }
}
