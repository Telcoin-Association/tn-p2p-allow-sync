//! `nft::parse_listing` on any bytes: the reader of `nft -j list table inet tn_p2p_allow`, whose output the idempotence check trusts.
//!
//! Oracles:
//! - every refusal is `NftError::Parse`, never another variant;
//! - an accepted listing holds only the program's four sets, each set's elements sorted, every prefix within its family's length
//!   and every range within one family;
//! - the same bytes parse to the same listing.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tn_p2p_allow_sync::nft::{Element, NftError, parse_listing};

/// The sets of the program (schema 1); any other set of the table is not read.
const SETS: [&str; 4] = ["allow_v4", "allow_v6", "static_v4", "static_v6"];

fuzz_target!(|data: &[u8]| {
    let listing = match parse_listing(data) {
        Ok(listing) => listing,
        Err(NftError::Parse(_)) => return,
        Err(other) => panic!("parse_listing refused with {other:?}, not NftError::Parse"),
    };
    for (name, elements) in &listing.sets {
        assert!(
            SETS.contains(&name.as_str()),
            "the listing holds set {name:?}"
        );
        assert!(
            elements.is_sorted(),
            "set {name} is not sorted: {elements:?}"
        );
        for element in elements {
            match *element {
                Element::Ip(_) => {}
                Element::Cidr { net, prefix } => {
                    let full = if net.is_ipv4() { 32 } else { 128 };
                    assert!(prefix <= full, "set {name} holds {net}/{prefix}");
                }
                Element::Range(first, last) => assert_eq!(
                    first.is_ipv4(),
                    last.is_ipv4(),
                    "set {name} holds a range from {first} to {last}"
                ),
            }
        }
    }
    assert_eq!(
        parse_listing(data).ok(),
        Some(listing),
        "a second parse differs"
    );
});
