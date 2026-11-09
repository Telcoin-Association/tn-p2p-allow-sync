//! `addr::parse_cidr_or_host` on any text: the parser of every `P2P_ALLOW_FROM` entry.
//!
//! Oracles, for an accepted entry:
//! - the prefix fits the family and the address is masked to it;
//! - an entry without `/` is a host, the address as written;
//! - an address written IPv4-mapped has a prefix of at least 96 (a shorter one is refused);
//! - its `Display` parses back to an equal value;
//! - `Cidr::unmapped` gives the IPv4 network of a mapped network, prefix minus 96, and leaves any other as it is.

#![no_main]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use libfuzzer_sys::fuzz_target;
use tn_p2p_allow_sync::addr::{Cidr, parse_cidr_or_host};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(cidr) = parse_cidr_or_host(text) else {
        return;
    };

    let full = if cidr.net.is_ipv4() { 32 } else { 128 };
    assert!(cidr.prefix <= full, "{text:?} gave prefix {}", cidr.prefix);
    assert_eq!(
        Cidr::new(cidr.net, cidr.prefix),
        Some(cidr),
        "{text:?} gave an unmasked {cidr}"
    );

    let (written, prefix) = match text.split_once('/') {
        Some((written, prefix)) => (written, Some(prefix)),
        None => (text, None),
    };
    if prefix.is_none() {
        assert!(cidr.is_host(), "{text:?} has no prefix and gave {cidr}");
        assert_eq!(written.parse::<IpAddr>().ok(), Some(cidr.net));
    }
    if written.parse::<Ipv6Addr>().is_ok_and(is_mapped) {
        assert!(
            cidr.prefix >= 96,
            "{text:?} is IPv4-mapped with a prefix below 96 and was accepted as {cidr}"
        );
    }

    let shown = cidr.to_string();
    assert_eq!(
        parse_cidr_or_host(&shown),
        Ok(cidr),
        "{text:?} gave {cidr}, whose text does not parse back to it"
    );

    let unmapped = cidr.unmapped();
    match cidr.net {
        IpAddr::V6(v6) if is_mapped(v6) => {
            // the network is masked at 96 bits or more, so its low 32 bits are the IPv4 network
            let v4 = Ipv4Addr::from((u128::from(v6) & 0xffff_ffff) as u32);
            assert_eq!(
                unmapped.net,
                IpAddr::V4(v4),
                "{cidr} unmapped to {unmapped}"
            );
            assert_eq!(
                Some(unmapped.prefix),
                cidr.prefix.checked_sub(96),
                "{cidr} unmapped to {unmapped}"
            );
        }
        _ => assert_eq!(
            unmapped, cidr,
            "{cidr} is not mapped but unmapped to {unmapped}"
        ),
    }
});

/// Whether `v6` is IPv4-mapped, `::ffff:a.b.c.d`, from the bits rather than std's helper.
fn is_mapped(v6: Ipv6Addr) -> bool {
    u128::from(v6) >> 32 == 0xffff
}
