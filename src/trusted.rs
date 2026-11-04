//! Address allowlist for incoming connection requests.
//!
//! A device that connects from one of these addresses is trusted without the
//! interactive authorization dialog: its fingerprint is stored on first use,
//! so later connections are matched by certificate like any other device.
//!
//! Entries are plain IPs (`192.168.1.50`) or CIDR ranges (`10.0.0.0/24`).

use std::net::IpAddr;

/// One entry of the allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedNet {
    addr: IpAddr,
    /// network prefix length (32 for a single address)
    prefix: u8,
}

impl TrustedNet {
    /// Parse `192.168.1.5`, `192.168.1.5/24` or `fd00::1/64`.
    pub fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            return None;
        }
        let (addr, prefix) = match entry.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix.parse::<u8>().ok()?)),
            None => (entry, None),
        };
        let addr: IpAddr = addr.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        if prefix > max {
            return None;
        }
        Some(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix as u32)
                };
                (u32::from(net) & mask) == (u32::from(ip) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix as u32)
                };
                (u128::from(net) & mask) == (u128::from(ip) & mask)
            }
            _ => false,
        }
    }
}

/// Parse every valid entry, ignoring the malformed ones.
pub fn parse_all(entries: &[String]) -> Vec<TrustedNet> {
    entries
        .iter()
        .filter_map(|e| TrustedNet::parse(e))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn plain_address_matches_only_itself() {
        let net = TrustedNet::parse("192.168.1.50").expect("parse");
        assert!(net.contains(ip("192.168.1.50")));
        assert!(!net.contains(ip("192.168.1.51")));
    }

    #[test]
    fn cidr_covers_the_range() {
        let net = TrustedNet::parse("192.168.1.0/24").expect("parse");
        assert!(net.contains(ip("192.168.1.1")));
        assert!(net.contains(ip("192.168.1.254")));
        assert!(!net.contains(ip("192.168.2.1")));
    }

    #[test]
    fn ipv6_is_supported() {
        let net = TrustedNet::parse("fd00::/64").expect("parse");
        assert!(net.contains(ip("fd00::1")));
        assert!(!net.contains(ip("fd01::1")));
    }

    #[test]
    fn families_never_match_each_other() {
        let net = TrustedNet::parse("0.0.0.0/0").expect("parse");
        assert!(!net.contains(ip("::1")));
    }

    #[test]
    fn malformed_entries_are_rejected() {
        assert!(TrustedNet::parse("").is_none());
        assert!(TrustedNet::parse("not-an-ip").is_none());
        assert!(TrustedNet::parse("192.168.1.1/33").is_none());
        assert!(TrustedNet::parse("192.168.1.1/x").is_none());
    }
}
