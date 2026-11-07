//! `config::load_config` and `config::load_floor` on any bytes: the two files of the state dir the scripts and `capture-floor` write.
//!
//! The parsers are private and read a path, so each input is written to one file per process in the temp dir and both loaders read it.
//! Each input is read three times: as it is, as the value of `P2P_ALLOW_FROM` in an otherwise valid config, and as the lines of a floor file
//! after its header, so both parsers' accepting paths are reached without a corpus.
//!
//! Oracles:
//! - loading the same bytes twice gives the same result, accepted or refused;
//! - an accepted config has an RPC port and at least one P2P port, none of them 0; its static sets hold one family each, masked,
//!   hosts in their canonical form, no IPv4-mapped IPv6 left, and no network inside another; `catch_all` is a prefix-0 entry;
//! - the config this target writes back from an accepted one (every port and every entry, as `Display` prints it) loads to the same settings,
//!   with nothing left to drop as covered;
//! - an accepted floor holds canonical IPs only, and `render_floor` of it is the input, given a final newline.

#![no_main]

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use tn_p2p_allow_sync::addr::{Cidr, canonical_ip};
use tn_p2p_allow_sync::cli::CONFIG_VERSION;
use tn_p2p_allow_sync::config::{Config, FLOOR_HEADER, load_config, load_floor, render_floor};

/// The config lines put in front of the input for the second reading: every key but the last set.
const CONFIG_HEAD: &[u8] =
    b"CONFIG_VERSION=1\nRPC_PORT=8545\nP2P_PORTS=49590,49594\nP2P_ALLOW_FROM=";

fuzz_target!(|data: &[u8]| {
    check(data);
    check(&[CONFIG_HEAD, data].concat());
    check(&[FLOOR_HEADER.as_bytes(), b"\n", data].concat());
});

fn check(data: &[u8]) {
    let path = input_path();
    write(path, data);

    let config = load_config(path);
    assert_eq!(
        load_config(path),
        config,
        "a second load of the same bytes differs"
    );
    let floor = load_floor(path);
    assert_eq!(
        load_floor(path),
        floor,
        "a second load of the same bytes differs"
    );

    if let Ok(floor) = floor {
        check_floor(data, &floor);
    }
    if let Ok(config) = config {
        check_config(&config);
        let written = write_config(&config);
        write(path, written.as_bytes());
        let again = load_config(path)
            .unwrap_or_else(|e| panic!("the config written back is refused: {e}\n{written}"));
        assert_eq!(
            again,
            Config {
                covered_dropped: 0,
                ..config
            },
            "the config written back loads differently:\n{written}"
        );
    }
}

/// The file this process feeds the loaders: its own, so parallel fuzzing jobs do not share one.
fn input_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::temp_dir().join(format!("tn-p2p-allow-fuzz-config.{}", std::process::id()))
    })
}

fn write(path: &PathBuf, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("cannot write {path:?}: {e}"));
}

fn check_config(config: &Config) {
    assert_ne!(config.rpc_port, 0);
    assert!(!config.p2p_ports.is_empty() && !config.p2p_ports.contains(&0));
    for (set, v4) in [(&config.static_v4, true), (&config.static_v6, false)] {
        for cidr in set {
            assert_eq!(
                cidr.net.is_ipv4(),
                v4,
                "{cidr} is in the other family's set"
            );
            assert_eq!(
                Cidr::new(cidr.net, cidr.prefix),
                Some(*cidr),
                "{cidr} is not masked"
            );
            if let IpAddr::V6(v6) = cidr.net {
                assert_ne!(u128::from(v6) >> 32, 0xffff, "{cidr} is still IPv4-mapped");
            }
            if cidr.is_host() {
                assert_eq!(
                    canonical_ip(cidr.net),
                    Some(cidr.net),
                    "host {cidr} is not canonical"
                );
            }
        }
        let networks = set.iter().filter(|cidr| !cidr.is_host());
        for wide in networks.clone() {
            for narrow in networks.clone().filter(|narrow| *narrow != wide) {
                assert!(
                    !covers(*wide, *narrow),
                    "{wide} covers {narrow} and both were kept"
                );
            }
        }
    }
    let catch_all = config
        .static_v4
        .iter()
        .chain(&config.static_v6)
        .any(|cidr| cidr.prefix == 0);
    assert_eq!(config.catch_all, catch_all);
}

/// Whether the network `wide` holds every address of `narrow`.
fn covers(wide: Cidr, narrow: Cidr) -> bool {
    wide.prefix <= narrow.prefix && Cidr::new(narrow.net, wide.prefix) == Some(wide)
}

/// A config file of version 1 for `config`; the format has no writer of its own, the scripts write it.
fn write_config(config: &Config) -> String {
    let ports: Vec<String> = config.p2p_ports.iter().map(u16::to_string).collect();
    let entries: Vec<String> = config
        .static_v4
        .iter()
        .chain(&config.static_v6)
        .map(Cidr::to_string)
        .collect();
    format!(
        "CONFIG_VERSION={CONFIG_VERSION}\nRPC_PORT={}\nP2P_PORTS={}\nP2P_ALLOW_FROM={}\n",
        config.rpc_port,
        ports.join(","),
        entries.join(",")
    )
}

fn check_floor(data: &[u8], floor: &BTreeSet<IpAddr>) {
    for ip in floor {
        assert_eq!(
            canonical_ip(*ip),
            Some(*ip),
            "floor IP {ip} is not canonical"
        );
    }
    let mut expected = data.to_vec();
    if !expected.ends_with(b"\n") {
        expected.push(b'\n');
    }
    assert_eq!(
        render_floor(floor).as_bytes(),
        expected,
        "the floor file does not render back to itself"
    );
}
