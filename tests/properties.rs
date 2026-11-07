//! Seeded property tests: canonical addresses, multiaddr classification, the canonical order, the rendered program and the IPv4-mapped network rule.
//!
//! Each property runs the seeds `1..=CASES` with one [`Rng`] per seed, and every assertion names the seed and the generated input,
//! so a failure names the case that reproduces it.
//! Nothing is read from the clock or the environment, so every run checks the same cases.
//! `permuted_inputs_render_byte_equal` in `render_golden.rs` reverses the golden cases; the render properties here generalise it to random inputs, orders and spellings.

mod support;

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::Debug;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::panic;

use support::TempDir;
use support::rng::Rng;
use tn_p2p_allow_sync::addr::{
    AddrClass, AddrError, Cidr, canonical_ip, classify_multiaddr, parse_cidr_or_host,
};
use tn_p2p_allow_sync::config::{CONFIG_FILE, Config, ConfigError, load_config};
use tn_p2p_allow_sync::render::{HEADER, Program, render};
use tn_p2p_allow_sync::sync::Sets;

type TestResult = Result<(), Box<dyn Error>>;

/// How many seeds each property runs.
const CASES: u64 = 256;

/// The 1-based line of `P2P_ALLOW_FROM` in the text [`config_text`] writes.
const ALLOW_FROM_LINE: usize = 5;

/// The prefix length of the IPv4-mapped range `::ffff:0:0/96`.
const MAPPED_PREFIX: u8 = 96;

// ---- generators ----

/// 32 random bits.
fn bits32(rng: &mut Rng) -> u32 {
    let [a, b, c, d, ..] = rng.next_u64().to_le_bytes();
    u32::from_le_bytes([a, b, c, d])
}

/// 128 random bits.
fn bits128(rng: &mut Rng) -> u128 {
    u128::from(rng.next_u64()).wrapping_shl(64) | u128::from(rng.next_u64())
}

/// A value in `0..n`.
fn below_u8(rng: &mut Rng, n: u8) -> u8 {
    u8::try_from(rng.below(u64::from(n))).unwrap_or(0)
}

/// A port 1 to 65535, with the two ends and the 6.6 ports more often than chance would give them.
fn port(rng: &mut Rng) -> u16 {
    match rng.below(8) {
        0 => 1,
        1 => u16::MAX,
        2 => 49590,
        3 => 49594,
        _ => u16::try_from(rng.below(65535))
            .unwrap_or(0)
            .saturating_add(1),
    }
}

/// A random IPv4 address, often one of the kinds [`canonical_ip`] treats differently.
fn any_v4(rng: &mut Rng) -> Ipv4Addr {
    let bits = bits32(rng);
    Ipv4Addr::from(match rng.below(8) {
        0 => 0,
        // link-local 169.254.0.0/16
        1 => 0xA9FE_0000 | (bits & 0xFFFF),
        // multicast 224.0.0.0/4
        2 => 0xE000_0000 | (bits & 0x0FFF_FFFF),
        // loopback 127.0.0.0/8
        3 => 0x7F00_0000 | (bits & 0x00FF_FFFF),
        // private 10.0.0.0/8
        4 => 0x0A00_0000 | (bits & 0x00FF_FFFF),
        5 => u32::MAX,
        _ => bits,
    })
}

/// A random IPv6 address, often one of the kinds [`canonical_ip`] treats differently.
fn any_v6(rng: &mut Rng) -> Ipv6Addr {
    let bits = bits128(rng);
    let v4 = any_v4(rng);
    match rng.below(10) {
        0 => v4.to_ipv6_mapped(),
        1 => v4.to_ipv6_compatible(),
        2 => Ipv6Addr::UNSPECIFIED,
        3 => Ipv6Addr::LOCALHOST,
        // link-local fe80::/10
        4 => Ipv6Addr::from(0xFE80_u128.wrapping_shl(112) | (bits & u128::MAX.wrapping_shr(10))),
        // multicast ff00::/8
        5 => Ipv6Addr::from(0xFF_u128.wrapping_shl(120) | (bits & u128::MAX.wrapping_shr(8))),
        // documentation 2001:db8::/32
        6 => {
            Ipv6Addr::from(0x2001_0DB8_u128.wrapping_shl(96) | (bits & u128::MAX.wrapping_shr(32)))
        }
        // a few low bits: next to ::, ::1 and the IPv4-compatible range
        7 => Ipv6Addr::from(bits & 0xFFFF),
        _ => Ipv6Addr::from(bits),
    }
}

/// A random address of either family.
fn any_ip(rng: &mut Rng) -> IpAddr {
    if rng.chance(1, 2) {
        IpAddr::V4(any_v4(rng))
    } else {
        IpAddr::V6(any_v6(rng))
    }
}

/// Up to `most - 1` random addresses that [`canonical_ip`] keeps, as memory and the floor hold them.
fn canonical_ips(rng: &mut Rng, most: u64) -> BTreeSet<IpAddr> {
    (0..rng.below(most))
        .filter_map(|_| canonical_ip(any_ip(rng)))
        .collect()
}

/// The prefix length of a host: 32 for IPv4, 128 for IPv6.
fn full_prefix(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    }
}

/// The address width in bits and the address as a number.
fn ip_bits(ip: IpAddr) -> (u32, u128) {
    match ip {
        IpAddr::V4(v4) => (32, u128::from(u32::from(v4))),
        IpAddr::V6(v6) => (128, u128::from(v6)),
    }
}

/// The address of `width` bits holding the low bits of `value`.
fn ip_from_bits(width: u32, value: u128) -> IpAddr {
    if width == 32 {
        IpAddr::V4(Ipv4Addr::from(
            u32::try_from(value & u128::from(u32::MAX)).unwrap_or(0),
        ))
    } else {
        IpAddr::V6(Ipv6Addr::from(value))
    }
}

/// The bits of a `width`-bit address that a `prefix` network keeps.
fn net_mask(width: u32, prefix: u8) -> u128 {
    let all = if width == 32 {
        u128::from(u32::MAX)
    } else {
        u128::MAX
    };
    let host = width.saturating_sub(u32::from(prefix));
    all & u128::MAX.checked_shl(host).unwrap_or(0)
}

/// Whether `ip` is IPv4-mapped IPv6 (`::ffff:a.b.c.d`).
fn is_mapped(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some())
}

/// Whether `ip` is never a peer's source by the documented rule: unspecified, multicast or link-local.
fn dropped(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_unspecified() || v4.is_multicast() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast() || v6.is_unicast_link_local(),
    }
}

/// A random network: any address with any prefix of its family.
fn any_cidr(rng: &mut Rng, seed: u64) -> Result<Cidr, String> {
    let ip = any_ip(rng);
    let prefix = below_u8(rng, full_prefix(ip).saturating_add(1));
    Cidr::new(ip, prefix)
        .ok_or_else(|| format!("seed {seed}: Cidr::new({ip}, {prefix}) refused a prefix in range"))
}

/// A network next to, around, inside or equal to `base`: the neighbours the static sets and the covered-network rule must handle.
fn related(rng: &mut Rng, base: Cidr) -> Option<Cidr> {
    let (width, bits) = ip_bits(base.net);
    let full = full_prefix(base.net);
    match rng.below(4) {
        0 => {
            // the next block of the same size, wrapping at the top of the space
            let host = width.checked_sub(u32::from(base.prefix))?;
            let step = 1u128.checked_shl(host).unwrap_or(0);
            Cidr::new(ip_from_bits(width, bits.wrapping_add(step)), base.prefix)
        }
        // a network that covers it
        1 => Cidr::new(base.net, below_u8(rng, base.prefix.saturating_add(1))),
        2 => {
            // a network inside it: a longer prefix and random host bits
            let extra = below_u8(rng, full.saturating_sub(base.prefix).saturating_add(1));
            let inside = bits | (bits128(rng) & !net_mask(width, base.prefix));
            Cidr::new(
                ip_from_bits(width, inside),
                base.prefix.saturating_add(extra),
            )
        }
        _ => Some(base),
    }
}

/// Whether `c` is a `P2P_ALLOW_FROM` entry in its canonical form: a host [`canonical_ip`] keeps as it is, or a network outside the IPv4-mapped range.
fn canonical_entry(c: Cidr) -> bool {
    !is_mapped(c.net) && (!c.is_host() || canonical_ip(c.net) == Some(c.net))
}

/// A random entry in canonical form, or `None` when the draw is not one.
fn entry(rng: &mut Rng) -> Option<Cidr> {
    let ip = any_ip(rng);
    let full = full_prefix(ip);
    let prefix = if rng.chance(1, 2) {
        full
    } else {
        below_u8(rng, full.saturating_add(1))
    };
    Cidr::new(ip, prefix).filter(|c| canonical_entry(*c))
}

/// Appends up to `most - 1` copies of elements already in `items`.
fn duplicate<T: Copy>(rng: &mut Rng, items: &mut Vec<T>, most: u64) {
    for _ in 0..rng.below(most) {
        if let Some(item) = rng.pick(items.as_slice()).copied() {
            items.push(item);
        }
    }
}

/// Shuffles `items` in place (Fisher-Yates).
fn shuffle<T>(rng: &mut Rng, items: &mut [T]) {
    for i in (1..items.len()).rev() {
        let bound = u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1);
        let j = usize::try_from(rng.below(bound)).unwrap_or(0);
        items.swap(i, j);
    }
}

// ---- spellings ----

/// One spelling of `v6`: compressed as std prints it, uncompressed, zero-padded or in upper case.
fn spell_v6(rng: &mut Rng, v6: Ipv6Addr) -> String {
    let [a, b, c, d, e, f, g, h] = v6.segments();
    match rng.below(4) {
        0 => format!("{a:x}:{b:x}:{c:x}:{d:x}:{e:x}:{f:x}:{g:x}:{h:x}"),
        1 => format!("{a:04x}:{b:04x}:{c:04x}:{d:04x}:{e:04x}:{f:04x}:{g:04x}:{h:04x}"),
        2 => v6.to_string().to_uppercase(),
        _ => v6.to_string(),
    }
}

/// One spelling of the IPv4-mapped form of `v4`, without a prefix.
fn spell_mapped(rng: &mut Rng, v4: Ipv4Addr) -> String {
    let [a, b, c, d] = v4.octets();
    let (hi, lo) = (u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d]));
    match rng.below(5) {
        0 => format!("::FFFF:{v4}"),
        1 => format!("0:0:0:0:0:ffff:{v4}"),
        2 => format!("::ffff:{hi:x}:{lo:x}"),
        3 => format!("0000:0000:0000:0000:0000:FFFF:{hi:04X}:{lo:04X}"),
        _ => format!("::ffff:{v4}"),
    }
}

/// One spelling of the canonical entry `entry` for `P2P_ALLOW_FROM`; every spelling of one entry names the same addresses.
///
/// A network may carry host bits, an IPv4 entry may be written IPv4-mapped, and a host may carry its full prefix.
fn spell(rng: &mut Rng, entry: Cidr) -> String {
    let (width, bits) = ip_bits(entry.net);
    let host = entry.is_host();
    let written = if host || rng.chance(1, 2) {
        entry.net
    } else {
        ip_from_bits(
            width,
            bits | (bits128(rng) & !net_mask(width, entry.prefix)),
        )
    };
    // host bits must not make an IPv6 network read as IPv4-mapped, which the config refuses below /96
    let written = if is_mapped(written) {
        entry.net
    } else {
        written
    };
    let bare = rng.chance(1, 2);
    match written {
        IpAddr::V4(v4) if rng.chance(1, 2) => {
            let mapped = spell_mapped(rng, v4);
            if host && bare {
                mapped
            } else {
                format!("{mapped}/{}", entry.prefix.saturating_add(MAPPED_PREFIX))
            }
        }
        IpAddr::V4(v4) if host && bare => v4.to_string(),
        IpAddr::V4(v4) => format!("{v4}/{}", entry.prefix),
        IpAddr::V6(v6) => {
            let text = spell_v6(rng, v6);
            if host && bare {
                text
            } else {
                format!("{text}/{}", entry.prefix)
            }
        }
    }
}

/// The text of a minimal valid config with these ports and `P2P_ALLOW_FROM` entries, which lands on [`ALLOW_FROM_LINE`].
fn config_text(ports: &[u16], allow_from: &[String]) -> String {
    let ports: Vec<String> = ports.iter().map(u16::to_string).collect();
    format!(
        "# tn-p2p-allow-sync config, written by tests/properties.rs\nCONFIG_VERSION=1\nRPC_PORT=8545\nP2P_PORTS={}\nP2P_ALLOW_FROM={}\n",
        ports.join(","),
        allow_from.join(",")
    )
}

/// Writes `text` as the config in `dir` and loads it through the public reader.
fn load(dir: &TempDir, text: &str) -> Result<Result<Config, ConfigError>, Box<dyn Error>> {
    let path = dir.join(CONFIG_FILE);
    fs::write(&path, text)?;
    Ok(load_config(&path))
}

/// Asserts that the config `text` was refused as `config-invalid` on the `P2P_ALLOW_FROM` line, naming `entry`.
fn assert_refused(seed: u64, text: &str, loaded: &Result<Config, ConfigError>, entry: &str) {
    assert!(
        loaded.is_err(),
        "seed {seed}: {text:?} was accepted: {loaded:?}"
    );
    if let Err(e) = loaded {
        assert_eq!(e.token(), "config-invalid", "seed {seed}: {text:?}: {e}");
        assert_eq!(
            e.line(),
            Some(ALLOW_FROM_LINE),
            "seed {seed}: {text:?}: {e}"
        );
        let named = format!("{entry:?}");
        assert!(
            e.to_string().contains(&named),
            "seed {seed}: the refusal of {text:?} does not name {named}: {e}"
        );
    }
}

// ---- 1. addresses ----

#[test]
fn canonical_ip_is_a_fixpoint_that_never_keeps_a_dropped_address() {
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        for _ in 0..16 {
            let ip = any_ip(&mut rng);
            let canonical = canonical_ip(ip);
            if let Some(c) = canonical {
                assert_eq!(
                    canonical_ip(c),
                    Some(c),
                    "seed {seed}: canonical_ip is not idempotent on {ip} -> {c}"
                );
                assert!(
                    !dropped(c),
                    "seed {seed}: canonical_ip({ip}) kept {c}, which is never a peer's source"
                );
                assert!(
                    !is_mapped(c),
                    "seed {seed}: canonical_ip({ip}) is still IPv4-mapped: {c}"
                );
            }
            let mapped = match ip {
                IpAddr::V6(v6) => v6.to_ipv4_mapped(),
                IpAddr::V4(_) => None,
            };
            match mapped {
                Some(v4) => {
                    assert_eq!(
                        canonical,
                        canonical_ip(IpAddr::V4(v4)),
                        "seed {seed}: mapped {ip} and {v4} canonicalise differently"
                    );
                    assert!(
                        canonical.is_none_or(|c| c == IpAddr::V4(v4)),
                        "seed {seed}: mapped {ip} canonicalised to {canonical:?}, not to {v4}"
                    );
                }
                // the IPv4-compatible form `::a.b.c.d` lands here and must stay IPv6
                None => assert!(
                    canonical.is_none_or(|c| c == ip),
                    "seed {seed}: {ip} is not mapped but canonicalised to {canonical:?}"
                ),
            }
            let unmapped = mapped.map_or(ip, IpAddr::V4);
            assert_eq!(
                canonical.is_none(),
                dropped(unmapped),
                "seed {seed}: {ip} (as {unmapped}) gave {canonical:?}"
            );
        }
    }
}

#[test]
fn ips_and_networks_round_trip_through_their_text() {
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        for _ in 0..16 {
            let ip = any_ip(&mut rng);
            let text = ip.to_string();
            assert_eq!(
                text.parse::<IpAddr>().ok(),
                Some(ip),
                "seed {seed}: {ip:?} printed as {text:?}"
            );
            match ip {
                IpAddr::V4(v4) => assert_eq!(
                    text.parse::<Ipv4Addr>().ok(),
                    Some(v4),
                    "seed {seed}: {text:?}"
                ),
                IpAddr::V6(v6) => assert_eq!(
                    text.parse::<Ipv6Addr>().ok(),
                    Some(v6),
                    "seed {seed}: {text:?}"
                ),
            }

            let max = full_prefix(ip);
            // a few prefixes past the family's end, which `Cidr::new` refuses
            let prefix = below_u8(&mut rng, max.saturating_add(9));
            let Some(cidr) = Cidr::new(ip, prefix) else {
                assert!(
                    prefix > max,
                    "seed {seed}: Cidr::new({ip}, {prefix}) refused a prefix in range"
                );
                continue;
            };
            assert!(
                prefix <= max,
                "seed {seed}: Cidr::new({ip}, {prefix}) accepted a prefix longer than {max}: {cidr}"
            );
            assert_eq!(cidr.prefix, prefix, "seed {seed}: {ip}/{prefix} -> {cidr}");
            let (width, bits) = ip_bits(ip);
            assert_eq!(
                ip_bits(cidr.net),
                (width, bits & net_mask(width, prefix)),
                "seed {seed}: Cidr::new({ip}, {prefix}) is not the masked network: {cidr}"
            );
            assert_eq!(
                Cidr::new(cidr.net, prefix),
                Some(cidr),
                "seed {seed}: masking {cidr} again changed it"
            );
            assert_eq!(
                cidr.is_host(),
                prefix == max,
                "seed {seed}: is_host of {cidr}"
            );
            let text = cidr.to_string();
            assert_eq!(
                parse_cidr_or_host(&text),
                Ok(cidr),
                "seed {seed}: Cidr::new({ip}, {prefix}) printed as {text:?}"
            );
        }
    }
}

#[test]
fn ip_multiaddrs_classify_to_the_bare_address() {
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        for _ in 0..8 {
            let ip = any_ip(&mut rng);
            let p = port(&mut rng);
            let tail = match rng.below(6) {
                0 => String::new(),
                1 => "/".to_owned(),
                2 => format!("/tcp/{p}"),
                3 => format!(
                    "/udp/{p}/quic-v1/p2p/12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN"
                ),
                4 => format!("/udp/{p}/quic-v1/{}", garbage(&mut rng)),
                _ => format!("/udp/{p}/quic-v1"),
            };
            let multiaddr = match ip {
                IpAddr::V4(v4) => format!("/ip4/{v4}{tail}"),
                IpAddr::V6(v6) => format!("/ip6/{}{tail}", spell_v6(&mut rng, v6)),
            };
            let class = classify_multiaddr(&multiaddr);
            assert_eq!(class, AddrClass::Ip(ip), "seed {seed}: {multiaddr:?}");
            let via_multiaddr = match class {
                AddrClass::Ip(got) => canonical_ip(got),
                AddrClass::Dns | AddrClass::Unparsed => None,
            };
            assert_eq!(
                via_multiaddr,
                canonical_ip(ip),
                "seed {seed}: {multiaddr:?} and {ip} canonicalise differently"
            );
        }
    }
}

/// Leading text for [`garbage`]: real protocol prefixes, near misses and none.
const GARBAGE_PREFIXES: [&str; 23] = [
    "",
    "/",
    "//",
    "/ip4/",
    "/ip6/",
    "/dns/",
    "/dns4/",
    "/dns6/",
    "/dnsaddr/",
    "/ip4",
    "/ip6",
    "/dns",
    "/dnsaddr",
    "ip4/",
    "/IP4/",
    "/Ip6/",
    "/IP6/",
    "/tcp/",
    "/udp/",
    "/p2p/",
    " /ip4/",
    "/ip4/ ",
    "/ip4//",
];

/// Characters for [`garbage`]: separators, address characters, whitespace, control and non-ASCII.
const GARBAGE_CHARS: [char; 28] = [
    '/', '/', '.', ':', '0', '1', '2', '5', '9', 'a', 'f', 'F', 'x', 'i', 'p', '4', '6', 'd', 'n',
    's', '%', '[', ']', ' ', '\t', '\0', 'é', '\u{202e}',
];

/// A random string that may or may not be a multiaddr: a prefix, maybe an address, then random characters.
fn garbage(rng: &mut Rng) -> String {
    let mut s = String::from(*rng.pick(&GARBAGE_PREFIXES).unwrap_or(&""));
    if rng.chance(1, 2) {
        let ip = any_ip(rng);
        s.push_str(&match ip {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => spell_v6(rng, v6),
        });
        if rng.chance(1, 2) {
            s.push('/');
        }
    }
    for _ in 0..rng.below(16) {
        s.push(*rng.pick(&GARBAGE_CHARS).unwrap_or(&'/'));
    }
    s
}

/// The class the documented rules give `s`, read off its leading text.
///
/// `/ip4/` or `/ip6/` followed by an address of that family up to the next `/` is that address, and anything else after either is unparsed.
/// `/dns/`, `/dns4/`, `/dns6/` or `/dnsaddr/` followed by a non-empty name is DNS. Everything else is unparsed.
fn expected_class(s: &str) -> AddrClass {
    for (protocol, v4) in [("/ip4/", true), ("/ip6/", false)] {
        if let Some(rest) = s.strip_prefix(protocol) {
            let value = rest.split('/').next().unwrap_or(rest);
            let ip = if v4 {
                value.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
            } else {
                value.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
            };
            return ip.map_or(AddrClass::Unparsed, AddrClass::Ip);
        }
    }
    for protocol in ["/dns/", "/dns4/", "/dns6/", "/dnsaddr/"] {
        if let Some(rest) = s.strip_prefix(protocol) {
            return if rest.is_empty() || rest.starts_with('/') {
                AddrClass::Unparsed
            } else {
                AddrClass::Dns
            };
        }
    }
    AddrClass::Unparsed
}

#[test]
fn any_multiaddr_text_classifies_by_its_leading_segment_without_panicking() -> TestResult {
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        for _ in 0..16 {
            let text = garbage(&mut rng);
            let class = panic::catch_unwind(|| classify_multiaddr(&text))
                .map_err(|_| format!("seed {seed}: classify_multiaddr panicked on {text:?}"))?;
            assert_eq!(class, expected_class(&text), "seed {seed}: {text:?}");
            if class != AddrClass::Unparsed {
                // everything after the first address is ignored
                let longer = format!("{text}/{}", garbage(&mut rng));
                assert_eq!(
                    classify_multiaddr(&longer),
                    class,
                    "seed {seed}: {longer:?} classifies unlike {text:?}"
                );
            }
        }
    }
    Ok(())
}

// ---- 2. canonical order ----

/// The order of a network computed without `Ord`: family (IPv4 first), then value, then prefix.
fn cidr_key(c: &Cidr) -> (u32, u128, u8) {
    let (width, bits) = ip_bits(c.net);
    (width, bits, c.prefix)
}

/// The order of an address computed without `Ord`: family (IPv4 first), then value.
fn ip_key(ip: &IpAddr) -> (u32, u128) {
    ip_bits(*ip)
}

/// Asserts that the `Ord` of `items` is total, agrees with `==` and equals the numeric `key` order, and that a `BTreeSet` iterates in it.
fn assert_canonical_order<T, K>(seed: u64, rng: &mut Rng, items: &[T], key: fn(&T) -> K)
where
    T: Ord + Copy + Debug,
    K: Ord,
{
    for a in items {
        for b in items {
            let ab = a.cmp(b);
            assert_eq!(
                ab,
                key(a).cmp(&key(b)),
                "seed {seed}: {a:?} cmp {b:?} is not the numeric order"
            );
            assert_eq!(
                ab,
                b.cmp(a).reverse(),
                "seed {seed}: {a:?} cmp {b:?} is not antisymmetric"
            );
            assert_eq!(
                ab == Ordering::Equal,
                a == b,
                "seed {seed}: {a:?} cmp {b:?} disagrees with =="
            );
            assert_eq!(
                a.partial_cmp(b),
                Some(ab),
                "seed {seed}: partial_cmp of {a:?} and {b:?}"
            );
        }
    }
    for _ in 0..256 {
        let (Some(a), Some(b), Some(c)) = (rng.pick(items), rng.pick(items), rng.pick(items))
        else {
            break;
        };
        if a <= b && b <= c {
            assert!(
                a <= c,
                "seed {seed}: {a:?} <= {b:?} <= {c:?} but not {a:?} <= {c:?}"
            );
        }
        if a < b && b < c {
            assert!(
                a < c,
                "seed {seed}: {a:?} < {b:?} < {c:?} but not {a:?} < {c:?}"
            );
        }
        if a == b && b == c {
            assert!(
                a == c,
                "seed {seed}: {a:?} == {b:?} == {c:?} but not {a:?} == {c:?}"
            );
        }
    }
    let set: Vec<T> = items
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut sorted = items.to_vec();
    sorted.sort();
    sorted.dedup();
    assert_eq!(set, sorted, "seed {seed}: BTreeSet order of {items:?}");
    let mut by_key = items.to_vec();
    by_key.sort_by_key(key);
    by_key.dedup();
    assert_eq!(set, by_key, "seed {seed}: numeric order of {items:?}");
}

#[test]
fn cidr_and_ip_order_is_total_numeric_and_ipv4_first() -> TestResult {
    const PREFIXES: [u8; 10] = [0, 1, 8, 24, 31, 32, 64, 96, 127, 128];
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        // a small pool of addresses, so that equal and nearly equal values are common
        let pool: Vec<IpAddr> = (0..4).map(|_| any_ip(&mut rng)).collect();
        let mut ips = Vec::new();
        let mut cidrs = Vec::new();
        for _ in 0..24 {
            let ip = match rng.pick(&pool).copied() {
                Some(ip) if rng.chance(3, 4) => ip,
                _ => any_ip(&mut rng),
            };
            ips.push(ip);
            let max = full_prefix(ip);
            let prefix = match rng.pick(&PREFIXES).copied() {
                Some(prefix) if prefix <= max && rng.chance(3, 4) => prefix,
                _ => below_u8(&mut rng, max.saturating_add(1)),
            };
            cidrs.push(Cidr::new(ip, prefix).ok_or_else(|| {
                format!("seed {seed}: Cidr::new({ip}, {prefix}) refused a prefix in range")
            })?);
        }
        assert_canonical_order(seed, &mut rng, &ips, ip_key);
        assert_canonical_order(seed, &mut rng, &cidrs, cidr_key);

        let ip_set: BTreeSet<IpAddr> = ips.iter().copied().collect();
        assert!(
            ip_set
                .iter()
                .skip_while(|ip| ip.is_ipv4())
                .all(|ip| ip.is_ipv6()),
            "seed {seed}: an IPv6 address sorts before an IPv4 one in {ip_set:?}"
        );
        let cidr_set: BTreeSet<Cidr> = cidrs.iter().copied().collect();
        assert!(
            cidr_set
                .iter()
                .skip_while(|c| c.net.is_ipv4())
                .all(|c| c.net.is_ipv6()),
            "seed {seed}: an IPv6 network sorts before an IPv4 one in {cidr_set:?}"
        );
    }
    Ok(())
}

// ---- 3. render ----

/// Owned inputs of one [`Program`], collected from lists in the order given.
struct Inputs {
    ports: BTreeSet<u16>,
    allow_v4: BTreeSet<Ipv4Addr>,
    allow_v6: BTreeSet<Ipv6Addr>,
    static_v4: BTreeSet<Cidr>,
    static_v6: BTreeSet<Cidr>,
}

impl Inputs {
    fn collect(ports: &[u16], v4: &[Ipv4Addr], v6: &[Ipv6Addr], networks: &[Cidr]) -> Self {
        let family = |ipv4: bool| -> BTreeSet<Cidr> {
            networks
                .iter()
                .filter(|c| c.net.is_ipv4() == ipv4)
                .copied()
                .collect()
        };
        Self {
            ports: ports.iter().copied().collect(),
            allow_v4: v4.iter().copied().collect(),
            allow_v6: v6.iter().copied().collect(),
            static_v4: family(true),
            static_v6: family(false),
        }
    }

    fn program(&self) -> Program<'_> {
        Program {
            ports: &self.ports,
            allow_v4: &self.allow_v4,
            allow_v6: &self.allow_v6,
            static_v4: &self.static_v4,
            static_v6: &self.static_v6,
        }
    }
}

/// Asserts what every program text holds: the contract's header first, and one newline at the end after the last `}`.
fn assert_program_shape(seed: u64, text: &str, inputs: &dyn Debug) {
    assert_eq!(
        text.lines().next(),
        Some(HEADER),
        "seed {seed}: the program of {inputs:?} does not start with the header"
    );
    assert!(
        text.ends_with("}\n") && !text.ends_with("\n\n"),
        "seed {seed}: the program of {inputs:?} does not end with one newline after }}: {text:?}"
    );
}

#[test]
fn program_inputs_in_any_order_render_byte_equal() -> TestResult {
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        let mut ports: Vec<u16> = (0..=rng.below(6)).map(|_| port(&mut rng)).collect();
        duplicate(&mut rng, &mut ports, 3);
        let mut v4: Vec<Ipv4Addr> = (0..rng.below(12))
            .filter_map(|_| match canonical_ip(IpAddr::V4(any_v4(&mut rng))) {
                Some(IpAddr::V4(v4)) => Some(v4),
                _ => None,
            })
            .collect();
        duplicate(&mut rng, &mut v4, 3);
        let mut v6: Vec<Ipv6Addr> = (0..rng.below(12))
            .filter_map(|_| match canonical_ip(IpAddr::V6(any_v6(&mut rng))) {
                Some(IpAddr::V6(v6)) => Some(v6),
                _ => None,
            })
            .collect();
        duplicate(&mut rng, &mut v6, 3);
        let mut networks = Vec::new();
        for _ in 0..rng.below(8) {
            networks.push(any_cidr(&mut rng, seed)?);
        }
        for _ in 0..rng.below(8) {
            if let Some(near) = rng
                .pick(&networks)
                .copied()
                .and_then(|base| related(&mut rng, base))
            {
                networks.push(near);
            }
        }
        // the renderer gets hosts in the allow sets and networks in the static sets
        networks.retain(|c| !c.is_host());

        let mut first: Option<(String, String)> = None;
        for round in 0..4 {
            shuffle(&mut rng, &mut ports);
            shuffle(&mut rng, &mut v4);
            shuffle(&mut rng, &mut v6);
            shuffle(&mut rng, &mut networks);
            let order = format!("ports {ports:?} v4 {v4:?} v6 {v6:?} networks {networks:?}");
            let inputs = Inputs::collect(&ports, &v4, &v6, &networks);
            let program = inputs.program();
            let text = render(&program);
            assert_eq!(
                render(&program),
                text,
                "seed {seed} round {round}: rendering {order} twice differs"
            );
            assert_eq!(
                program.to_string(),
                text,
                "seed {seed} round {round}: Display and render differ for {order}"
            );
            assert_program_shape(seed, &text, &order);
            match &first {
                None => first = Some((order, text)),
                Some((order0, text0)) => assert_eq!(
                    &text, text0,
                    "seed {seed} round {round}: {order} renders unlike {order0}"
                ),
            }
        }
    }
    Ok(())
}

/// Whether the network `wide` holds every address of `narrow`.
fn covers(wide: Cidr, narrow: Cidr) -> bool {
    wide.net.is_ipv4() == narrow.net.is_ipv4()
        && wide.prefix <= narrow.prefix
        && Cidr::new(narrow.net, wide.prefix) == Some(wide)
}

#[test]
fn config_entries_in_any_order_and_spelling_give_the_same_sets_and_program() -> TestResult {
    let dir = TempDir::new("properties-render")?;
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        let mut entries: Vec<Cidr> = (0..rng.below(10)).filter_map(|_| entry(&mut rng)).collect();
        for _ in 0..rng.below(10) {
            if let Some(near) = rng
                .pick(&entries)
                .copied()
                .and_then(|base| related(&mut rng, base))
                .filter(|c| canonical_entry(*c))
            {
                entries.push(near);
            }
        }
        duplicate(&mut rng, &mut entries, 3);
        // `P2P_PORTS` refuses a port listed twice
        let mut ports: Vec<u16> = (0..=rng.below(6))
            .map(|_| port(&mut rng))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let dynamic = canonical_ips(&mut rng, 12);
        let floor = canonical_ips(&mut rng, 6);

        // what the documented rules make of the entries: hosts join memory and the floor in the allow sets,
        // and networks go to the static sets unless another network of the list covers them
        let listed: BTreeSet<Cidr> = entries.iter().copied().collect();
        let networks: Vec<Cidr> = listed.iter().filter(|c| !c.is_host()).copied().collect();
        let kept: BTreeSet<Cidr> = networks
            .iter()
            .filter(|n| !networks.iter().any(|m| m != *n && covers(*m, **n)))
            .copied()
            .collect();
        let hosts = listed.iter().filter(|c| c.is_host()).map(|c| c.net);
        let allow: BTreeSet<IpAddr> = dynamic.iter().chain(&floor).copied().chain(hosts).collect();
        let dropped_count = networks
            .len()
            .checked_sub(kept.len())
            .ok_or_else(|| format!("seed {seed}: more networks kept than listed"))?;

        let mut first: Option<(String, Config, String)> = None;
        for round in 0..4 {
            shuffle(&mut rng, &mut entries);
            shuffle(&mut rng, &mut ports);
            let spelled: Vec<String> = entries.iter().map(|e| spell(&mut rng, *e)).collect();
            let text = config_text(&ports, &spelled);
            let config = load(&dir, &text)?
                .map_err(|e| format!("seed {seed} round {round}: {text:?} was refused: {e}"))?;
            let sets = Sets::build(&dynamic, &floor, &config);
            let program = sets.program(&config.p2p_ports);
            let inputs = format!("{text:?} with memory {dynamic:?} and floor {floor:?}");

            let got_allow: BTreeSet<IpAddr> = sets
                .allow_v4
                .iter()
                .map(|v4| IpAddr::V4(*v4))
                .chain(sets.allow_v6.iter().map(|v6| IpAddr::V6(*v6)))
                .collect();
            assert_eq!(
                got_allow, allow,
                "seed {seed} round {round}: allow sets of {inputs}"
            );
            assert!(
                sets.static_v4.iter().all(|c| c.net.is_ipv4())
                    && sets.static_v6.iter().all(|c| c.net.is_ipv6()),
                "seed {seed} round {round}: a static network in the wrong family set for {inputs}: {sets:?}"
            );
            let got_static: BTreeSet<Cidr> =
                sets.static_v4.union(&sets.static_v6).copied().collect();
            assert_eq!(
                got_static, kept,
                "seed {seed} round {round}: static sets of {inputs}"
            );
            assert_eq!(
                config.covered_dropped, dropped_count,
                "seed {seed} round {round}: covered_dropped of {inputs}"
            );
            assert_eq!(
                config.catch_all,
                networks.iter().any(|c| c.prefix == 0),
                "seed {seed} round {round}: catch_all of {inputs}"
            );
            assert_program_shape(seed, &program, &inputs);

            match &first {
                None => first = Some((text, config, program)),
                Some((text0, config0, program0)) => {
                    assert_eq!(
                        &config, config0,
                        "seed {seed} round {round}: {text:?} loads unlike {text0:?}"
                    );
                    assert_eq!(
                        &program, program0,
                        "seed {seed} round {round}: {text:?} renders unlike {text0:?}"
                    );
                }
            }
        }
    }
    Ok(())
}

// ---- 4. IPv4-mapped entries ----

#[test]
fn mapped_networks_of_96_bits_or_more_are_the_ipv4_network() -> TestResult {
    let dir = TempDir::new("properties-mapped")?;
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        let v4 = any_v4(&mut rng);
        let v4_prefix = below_u8(&mut rng, 33);
        let prefix = v4_prefix.saturating_add(MAPPED_PREFIX);
        let entry = format!("{}/{prefix}", spell_mapped(&mut rng, v4));
        let expected = Cidr::new(IpAddr::V4(v4), v4_prefix)
            .ok_or_else(|| format!("seed {seed}: Cidr::new({v4}, {v4_prefix}) refused"))?;

        // the public functions the config applies, then the config itself
        let parsed = parse_cidr_or_host(&entry).map_err(|e| format!("seed {seed}: {e}"))?;
        assert_eq!(
            parsed.unmapped(),
            expected,
            "seed {seed}: {entry:?} parsed as {parsed}"
        );
        let text = config_text(&[49590], std::slice::from_ref(&entry));
        let loaded = load(&dir, &text)?;
        // a /128 is a host, and a host that is never a peer's source is refused
        if expected.is_host() && canonical_ip(IpAddr::V4(v4)).is_none() {
            assert_refused(seed, &text, &loaded, &entry);
            continue;
        }
        let config = loaded.map_err(|e| format!("seed {seed}: {text:?} was refused: {e}"))?;
        assert_eq!(
            config.static_v4,
            BTreeSet::from([expected]),
            "seed {seed}: {text:?} -> {config:?}"
        );
        assert!(
            config.static_v6.is_empty(),
            "seed {seed}: {text:?} left an IPv6 entry: {config:?}"
        );
        assert_eq!(
            config.catch_all,
            v4_prefix == 0,
            "seed {seed}: catch_all of {text:?}"
        );

        let sets = Sets::build(&BTreeSet::new(), &BTreeSet::new(), &config);
        let (allow_v4, static_v4) = if expected.is_host() {
            (BTreeSet::from([v4]), BTreeSet::new())
        } else {
            (BTreeSet::new(), BTreeSet::from([expected]))
        };
        assert_eq!(sets.allow_v4, allow_v4, "seed {seed}: {text:?} -> {sets:?}");
        assert_eq!(
            sets.static_v4, static_v4,
            "seed {seed}: {text:?} -> {sets:?}"
        );
        assert!(
            sets.allow_v6.is_empty() && sets.static_v6.is_empty(),
            "seed {seed}: {text:?} left an IPv6 element: {sets:?}"
        );
    }
    Ok(())
}

#[test]
fn mapped_networks_shorter_than_96_bits_are_refused_by_name() -> TestResult {
    let dir = TempDir::new("properties-mapped-short")?;
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        let v4 = any_v4(&mut rng);
        let prefix = below_u8(&mut rng, MAPPED_PREFIX);
        let entry = format!("{}/{prefix}", spell_mapped(&mut rng, v4));
        assert_eq!(
            parse_cidr_or_host(&entry),
            Err(AddrError::MappedPrefixTooShort {
                entry: entry.clone()
            }),
            "seed {seed}: {entry:?}"
        );
        // a valid entry next to it does not save the file
        let mut entries = vec![entry.clone(), "192.0.2.10".to_owned()];
        shuffle(&mut rng, &mut entries);
        let text = config_text(&[49590], &entries);
        assert_refused(seed, &text, &load(&dir, &text)?, &entry);
    }
    Ok(())
}

#[test]
fn mapped_hosts_go_to_the_ipv4_allow_set() -> TestResult {
    let dir = TempDir::new("properties-mapped-host")?;
    for seed in 1..=CASES {
        let mut rng = Rng::new(seed);
        let v4 = any_v4(&mut rng);
        let entry = spell_mapped(&mut rng, v4);
        let text = config_text(&[49590], std::slice::from_ref(&entry));
        let loaded = load(&dir, &text)?;
        if canonical_ip(IpAddr::V4(v4)).is_none() {
            assert_refused(seed, &text, &loaded, &entry);
            continue;
        }
        let config = loaded.map_err(|e| format!("seed {seed}: {text:?} was refused: {e}"))?;
        let host = Cidr::new(IpAddr::V4(v4), 32)
            .ok_or_else(|| format!("seed {seed}: Cidr::new({v4}, 32) refused"))?;
        assert_eq!(
            config.static_v4,
            BTreeSet::from([host]),
            "seed {seed}: {text:?} -> {config:?}"
        );
        assert!(
            config.static_v6.is_empty(),
            "seed {seed}: {text:?} left an IPv6 entry: {config:?}"
        );
        let sets = Sets::build(&BTreeSet::new(), &BTreeSet::new(), &config);
        assert_eq!(
            sets.allow_v4,
            BTreeSet::from([v4]),
            "seed {seed}: {text:?} -> {sets:?}"
        );
        assert!(
            sets.allow_v6.is_empty() && sets.static_v4.is_empty() && sets.static_v6.is_empty(),
            "seed {seed}: {text:?} -> {sets:?}"
        );
    }
    Ok(())
}
