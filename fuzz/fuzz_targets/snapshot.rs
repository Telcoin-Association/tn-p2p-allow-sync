//! `snapshot::parse` on any JSON, then `memory::apply`, `serialize` and `load_state` on what it accepts.
//!
//! Each input is used twice: as JSON when it parses as JSON, and as the choices of a snapshot-shaped value built here,
//! so the fold into memory is reached without a corpus of valid snapshots.
//!
//! Oracles, for an accepted snapshot:
//! - it has 1 to 16 swarms, an authoritative primary whose `tracked_epoch` is the snapshot's epoch, and canonical IPs only;
//! - folded into an empty `memory::State` it is not refused, removes and expires nothing, takes the snapshot's epoch,
//!   and remembers exactly the snapshot's IPs;
//! - the canonical `state.json` of the result loads back to the same state, and `serialize(load_state(serialize(state)))` is byte-equal;
//! - folding the same snapshot a second time changes nothing and adds nothing.

#![no_main]

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use libfuzzer_sys::arbitrary::{self, Unstructured};
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value, json};
use tn_p2p_allow_sync::addr::canonical_ip;
use tn_p2p_allow_sync::memory::{self, State};
use tn_p2p_allow_sync::snapshot::{self, MAX_SWARMS, PRIMARY_SWARM};

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = serde_json::from_slice::<Value>(data) {
        check(value);
    }
    if let Ok(value) = build(&mut Unstructured::new(data)) {
        check(value);
    }
});

fn check(value: Value) {
    let Ok(snap) = snapshot::parse(value) else {
        return;
    };
    assert!(!snap.swarms.is_empty() && snap.swarms.len() <= MAX_SWARMS);
    let primary = snap.swarms.get(PRIMARY_SWARM);
    assert!(
        primary.is_some_and(|p| p.authoritative && p.tracked_epoch == Some(snap.epoch)),
        "accepted without an authoritative primary at epoch {}: {primary:?}",
        snap.epoch
    );
    let listed: BTreeSet<IpAddr> = snap
        .swarms
        .values()
        .flat_map(|swarm| swarm.entries.values())
        .flat_map(|entry| entry.ips.iter().copied())
        .collect();
    for ip in &listed {
        assert_eq!(
            canonical_ip(*ip),
            Some(*ip),
            "the snapshot holds {ip}, which is not canonical"
        );
    }

    let mut state = State::default();
    let outcome = memory::apply(&mut state, &snap)
        .unwrap_or_else(|e| panic!("the empty state refuses a parsed snapshot: {e}"));
    assert_eq!((outcome.removed, outcome.expired), (0, 0), "{outcome:?}");
    assert_eq!(outcome.keys, state.keys.len());
    assert_eq!(state.epoch, snap.epoch);
    assert_eq!(
        memory::allow_ips(&state),
        listed,
        "memory holds other IPs than the snapshot"
    );

    let bytes = memory::serialize(&state);
    let loaded = memory::load_state(Some(&bytes)).unwrap_or_else(|e| {
        panic!(
            "state.json written from a reachable state is refused: {e}\n{}",
            String::from_utf8_lossy(&bytes)
        )
    });
    assert_eq!(loaded, state, "state.json loads to another state");
    assert_eq!(
        memory::serialize(&loaded),
        bytes,
        "state.json is not a fixpoint"
    );

    let mut again = state.clone();
    let second = memory::apply(&mut again, &snap)
        .unwrap_or_else(|e| panic!("a snapshot is refused at its own epoch: {e}"));
    assert_eq!(
        again, state,
        "folding the same snapshot twice changed memory"
    );
    assert_eq!(second.added, 0, "{second:?}");
}

/// A `tn_admissionSnapshot` result whose every choice comes from `u`: mostly valid, with the faults the parser absorbs or refuses mixed in.
fn build(u: &mut Unstructured<'_>) -> arbitrary::Result<Value> {
    let count = u.int_in_range(1..=4)?;
    let mut swarms = Vec::new();
    for index in 0..count {
        let network = if index == 0 {
            PRIMARY_SWARM
        } else {
            *u.choose(&[
                "worker-0",
                "worker-1",
                "worker-2",
                "worker-15",
                PRIMARY_SWARM,
            ])?
        };
        swarms.push(swarm(u, network)?);
    }
    let mut result = Map::new();
    result.insert("version".to_owned(), json!(1));
    if u.arbitrary()? {
        result.insert("epoch".to_owned(), json!(epoch(u)?));
    }
    result.insert("swarms".to_owned(), Value::Array(swarms));
    Ok(Value::Object(result))
}

fn swarm(u: &mut Unstructured<'_>, network: &str) -> arbitrary::Result<Value> {
    let available = u.ratio(7, 8)?;
    let admission = if available || u.ratio(1, 8)? {
        json!({
            "configured": u.choose(&["Open", "Grace", "Closed"])?,
            "effective": u.choose(&["Open", "Grace", "Closed"])?,
            "epoch": epoch(u)?,
            "fallback": u.choose(&[None, None, Some("Unresolved"), Some("Missing"), Some("Stale"), Some("Contradictory"), Some("Other")])?,
            "resolved_current": u.int_in_range(0..=5_u64)?,
            "required_current": u.int_in_range(0..=5_u64)?,
            "connected_current": u.int_in_range(0..=5_u64)?,
            "contradicted": u.int_in_range(0..=1_u64)?,
        })
    } else {
        Value::Null
    };
    let mut entries = Vec::new();
    for _ in 0..u.int_in_range(0..=6)? {
        entries.push(entry(u)?);
    }
    Ok(json!({
        "network": network,
        "available": available,
        "tracked_epoch": epoch(u)?,
        "admission": admission,
        "entries": entries,
    }))
}

/// An epoch near the others, or null now and then.
fn epoch(u: &mut Unstructured<'_>) -> arbitrary::Result<Option<u64>> {
    Ok(if u.ratio(1, 8)? {
        None
    } else {
        Some(u.int_in_range(40..=44)?)
    })
}

fn entry(u: &mut Unstructured<'_>) -> arbitrary::Result<Value> {
    // four keys, so that one swarm lists a key twice and two swarms share one
    let byte: u8 = *u.choose(&[0x0a, 0x1b, 0x2c, 0x3d])?;
    let mut key = format!("0x{}", format!("{byte:02x}").repeat(96));
    if u.ratio(1, 8)? {
        key = key.to_uppercase().replacen("0X", "0x", 1);
    }
    if u.ratio(1, 32)? {
        key.truncate(u.int_in_range(0..=key.len())?);
    }
    let mut slots = Vec::new();
    for _ in 0..u.int_in_range(0..=3)? {
        slots.push(*u.choose(&[
            "previous",
            "current",
            "next",
            "scheduled",
            "eligible",
            "trusted",
            "bootstrap",
            "bogus",
        ])?);
    }
    let mut fields = Map::new();
    fields.insert("bls_pubkey".to_owned(), json!(key));
    fields.insert("slots".to_owned(), json!(slots));
    fields.insert("multiaddrs".to_owned(), json!(multiaddrs(u)?));
    if u.arbitrary()? {
        fields.insert("record_multiaddrs".to_owned(), json!(multiaddrs(u)?));
    }
    fields.insert(
        "source".to_owned(),
        json!(u.choose(&["record", "config", "stub", "unresolved"])?),
    );
    fields.insert("admitted".to_owned(), json!(u.arbitrary::<bool>()?));
    Ok(Value::Object(fields))
}

fn multiaddrs(u: &mut Unstructured<'_>) -> arbitrary::Result<Vec<String>> {
    // now and then one past the 32 that are read
    let count = if u.ratio(1, 32)? {
        33
    } else {
        u.int_in_range(0..=3)?
    };
    let mut list = Vec::new();
    for _ in 0..count {
        list.push(multiaddr(u)?);
    }
    Ok(list)
}

fn multiaddr(u: &mut Unstructured<'_>) -> arbitrary::Result<String> {
    Ok(match u.int_in_range(0..=5)? {
        0 | 1 => format!(
            "/ip4/{}/udp/49590/quic-v1",
            Ipv4Addr::from(u.arbitrary::<u32>()?)
        ),
        2 => format!(
            "/ip6/{}/udp/49590/quic-v1",
            Ipv6Addr::from(u.arbitrary::<u128>()?)
        ),
        // IPv4-mapped, so the snapshot holds the IPv4 form
        3 => format!(
            "/ip6/{}/udp/49590/quic-v1",
            Ipv4Addr::from(u.arbitrary::<u32>()?).to_ipv6_mapped()
        ),
        4 => "/dns4/node.example/udp/49590/quic-v1".to_owned(),
        _ => u.arbitrary::<String>()?,
    })
}
