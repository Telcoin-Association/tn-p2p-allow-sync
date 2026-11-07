//! Addresses: the leading IP segment of a multiaddr, the canonical form of an IP, CIDR entries and their order.
//!
//! Everything here is hand-parsed with std.
//! The `multiaddr` crate would pull in multihash, multibase, unsigned-varint, url and libp2p-identity to read one segment.
//!
//! Order is numeric and deterministic: IPv4 sorts before IPv6, and within a family by value, never by text.
//! IPv6 prints through std's `Display`, which is the RFC 5952 compressed form.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// How many entries of one multiaddr list are read; the rest of the list is ignored.
///
/// The limit applies to `multiaddrs` and `record_multiaddrs` of a snapshot entry separately.
pub const MAX_MULTIADDRS_PER_LIST: usize = 32;

/// The prefix length of the IPv4-mapped range `::ffff:0:0/96`: the IPv6 bits in front of the IPv4 address.
const MAPPED_PREFIX: u8 = 96;

/// What the leading segment of a multiaddr says about the peer's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrClass {
    /// The multiaddr starts with `/ip4/A` or `/ip6/A` and `A` parses as an address of that family.
    ///
    /// The address is as written; [`canonical_ip`] maps and filters it.
    Ip(IpAddr),
    /// The multiaddr starts with `/dns/`, `/dns4/`, `/dns6/` or `/dnsaddr/` and a name.
    ///
    /// Names are never resolved: the tool makes no DNS queries.
    Dns,
    /// Anything else: another protocol first, a missing or malformed address, or text that is not a multiaddr.
    Unparsed,
}

/// Classifies the multiaddr `s` by its leading segment; everything after the first address is ignored.
///
/// The address ends at the next `/` or at the end of the string, so `/ip4/192.0.2.10` and `/ip4/192.0.2.10/udp/49590/quic-v1` give the same address.
/// Protocol names match case-sensitively, as the multiaddr spec writes them, and nothing is trimmed.
pub fn classify_multiaddr(s: &str) -> AddrClass {
    let Some(rest) = s.strip_prefix('/') else {
        return AddrClass::Unparsed;
    };
    let mut segments = rest.split('/');
    let (Some(protocol), Some(value)) = (segments.next(), segments.next()) else {
        return AddrClass::Unparsed;
    };
    match protocol {
        "ip4" => value
            .parse::<Ipv4Addr>()
            .map_or(AddrClass::Unparsed, |ip| AddrClass::Ip(IpAddr::V4(ip))),
        "ip6" => value
            .parse::<Ipv6Addr>()
            .map_or(AddrClass::Unparsed, |ip| AddrClass::Ip(IpAddr::V6(ip))),
        "dns" | "dns4" | "dns6" | "dnsaddr" if !value.is_empty() => AddrClass::Dns,
        _ => AddrClass::Unparsed,
    }
}

/// The form of `ip` that goes into an allow set, or `None` when the address is never a peer's source.
///
/// An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) becomes the IPv4 address, because the kernel matches its packets as IPv4.
/// The deprecated IPv4-compatible form (`::a.b.c.d`) stays IPv6: converting it would turn `::1` into `0.0.0.1`.
/// Unspecified, multicast and link-local addresses (169.254.0.0/16 and fe80::/10) are dropped, after the mapping.
/// Loopback and private ranges are kept, because a devnet runs on them.
pub fn canonical_ip(ip: IpAddr) -> Option<IpAddr> {
    // `to_ipv4` would also convert the compatible form, so only the mapped form is converted
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    };
    let dropped = match ip {
        IpAddr::V4(v4) => v4.is_unspecified() || v4.is_multicast() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast() || v6.is_unicast_link_local(),
    };
    (!dropped).then_some(ip)
}

/// Splits addresses by family into the sets the renderer takes.
///
/// The sets order numerically and drop duplicates.
/// Addresses are taken as given; pass them through [`canonical_ip`] first.
pub fn split_ips<I>(ips: I) -> (BTreeSet<Ipv4Addr>, BTreeSet<Ipv6Addr>)
where
    I: IntoIterator<Item = IpAddr>,
{
    let mut v4 = BTreeSet::new();
    let mut v6 = BTreeSet::new();
    for ip in ips {
        match ip {
            IpAddr::V4(ip) => {
                v4.insert(ip);
            }
            IpAddr::V6(ip) => {
                v6.insert(ip);
            }
        }
    }
    (v4, v6)
}

/// An IP network: an address masked to its first `prefix` bits.
///
/// [`parse_cidr_or_host`] and [`Cidr::new`] always mask, so two spellings of one network compare equal.
/// A value built from the public fields is trusted as written.
///
/// `Display` prints `A/N`, a host included (`198.51.100.9/32`).
/// The order is by family (IPv4 first), then network, then prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cidr {
    /// The network address: every bit past `prefix` is zero.
    pub net: IpAddr,
    /// The prefix length: 0 to 32 for IPv4, 0 to 128 for IPv6.
    pub prefix: u8,
}

impl Cidr {
    /// The network of `ip` with `prefix` bits, or `None` when `prefix` is longer than the family's address.
    pub fn new(ip: IpAddr, prefix: u8) -> Option<Self> {
        let net = match ip {
            IpAddr::V4(v4) => {
                let host_bits = 32u32.checked_sub(u32::from(prefix))?;
                // a shift by the full width is a /0, whose mask is empty
                let mask = u32::MAX.checked_shl(host_bits).unwrap_or(0);
                IpAddr::V4(Ipv4Addr::from(u32::from(v4) & mask))
            }
            IpAddr::V6(v6) => {
                let host_bits = 128u32.checked_sub(u32::from(prefix))?;
                let mask = u128::MAX.checked_shl(host_bits).unwrap_or(0);
                IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask))
            }
        };
        Some(Self { net, prefix })
    }

    /// Whether this network is one address: a prefix of 32 for IPv4 or 128 for IPv6.
    pub fn is_host(&self) -> bool {
        self.prefix == full_prefix(self.net)
    }

    /// The network as the kernel matches it: a network inside the IPv4-mapped range `::ffff:0:0/96` becomes the IPv4 network of the same addresses, as [`canonical_ip`] maps a host.
    ///
    /// `::ffff:a.b.c.d/N` gives `a.b.c.d/(N-96)`, masked as usual: `::ffff:10.0.0.0/104` is `10.0.0.0/8` and `::ffff:0.0.0.0/96` is `0.0.0.0/0`.
    /// The kernel sees the source of an IPv4 packet as IPv4, never as `::ffff:a.b.c.d`, so the mapped form would match nothing.
    /// Every other network is returned as it is; [`parse_cidr_or_host`] refuses a mapped address with a prefix shorter than 96.
    pub fn unmapped(self) -> Self {
        let mapped = match self.net {
            IpAddr::V6(v6) => v6
                .to_ipv4_mapped()
                .zip(self.prefix.checked_sub(MAPPED_PREFIX)),
            IpAddr::V4(_) => None,
        };
        mapped
            .and_then(|(v4, prefix)| Self::new(IpAddr::V4(v4), prefix))
            .unwrap_or(self)
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.net, self.prefix)
    }
}

impl Ord for Cidr {
    fn cmp(&self, other: &Self) -> Ordering {
        // `IpAddr` orders V4 before V6 and numerically within a family
        (self.net, self.prefix).cmp(&(other.net, other.prefix))
    }
}

impl PartialOrd for Cidr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Why an entry of `P2P_ALLOW_FROM` is not an address or a network.
///
/// The caller refuses the whole run: dropping the entry would cut the static list, and resolving a name would be DNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddrError {
    /// The text before any `/` is not an IPv4 or IPv6 literal; a hostname lands here.
    NotAnIp {
        /// The entry as given.
        entry: String,
    },
    /// The text after `/` is not a decimal prefix length: empty, signed, with a leading zero or with other characters.
    BadPrefix {
        /// The entry as given.
        entry: String,
    },
    /// The prefix length is longer than the family's address.
    PrefixTooLong {
        /// The entry as given.
        entry: String,
        /// The longest prefix of the family: 32 or 128.
        max: u8,
    },
    /// The address is IPv4-mapped (`::ffff:a.b.c.d`) and the prefix length is shorter than 96.
    ///
    /// Such a network reaches past the mapped range, so it has no IPv4 form, and as IPv6 it would match none of the IPv4 sources it names.
    MappedPrefixTooShort {
        /// The entry as given.
        entry: String,
    },
}

impl fmt::Display for AddrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // the entry prints through `Debug`, so a control character cannot break the log line
        match self {
            Self::NotAnIp { entry } => write!(
                f,
                "{entry:?} is not an IP address or a CIDR (hostnames are not resolved)"
            ),
            Self::BadPrefix { entry } => write!(
                f,
                "{entry:?} has a prefix length that is not a plain decimal number"
            ),
            Self::PrefixTooLong { entry, max } => {
                write!(f, "{entry:?} has a prefix length longer than {max}")
            }
            Self::MappedPrefixTooShort { entry } => write!(
                f,
                "{entry:?} is an IPv4-mapped address with a prefix length shorter than {MAPPED_PREFIX}"
            ),
        }
    }
}

impl std::error::Error for AddrError {}

/// Parses one `P2P_ALLOW_FROM` entry: an address (`198.51.100.9`, a host) or a network (`203.0.113.0/24`).
///
/// A host gets the full prefix of its family, 32 or 128.
/// A network is masked to its network address, so `203.0.113.77/24` gives `203.0.113.0/24`.
/// The address is a std IP literal: no hostname, zone, brackets, whitespace or shortened IPv4 form.
/// The prefix is decimal digits without a sign or a leading zero.
/// An IPv4-mapped address (`::ffff:a.b.c.d`) needs a prefix of at least 96; the network keeps its mapped form, and [`Cidr::unmapped`] gives the IPv4 one.
pub fn parse_cidr_or_host(s: &str) -> Result<Cidr, AddrError> {
    let (addr, prefix) = match s.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix)),
        None => (s, None),
    };
    let ip = addr.parse::<IpAddr>().map_err(|_| AddrError::NotAnIp {
        entry: s.to_owned(),
    })?;
    let max = full_prefix(ip);
    let prefix = match prefix {
        None => max,
        Some(text) => parse_prefix(text).ok_or_else(|| AddrError::BadPrefix {
            entry: s.to_owned(),
        })?,
    };
    // the check needs the address as written: masking to a prefix below 96 clears the `ffff`
    let mapped = matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some());
    if mapped && prefix < MAPPED_PREFIX {
        return Err(AddrError::MappedPrefixTooShort {
            entry: s.to_owned(),
        });
    }
    Cidr::new(ip, prefix).ok_or_else(|| AddrError::PrefixTooLong {
        entry: s.to_owned(),
        max,
    })
}

/// The prefix length of a host: 32 for IPv4, 128 for IPv6.
const fn full_prefix(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    }
}

/// Reads a prefix length: one or more ASCII digits, no leading zero unless the length is 0.
///
/// A number too large for `u8` reads as 255, which is longer than any family allows and fails the range check.
fn parse_prefix(text: &str) -> Option<u8> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let leading_zero = text.len() > 1 && text.starts_with('0');
    (digits && !leading_zero).then(|| text.parse::<u8>().unwrap_or(u8::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    type TestResult = Result<(), Box<dyn Error>>;

    const fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn v6(s: &str) -> Result<IpAddr, Box<dyn Error>> {
        Ok(IpAddr::V6(s.parse::<Ipv6Addr>()?))
    }

    fn cidr(s: &str) -> Result<Cidr, Box<dyn Error>> {
        Ok(parse_cidr_or_host(s)?)
    }

    #[test]
    fn classify_reads_the_leading_ip4_and_ip6_segment() -> TestResult {
        assert_eq!(
            classify_multiaddr("/ip4/192.0.2.10/udp/49590/quic-v1"),
            AddrClass::Ip(v4(192, 0, 2, 10))
        );
        assert_eq!(
            classify_multiaddr("/ip6/2001:db8::7/udp/49590/quic-v1"),
            AddrClass::Ip(v6("2001:db8::7")?)
        );
        assert_eq!(
            classify_multiaddr("/ip6/::ffff:192.0.2.10/udp/49590/quic-v1"),
            AddrClass::Ip(v6("::ffff:192.0.2.10")?)
        );
        Ok(())
    }

    #[test]
    fn classify_ignores_everything_after_the_address() -> TestResult {
        for s in [
            "/ip4/192.0.2.10",
            "/ip4/192.0.2.10/",
            "/ip4/192.0.2.10/udp/49590/quic-v1/p2p/12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN",
            "/ip4/192.0.2.10/tcp/not-a-port//junk",
        ] {
            assert_eq!(
                classify_multiaddr(s),
                AddrClass::Ip(v4(192, 0, 2, 10)),
                "{s}"
            );
        }
        assert_eq!(classify_multiaddr("/ip6/::1"), AddrClass::Ip(v6("::1")?));
        Ok(())
    }

    #[test]
    fn classify_dns_variants() {
        for s in [
            "/dns/node.example/udp/49590/quic-v1",
            "/dns4/node.example/udp/49590/quic-v1",
            "/dns6/node.example/udp/49590/quic-v1",
            "/dnsaddr/node.example",
            "/dns4/192.0.2.10/udp/49590",
        ] {
            assert_eq!(classify_multiaddr(s), AddrClass::Dns, "{s}");
        }
    }

    #[test]
    fn classify_garbage_is_unparsed() {
        for s in [
            "",
            "/",
            "//",
            "ip4/192.0.2.10/udp/49590",
            " /ip4/192.0.2.10/udp/49590",
            "/ip4",
            "/ip4/",
            "/ip4//udp/49590",
            "/ip4/192.0.2.10 /udp/49590",
            "/ip4/192.0.2.256/udp/49590",
            "/ip4/192.0.2/udp/49590",
            "/ip4/192.0.02.10/udp/49590",
            "/ip4/2001:db8::7/udp/49590",
            "/ip6/192.0.2.10/udp/49590",
            "/ip6/[2001:db8::7]/udp/49590",
            "/ip6/fe80::1%eth0/udp/49590",
            "/ip6zone/eth0/ip6/fe80::1/udp/49590",
            "/IP4/192.0.2.10/udp/49590",
            "/Dns4/node.example/udp/49590",
            "/dns4",
            "/dns4//udp/49590",
            "/udp/49590/ip4/192.0.2.10",
            "/p2p/12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN",
            "/unix/tmp/node.sock",
            "192.0.2.10",
        ] {
            assert_eq!(classify_multiaddr(s), AddrClass::Unparsed, "{s:?}");
        }
    }

    #[test]
    fn canonical_keeps_unicast_loopback_and_private() -> TestResult {
        for ip in [
            v4(192, 0, 2, 10),
            v4(198, 51, 100, 7),
            v4(10, 0, 0, 1),
            v4(172, 16, 0, 1),
            v4(192, 168, 1, 1),
            v4(100, 64, 0, 1),
            v4(127, 0, 0, 1),
            v4(169, 253, 255, 255),
            v4(169, 255, 0, 0),
            v4(223, 255, 255, 255),
            v4(240, 0, 0, 1),
            v6("::1")?,
            v6("2001:db8::7")?,
            v6("fd00::1")?,
            v6("fe7f:ffff::1")?,
            v6("fec0::1")?,
            v6("feff::1")?,
        ] {
            assert_eq!(canonical_ip(ip), Some(ip), "{ip}");
        }
        Ok(())
    }

    #[test]
    fn canonical_drops_unspecified() -> TestResult {
        for ip in [v4(0, 0, 0, 0), v6("::")?, v6("::ffff:0.0.0.0")?] {
            assert_eq!(canonical_ip(ip), None, "{ip}");
        }
        Ok(())
    }

    #[test]
    fn canonical_drops_multicast() -> TestResult {
        for ip in [
            v4(224, 0, 0, 1),
            v4(239, 255, 255, 255),
            v6("ff02::1")?,
            v6("ff0e::1")?,
            v6("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff")?,
            v6("::ffff:224.0.0.251")?,
        ] {
            assert_eq!(canonical_ip(ip), None, "{ip}");
        }
        Ok(())
    }

    #[test]
    fn canonical_drops_link_local_v4_and_v6() -> TestResult {
        for ip in [
            v4(169, 254, 0, 0),
            v4(169, 254, 0, 1),
            v4(169, 254, 255, 255),
            v6("fe80::")?,
            v6("fe80::1")?,
            v6("febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff")?,
            v6("::ffff:169.254.1.1")?,
        ] {
            assert_eq!(canonical_ip(ip), None, "{ip}");
        }
        Ok(())
    }

    #[test]
    fn canonical_maps_ipv4_mapped_to_ipv4() -> TestResult {
        assert_eq!(
            canonical_ip(v6("::ffff:192.0.2.10")?),
            Some(v4(192, 0, 2, 10))
        );
        assert_eq!(
            canonical_ip(v6("::ffff:c000:20a")?),
            Some(v4(192, 0, 2, 10))
        );
        assert_eq!(
            canonical_ip(v6("::ffff:127.0.0.1")?),
            Some(v4(127, 0, 0, 1))
        );
        Ok(())
    }

    #[test]
    fn canonical_keeps_the_compatible_form_as_ipv6() -> TestResult {
        // `to_ipv4` would give 0.0.0.1, 0.0.0.2 and 192.0.2.10 here
        for ip in [v6("::1")?, v6("::2")?, v6("::192.0.2.10")?] {
            assert_eq!(canonical_ip(ip), Some(ip), "{ip}");
        }
        Ok(())
    }

    #[test]
    fn classify_then_canonical_on_a_mapped_multiaddr() -> TestResult {
        let AddrClass::Ip(ip) = classify_multiaddr("/ip6/::ffff:192.0.2.10/udp/49590/quic-v1")
        else {
            return Err("not classified as an IP".into());
        };
        assert_eq!(canonical_ip(ip), Some(v4(192, 0, 2, 10)));
        Ok(())
    }

    #[test]
    fn a_bare_ip_is_a_host() -> TestResult {
        let host = cidr("198.51.100.9")?;
        assert_eq!(
            host,
            Cidr {
                net: v4(198, 51, 100, 9),
                prefix: 32
            }
        );
        assert!(host.is_host());
        assert_eq!(host.to_string(), "198.51.100.9/32");

        let host = cidr("2001:db8::7")?;
        assert_eq!(
            host,
            Cidr {
                net: v6("2001:db8::7")?,
                prefix: 128
            }
        );
        assert!(host.is_host());
        assert_eq!(host.to_string(), "2001:db8::7/128");

        assert_eq!(cidr("198.51.100.9/32")?, cidr("198.51.100.9")?);
        assert!(!cidr("203.0.113.0/24")?.is_host());
        assert!(!cidr("2001:db8::/127")?.is_host());
        Ok(())
    }

    #[test]
    fn cidr_masks_to_the_network() -> TestResult {
        for (entry, expected) in [
            ("203.0.113.77/24", "203.0.113.0/24"),
            ("203.0.113.0/24", "203.0.113.0/24"),
            ("10.1.2.3/8", "10.0.0.0/8"),
            ("192.0.2.1/31", "192.0.2.0/31"),
            ("192.0.2.255/25", "192.0.2.128/25"),
            ("198.51.100.9/32", "198.51.100.9/32"),
            ("198.51.100.9/0", "0.0.0.0/0"),
            ("2001:db8::1/64", "2001:db8::/64"),
            ("2001:db8:ffff::1/33", "2001:db8:8000::/33"),
            ("2001:db8::1/128", "2001:db8::1/128"),
            ("::1/0", "::/0"),
        ] {
            assert_eq!(cidr(entry)?.to_string(), expected, "{entry}");
        }
        assert_eq!(cidr("203.0.113.77/24")?, cidr("203.0.113.0/24")?);
        Ok(())
    }

    #[test]
    fn cidr_new_masks_and_checks_the_prefix() -> TestResult {
        assert_eq!(
            Cidr::new(v4(203, 0, 113, 77), 24),
            Some(Cidr {
                net: v4(203, 0, 113, 0),
                prefix: 24
            })
        );
        assert_eq!(
            Cidr::new(v4(203, 0, 113, 77), 0),
            Some(Cidr {
                net: v4(0, 0, 0, 0),
                prefix: 0
            })
        );
        assert_eq!(Cidr::new(v4(203, 0, 113, 77), 33), None);
        assert_eq!(
            Cidr::new(v6("2001:db8::1")?, 128),
            Some(Cidr {
                net: v6("2001:db8::1")?,
                prefix: 128
            })
        );
        assert_eq!(Cidr::new(v6("2001:db8::1")?, 129), None);
        assert_eq!(Cidr::new(v6("2001:db8::1")?, u8::MAX), None);
        Ok(())
    }

    #[test]
    fn cidr_prefix_out_of_range() {
        for (entry, max) in [
            ("203.0.113.0/33", 32),
            ("203.0.113.0/128", 32),
            ("203.0.113.0/255", 32),
            ("203.0.113.0/256", 32),
            ("203.0.113.0/99999999999999999999", 32),
            ("2001:db8::/129", 128),
            ("2001:db8::/200", 128),
            ("2001:db8::/256", 128),
            ("::ffff:10.0.0.0/129", 128),
        ] {
            assert_eq!(
                parse_cidr_or_host(entry),
                Err(AddrError::PrefixTooLong {
                    entry: entry.to_owned(),
                    max
                }),
                "{entry}"
            );
        }
    }

    #[test]
    fn a_mapped_address_needs_a_prefix_of_at_least_96() -> TestResult {
        for entry in [
            "::ffff:10.0.0.0/95",
            "::ffff:10.0.0.0/80",
            "::ffff:a00:0/64",
            "::ffff:0.0.0.0/0",
        ] {
            assert_eq!(
                parse_cidr_or_host(entry),
                Err(AddrError::MappedPrefixTooShort {
                    entry: entry.to_owned()
                }),
                "{entry}"
            );
        }
        // the rule reads the address as written: the compatible form and the networks around the mapped range are plain IPv6
        for (entry, expected) in [
            ("::ffff:10.0.0.0/96", "::ffff:0.0.0.0/96"),
            ("::192.0.2.0/120", "::c000:200/120"),
            ("::fffe:0:0/95", "::fffe:0:0/95"),
            ("::/80", "::/80"),
        ] {
            assert_eq!(cidr(entry)?.to_string(), expected, "{entry}");
        }
        Ok(())
    }

    #[test]
    fn unmapped_gives_the_ipv4_network_of_a_mapped_one() -> TestResult {
        for (entry, expected) in [
            ("::ffff:10.0.0.0/104", "10.0.0.0/8"),
            ("::ffff:10.1.2.3/104", "10.0.0.0/8"),
            ("::ffff:10.0.0.0/96", "0.0.0.0/0"),
            ("::ffff:0.0.0.0/96", "0.0.0.0/0"),
            ("::ffff:192.0.2.77/120", "192.0.2.0/24"),
            ("::ffff:192.0.2.10/128", "192.0.2.10/32"),
            ("::ffff:192.0.2.10", "192.0.2.10/32"),
            // every other network stays as it is
            ("192.0.2.0/24", "192.0.2.0/24"),
            ("2001:db8::/32", "2001:db8::/32"),
            ("::/0", "::/0"),
            ("::/80", "::/80"),
            ("::192.0.2.0/120", "::c000:200/120"),
            ("::fffe:0:0/95", "::fffe:0:0/95"),
        ] {
            assert_eq!(cidr(entry)?.unmapped().to_string(), expected, "{entry}");
        }
        // a value built from the fields is trusted as written; one without an IPv4 form stays as it is
        for prefix in [90, 200] {
            let odd = Cidr {
                net: v6("::ffff:10.0.0.0")?,
                prefix,
            };
            assert_eq!(odd.unmapped(), odd, "/{prefix}");
        }
        Ok(())
    }

    #[test]
    fn cidr_prefix_must_be_plain_decimal() {
        for entry in [
            "203.0.113.0/",
            "203.0.113.0/+24",
            "203.0.113.0/-1",
            "203.0.113.0/024",
            "203.0.113.0/00",
            "203.0.113.0/ 24",
            "203.0.113.0/24 ",
            "203.0.113.0/24/1",
            "203.0.113.0/0x18",
            "203.0.113.0/2.4",
            "203.0.113.0/255.255.255.0",
            "2001:db8::/064",
        ] {
            assert_eq!(
                parse_cidr_or_host(entry),
                Err(AddrError::BadPrefix {
                    entry: entry.to_owned()
                }),
                "{entry:?}"
            );
        }
    }

    #[test]
    fn hostnames_and_garbage_are_refused() {
        for entry in [
            "",
            "/24",
            "node.example",
            "node.example/24",
            "localhost",
            " 198.51.100.9",
            "198.51.100.9 ",
            "198.51.100",
            "198.051.100.9",
            "198.51.100.256",
            "0x7f.0.0.1",
            "198.51.100.9,203.0.113.0/24",
            "[2001:db8::7]",
            "fe80::1%eth0",
            "fe80::1%eth0/64",
            "2001:db8:::7",
            "/ip4/198.51.100.9",
        ] {
            assert_eq!(
                parse_cidr_or_host(entry),
                Err(AddrError::NotAnIp {
                    entry: entry.to_owned()
                }),
                "{entry:?}"
            );
        }
    }

    #[test]
    fn errors_name_the_entry_escaped() {
        let messages = [
            "node.example",
            "203.0.113.0/x",
            "203.0.113.0/33",
            "::ffff:10.0.0.0/95",
            "bad\nline\u{1b}[2J",
        ]
        .map(|entry| parse_cidr_or_host(entry).map_or_else(|e| e.to_string(), |c| c.to_string()));
        assert_eq!(
            messages,
            [
                "\"node.example\" is not an IP address or a CIDR (hostnames are not resolved)",
                "\"203.0.113.0/x\" has a prefix length that is not a plain decimal number",
                "\"203.0.113.0/33\" has a prefix length longer than 32",
                "\"::ffff:10.0.0.0/95\" is an IPv4-mapped address with a prefix length shorter than 96",
                "\"bad\\nline\\u{1b}[2J\" is not an IP address or a CIDR (hostnames are not resolved)",
            ]
        );
    }

    #[test]
    fn the_config_example_parses() -> TestResult {
        let entries = "203.0.113.0/24,198.51.100.9"
            .split(',')
            .map(parse_cidr_or_host)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            entries,
            [
                Cidr {
                    net: v4(203, 0, 113, 0),
                    prefix: 24
                },
                Cidr {
                    net: v4(198, 51, 100, 9),
                    prefix: 32
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn ipv6_prints_rfc5952_compressed() -> TestResult {
        for (written, printed) in [
            ("2001:0DB8:0000:0000:0000:0000:0000:0001", "2001:db8::1"),
            ("2001:db8:0:0:1:0:0:1", "2001:db8::1:0:0:1"),
            ("2001:db8:0:1:1:1:1:1", "2001:db8:0:1:1:1:1:1"),
            ("2001:0:0:1:0:0:0:1", "2001:0:0:1::1"),
            ("0:0:0:0:0:0:0:0", "::"),
            ("0:0:0:0:0:0:0:1", "::1"),
            ("fe80:0:0:0:0:0:0:0", "fe80::"),
            ("::ffff:192.0.2.10", "::ffff:192.0.2.10"),
            ("::192.0.2.10", "::c000:20a"),
        ] {
            let ip = v6(written)?;
            assert_eq!(ip.to_string(), printed, "{written}");
            assert_eq!(v6(printed)?, ip, "{printed}");
        }
        for (written, printed) in [
            ("2001:0DB8:0:0::/32", "2001:db8::/32"),
            ("2001:db8:0:0:1:0:0:1/128", "2001:db8::1:0:0:1/128"),
        ] {
            let net = cidr(written)?;
            assert_eq!(net.to_string(), printed, "{written}");
            assert_eq!(cidr(printed)?, net, "{printed}");
        }
        Ok(())
    }

    #[test]
    fn cidr_display_round_trips() -> TestResult {
        for entry in [
            "0.0.0.0/0",
            "9.0.0.0/8",
            "203.0.113.0/24",
            "198.51.100.9/32",
            "::/0",
            "2001:db8::/32",
            "2001:db8::7/128",
            "::ffff:192.0.2.0/120",
        ] {
            let net = cidr(entry)?;
            assert_eq!(net.to_string(), entry);
            assert_eq!(cidr(&net.to_string())?, net);
        }
        Ok(())
    }

    #[test]
    fn cidr_orders_by_family_then_network_then_prefix() -> TestResult {
        let mut set = BTreeSet::new();
        for entry in [
            "2001:db8::/32",
            "10.0.0.0/16",
            "::1",
            "192.0.2.0/24",
            "10.0.0.0/8",
            "9.0.0.0/8",
            "::/0",
            "2001:db8::/48",
            "203.0.113.77/24",
            "203.0.113.0/24",
            "0.0.0.0/0",
            "fe80::/10",
        ] {
            set.insert(cidr(entry)?);
        }
        let printed: Vec<String> = set.iter().map(Cidr::to_string).collect();
        assert_eq!(
            printed,
            [
                "0.0.0.0/0",
                "9.0.0.0/8",
                "10.0.0.0/8",
                "10.0.0.0/16",
                "192.0.2.0/24",
                "203.0.113.0/24",
                "::/0",
                "::1/128",
                "2001:db8::/32",
                "2001:db8::/48",
                "fe80::/10",
            ]
        );
        assert!(cidr("255.255.255.255")? < cidr("::/0")?);
        assert!(cidr("10.0.0.0/8")? < cidr("10.0.0.0/9")?);
        assert!(cidr("10.0.0.0/32")? < cidr("10.0.1.0/24")?);
        Ok(())
    }

    #[test]
    fn split_ips_by_family_in_numeric_order() -> TestResult {
        let (ips_v4, ips_v6) = split_ips([
            v4(192, 0, 2, 10),
            v6("2001:db8::10")?,
            v4(9, 9, 9, 9),
            v4(192, 0, 2, 9),
            v6("::1")?,
            v4(192, 0, 2, 10),
            v6("2001:db8::9")?,
            v6("2001:db8::10")?,
            v4(10, 0, 0, 1),
        ]);
        assert_eq!(
            ips_v4.iter().map(Ipv4Addr::to_string).collect::<Vec<_>>(),
            ["9.9.9.9", "10.0.0.1", "192.0.2.9", "192.0.2.10"]
        );
        assert_eq!(
            ips_v6.iter().map(Ipv6Addr::to_string).collect::<Vec<_>>(),
            ["::1", "2001:db8::9", "2001:db8::10"]
        );
        let (none_v4, none_v6) = split_ips([]);
        assert!(none_v4.is_empty() && none_v6.is_empty());
        Ok(())
    }
}
