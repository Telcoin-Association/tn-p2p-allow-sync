//! `addr::classify_multiaddr` and `addr::canonical_ip` on any text: the parser every snapshot multiaddr and every `capture-floor` token goes through.
//!
//! Oracles, for a multiaddr that yields an IP:
//! - the IP prints and parses back to itself, and the multiaddr spelled from it classifies the same;
//! - `canonical_ip` keeps it exactly when, after the IPv4-mapped form becomes IPv4, it is not unspecified, multicast or link-local;
//! - a mapped IPv6 address canonicalises to its IPv4 address, anything else to itself;
//! - the canonical form is its own canonical form and prints and parses back to itself.

#![no_main]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use libfuzzer_sys::fuzz_target;
use tn_p2p_allow_sync::addr::{AddrClass, canonical_ip, classify_multiaddr};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let AddrClass::Ip(ip) = classify_multiaddr(text) else {
        return;
    };

    assert_eq!(
        ip.to_string().parse::<IpAddr>(),
        Ok(ip),
        "{ip} does not parse back"
    );
    let family = if ip.is_ipv4() { "ip4" } else { "ip6" };
    let spelled = format!("/{family}/{ip}");
    assert_eq!(
        classify_multiaddr(&spelled),
        AddrClass::Ip(ip),
        "{spelled} does not classify as {ip}"
    );

    // the kernel's view, computed here from the bits: `::ffff:a.b.c.d` is a.b.c.d
    let seen = match ip {
        IpAddr::V6(v6) => mapped(v6).map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    };
    let canonical = canonical_ip(ip);
    assert_eq!(
        canonical.is_some(),
        !never_a_source(seen),
        "canonical_ip({ip}) is {canonical:?}"
    );
    let Some(canonical) = canonical else {
        return;
    };
    assert_eq!(canonical, seen, "canonical_ip({ip}) is {canonical}");
    if let IpAddr::V6(v6) = canonical {
        assert!(mapped(v6).is_none(), "canonical_ip({ip}) is mapped: {v6}");
    }
    assert_eq!(
        canonical_ip(canonical),
        Some(canonical),
        "{canonical} is not its own canonical form"
    );
    assert_eq!(canonical.to_string().parse::<IpAddr>(), Ok(canonical));
});

/// The IPv4 address of an IPv4-mapped IPv6 address `::ffff:a.b.c.d`, from the bits rather than std's helper.
fn mapped(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let bits = u128::from(v6);
    (bits >> 32 == 0xffff).then(|| Ipv4Addr::from((bits & 0xffff_ffff) as u32))
}

/// Unspecified, multicast or link-local (169.254.0.0/16, fe80::/10), from the bits.
fn never_a_source(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            bits == 0 || bits >> 28 == 0b1110 || bits >> 16 == 0xa9fe
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            bits == 0 || bits >> 120 == 0xff || bits >> 118 == 0b11_1111_1010
        }
    }
}
