//! Real-kernel test of the nft program: the rendered program loaded into the running kernel by the system's nft, datagrams sent through a veth pair, and the named counters read back through [`Nft::list_table`].
//!
//! It needs root, `/usr/sbin/nft` and `ip`, so it is ignored by default; CI runs it under `sudo` on each Ubuntu release (`.github/workflows/nft-real.yaml`).
//! Without root or either tool it prints why and passes, unless `TN_REAL_NFT_REQUIRED=1` makes that a failure, as CI does.
//!
//! The table is loaded in the test's own network namespace, which on a CI runner is the host's.
//! The senders live in a namespace of their own at the far end of a veth pair, so every datagram crosses the input hook; a sender is this test binary run again inside that namespace with `TN_REAL_NFT_SEND` set.
//! The test refuses to run when table `inet tn_p2p_allow` already exists, and deletes the table and the namespace when it ends, pass or fail.
//! Raw `nft -j` listings and `summary.txt` land in `target/real-nft/`, which CI uploads.
//!
//! The steps, in order: traffic from a listed and an unlisted IPv4 source; the same program and then a larger one re-applied; the smaller one again, so an element departs; a table made dormant by hand and woken by a re-apply; delete and reload; traffic through the IPv6 and static rules; adjacent static networks; `nft -c` of every golden program and of a syntax error; networks at the top of the address space; the built binary's own `sync` twice, against a [`FakeRpc`] on loopback; and timed loads of 20000 and of 65000 elements.

mod support;

use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use tn_p2p_allow_sync::addr::{Cidr, parse_cidr_or_host};
use tn_p2p_allow_sync::nft::{self, Counter, Element, Listing, Nft, NftError};
use tn_p2p_allow_sync::sync::{
    COUNTERS, CURRENT_FILE, EPOCH_INFO_METHOD, INPUT_RULES, SNAPSHOT_METHOD, Sets, listing_matches,
};

use support::TempDir;
use support::fake_rpc::{self, FakeRpc};

type TestResult = Result<(), Box<dyn Error>>;
type Res<T> = Result<T, Box<dyn Error>>;

/// This test's name; a sender runs it again under this name.
const TEST_NAME: &str = "real_kernel_nft";

/// Set only in a sender's environment: `SRC DST:PORT COUNT PAYLOAD`.
const SEND_VAR: &str = "TN_REAL_NFT_SEND";

/// `1` turns a skip into a failure, so a runner without root or nft cannot pass the job vacuously.
const REQUIRED_VAR: &str = "TN_REAL_NFT_REQUIRED";

/// Where `ip` may live; Ubuntu ships it in more than one of these.
const IP_PATHS: [&str; 4] = ["/usr/sbin/ip", "/sbin/ip", "/usr/bin/ip", "/bin/ip"];

/// The whole environment of every program the test starts, apart from a sender's job.
const CHILD_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// The node's end of the veth pair, in the test's own namespace.
const HOST: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
/// A sender listed in `allow_v4` from the start.
const LISTED: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
/// A sender in no set.
const UNLISTED: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 3);
/// A sender the changed program adds to `allow_v4`.
const ADDED: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 4);
/// A sender inside the base program's `static_v4` network `198.51.100.0/24`.
const STATIC4: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 7);

/// The node's IPv6 address on its end of the pair.
const HOST6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
/// A sender the base program lists in `allow_v6`.
const LISTED6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2);
/// An IPv6 sender in no set.
const UNLISTED6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 3);
/// A sender inside the base program's `static_v6` network `2001:db8:5::/48`.
const STATIC6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 5, 0, 0, 0, 0, 7);

/// Static networks of which the first two touch and the third lies past a gap; nft must list them as they are, not merged.
///
/// They come from the benchmarking range `198.18.0.0/15`, which no runner's own network uses, because the node's end gets a route to them.
const ADJACENT_V4: [&str; 3] = ["198.18.0.0/24", "198.18.1.0/24", "198.18.3.0/24"];
/// Two IPv6 networks that touch.
const ADJACENT_V6: [&str; 2] = ["2001:db8:a::/48", "2001:db8:b::/48"];
/// A sender in the second adjacent network.
const IN_ADJACENT: Ipv4Addr = Ipv4Addr::new(198, 18, 1, 5);
/// A sender in the gap between the adjacent networks and the third one.
const IN_GAP: Ipv4Addr = Ipv4Addr::new(198, 18, 2, 5);
/// Routes on the node's end to the senders outside its subnets, so even a strict reverse-path filter lets their datagrams reach the input hook.
const ROUTES: [&str; 3] = ["198.51.100.0/24", "198.18.0.0/22", "2001:db8:5::/48"];

/// The P2P ports of the program; every datagram counted goes to the first.
const PORTS: [u16; 2] = [49590, 49594];
const P2P_PORT: u16 = 49590;
/// A port outside the program, which the chain must leave alone.
const OTHER_PORT: u16 = 40000;

/// Payload of an accepted datagram; with 20 bytes of IPv4 header and 8 of UDP header it is 34 bytes to the counter.
const LISTED_PAYLOAD: &str = "listed";
/// Payload of a dropped datagram, 36 bytes to the counter.
///
/// Over IPv6 the header is 40 bytes, so the two payloads count 54 and 56 bytes.
const UNLISTED_PAYLOAD: &str = "unlisted";

/// Elements per family in the timed load.
const BIG: u32 = 10_000;
/// The longest a load of the big program may take.
const LOAD_LIMIT: Duration = Duration::from_secs(10);
/// IPv4 elements in the large timed load.
const LARGE: u32 = 65_000;
/// IPv6 elements in the large timed load.
const LARGE_V6: u32 = 8;
/// The longest a load of the large program may take; the sync unit's `TimeoutStartSec=90` bounds a whole run.
const LARGE_LIMIT: Duration = Duration::from_secs(30);
/// How long the counters get to reach the expected packet counts.
const COUNTER_WAIT: Duration = Duration::from_secs(10);

/// The `tn-p2p-allow-sync` binary of this test's build: built for the musl target under `make nft-real-docker`, for the host under `make nft-real`.
const BIN: &str = env!("CARGO_BIN_EXE_tn-p2p-allow-sync");
/// The snapshot the binary's `sync` gets from the fake node.
const SNAPSHOT: &str = include_str!("fixtures/snapshot/good.json");
/// The epoch info the binary's `sync` asks for after the snapshot.
const EPOCH_INFO: &str = r#"{"epochId":42,"epochDuration":86400}"#;

#[test]
#[ignore = "needs root, nft and ip; CI runs it under sudo (.github/workflows/nft-real.yaml)"]
fn real_kernel_nft() -> TestResult {
    if let Some(job) = env_var(SEND_VAR) {
        return send_datagrams(&job);
    }
    let ip = match prerequisites()? {
        Ok(ip) => ip,
        Err(why) if env_var(REQUIRED_VAR).is_some_and(|value| value == "1") => {
            return Err(format!("{TEST_NAME} cannot run and {REQUIRED_VAR}=1: {why}").into());
        }
        Err(why) => {
            println!("{TEST_NAME} skipped: {why}");
            return Ok(());
        }
    };
    let nft = Nft::new(None);
    if nft.table_present()? {
        return Err(
            "table inet tn_p2p_allow already exists; this test loads and deletes it, so it refuses to run where the table is in use"
                .into(),
        );
    }
    let pid = std::process::id();
    let bed = Bed {
        ip,
        nft,
        ns: format!("tn-real-nft-{pid}"),
        host_if: format!("tnr{pid}h"),
        peer_if: format!("tnr{pid}p"),
        temp: TempDir::new("real-nft")?,
    };
    bed.setup()?;
    let mut report = Report::create(&out_dir())?;
    environment(&bed, &mut report)?;

    let base = base_sets()?;
    let changed = changed_sets()?;
    let p2p = UdpSocket::bind((HOST, P2P_PORT))?;
    let other = UdpSocket::bind((HOST, OTHER_PORT))?;
    let p2p6 = UdpSocket::bind((HOST6, P2P_PORT))?;
    let other6 = UdpSocket::bind((HOST6, OTHER_PORT))?;
    let counted = traffic(&bed, &mut report, &base, &p2p, &other)?;
    reapply(&bed, &mut report, &base, &changed, counted, &p2p)?;
    shrink(&bed, &mut report, &base, &p2p)?;
    dormant(&bed, &mut report, &base, &p2p)?;
    reload(&bed, &mut report, &changed)?;
    ipv6_and_static(&bed, &mut report, &base, [&p2p, &p2p6], &other6)?;
    adjacent(&bed, &mut report, &p2p)?;
    syntax(&bed, &mut report, &changed)?;
    top_of_space(&bed, &mut report)?;
    binary_sync(&bed, &mut report)?;
    big_load(&bed, &mut report)?;
    large_load(&bed, &mut report)?;

    bed.nft.delete_table()?;
    assert!(!bed.nft.table_present()?, "the table survived its deletion");
    report.line("done")
}

/// Loads the base program, sends from the listed and the unlisted source, and checks what was counted and what was delivered.
fn traffic(
    bed: &Bed,
    report: &mut Report,
    base: &Sets,
    p2p: &UdpSocket,
    other: &UdpSocket,
) -> Res<Counts> {
    bed.load("base", base)?;
    let listing = bed.listing()?;
    assert!(listing_matches(&listing, base, &ports()), "{listing:?}");
    assert_eq!(
        counts(&listing)?,
        Counts::default(),
        "a new table counts from zero"
    );

    warm_up(bed, LISTED.into(), other)?;
    bed.send(LISTED, P2P_PORT, 3, LISTED_PAYLOAD)?;
    bed.send(UNLISTED, P2P_PORT, 2, UNLISTED_PAYLOAD)?;
    bed.send(UNLISTED, OTHER_PORT, 1, UNLISTED_PAYLOAD)?;
    let counted = bed.wait_for(3, 2)?;
    report.line(&format!("traffic: {counted}"))?;
    bed.save_json("traffic")?;
    assert_eq!(
        counted,
        Counts {
            accept: Counter {
                packets: 3,
                bytes: 102
            },
            drop: Counter {
                packets: 2,
                bytes: 72
            },
        }
    );
    let delivered = drain(p2p)?;
    assert_eq!(
        delivered,
        vec![(IpAddr::V4(LISTED), LISTED_PAYLOAD.to_owned()); 3]
    );
    // the chain leaves other ports alone, whoever sends
    let elsewhere = drain(other)?;
    let unlisted = (IpAddr::V4(UNLISTED), UNLISTED_PAYLOAD.to_owned());
    assert_eq!(
        elsewhere.iter().filter(|got| **got == unlisted).count(),
        1,
        "{elsewhere:?}"
    );
    Ok(counted)
}

/// Re-applies the same program and then one with an extra element, and checks that both counters kept their totals and still count.
///
/// The host-metrics collector sums the two counters by name, so a reset on every sync would restart its totals; a reset fails the test rather than the program switching to another form.
fn reapply(
    bed: &Bed,
    report: &mut Report,
    base: &Sets,
    changed: &Sets,
    counted: Counts,
    p2p: &UdpSocket,
) -> TestResult {
    bed.load("base", base)?;
    let listing = bed.listing()?;
    let same = counts(&listing)?;
    report.line(&format!(
        "re-applied the same program: before {counted}; after {same}"
    ))?;
    bed.save_json("reapplied-same")?;
    assert!(listing_matches(&listing, base, &ports()), "{listing:?}");
    assert_eq!(
        same, counted,
        "re-applying the same program reset the counters"
    );

    bed.load("changed", changed)?;
    let listing = bed.listing()?;
    let after = counts(&listing)?;
    report.line(&format!(
        "applied a changed program (one more allow_v4 element): before {counted}; after {after}"
    ))?;
    bed.save_json("reapplied-changed")?;
    assert!(listing_matches(&listing, changed, &ports()), "{listing:?}");
    assert_eq!(
        after, counted,
        "applying a changed program reset the counters"
    );

    // the new element admits its source, and the rules count into the counters that survived
    bed.send(ADDED, P2P_PORT, 1, LISTED_PAYLOAD)?;
    bed.send(UNLISTED, P2P_PORT, 1, UNLISTED_PAYLOAD)?;
    let more = bed.wait_for(4, 3)?;
    report.line(&format!("traffic after the change: {more}"))?;
    assert_eq!(
        more,
        Counts {
            accept: Counter {
                packets: 4,
                bytes: 136
            },
            drop: Counter {
                packets: 3,
                bytes: 108
            },
        }
    );
    assert_eq!(
        drain(p2p)?,
        vec![(IpAddr::V4(ADDED), LISTED_PAYLOAD.to_owned())]
    );
    Ok(())
}

/// Re-applies the base program over the changed one, so [`ADDED`] departs as a validator leaving the sets would, and checks that the program's `flush set` lines removed it on a real kernel.
///
/// The listing must match the base sets without the departed element, and a datagram from it must land on the drop counter and never reach the node.
fn shrink(bed: &Bed, report: &mut Report, base: &Sets, p2p: &UdpSocket) -> TestResult {
    bed.load("base", base)?;
    let listing = bed.listing()?;
    let before = counts(&listing)?;
    bed.save_json("shrunk")?;
    assert!(
        listing_matches(&listing, base, &ports()),
        "shrink: the listing does not match the base sets: {listing:?}"
    );
    let departed = Element::Ip(IpAddr::V4(ADDED));
    assert!(
        !listing
            .sets
            .get("allow_v4")
            .is_some_and(|live| live.contains(&departed)),
        "shrink: {ADDED} is still in allow_v4: {listing:?}"
    );
    assert_eq!(
        before,
        Counts::of(4, 136, 3, 108),
        "shrink: re-applying the smaller program reset the counters"
    );

    bed.send(ADDED, P2P_PORT, 1, LISTED_PAYLOAD)?;
    let after = bed.wait_for(4, 4)?;
    report.line(&format!(
        "shrink: re-applied the base program, {ADDED} is gone from allow_v4; before {before}; after a datagram from it {after}"
    ))?;
    assert_eq!(
        after,
        Counts::of(4, 136, 4, 142),
        "shrink: the datagram from the departed {ADDED} did not land on the drop counter"
    );
    assert_eq!(
        drain(p2p)?,
        Vec::new(),
        "shrink: a datagram from the departed {ADDED} reached the node"
    );
    Ok(())
}

/// Makes the loaded table dormant by hand, so none of its chains sees a packet, and checks that the listing says so and that a re-apply of the same program, as the next sync does, wakes it.
fn dormant(bed: &Bed, report: &mut Report, base: &Sets, p2p: &UdpSocket) -> TestResult {
    let before = counts(&bed.listing()?)?;
    checked(
        command(Path::new(nft::DEFAULT_PATH)).args([
            "add",
            "table",
            "inet",
            "tn_p2p_allow",
            "{ flags dormant; }",
        ]),
        "dormant: nft add table inet tn_p2p_allow { flags dormant; }",
    )?;
    let listing = bed.listing()?;
    bed.save_json("dormant")?;
    assert!(
        listing.dormant,
        "dormant: the listing of a dormant table says it is awake: {listing:?}"
    );
    assert!(
        !listing_matches(&listing, base, &ports()),
        "dormant: the idempotence compare matches a dormant table, so sync would leave it asleep"
    );

    // the chain sees nothing, so the unlisted datagram is neither counted nor dropped
    bed.send(UNLISTED, P2P_PORT, 1, UNLISTED_PAYLOAD)?;
    let delivered = drain(p2p)?;
    let asleep = counts(&bed.listing()?)?;
    assert_eq!(
        delivered,
        vec![(IpAddr::V4(UNLISTED), UNLISTED_PAYLOAD.to_owned())],
        "dormant: the unlisted datagram did not pass the dormant table"
    );
    assert_eq!(
        asleep, before,
        "dormant: a dormant table counted the unlisted datagram"
    );

    bed.load("base", base)?;
    let listing = bed.listing()?;
    bed.save_json("dormant-reapplied")?;
    assert!(
        !listing.dormant,
        "dormant: re-applying the program left the table dormant: {listing:?}"
    );
    assert!(
        listing_matches(&listing, base, &ports()),
        "dormant: the woken table does not match the base sets: {listing:?}"
    );
    assert_eq!(
        counts(&listing)?,
        before,
        "dormant: waking the table reset the counters"
    );
    bed.send(UNLISTED, P2P_PORT, 1, UNLISTED_PAYLOAD)?;
    let awake = bed.wait_for(4, 5)?;
    report.line(&format!(
        "dormant: with flags dormant the listing says dormant and an unlisted datagram passed uncounted ({asleep}); re-applied the program: awake, and the next one dropped ({awake})"
    ))?;
    assert_eq!(
        awake,
        Counts::of(4, 136, 5, 178),
        "dormant: the woken table did not drop and count the unlisted datagram"
    );
    assert_eq!(
        drain(p2p)?,
        Vec::new(),
        "dormant: the woken table let the unlisted datagram through"
    );
    Ok(())
}

/// Deletes the table and loads the program into nothing, as after a ruleset flush.
fn reload(bed: &Bed, report: &mut Report, changed: &Sets) -> TestResult {
    bed.nft.delete_table()?;
    assert!(!bed.nft.table_present()?, "the table survived its deletion");
    bed.load("changed", changed)?;
    assert!(
        bed.nft.table_present()?,
        "the program did not create the table"
    );
    let listing = bed.listing()?;
    let fresh = counts(&listing)?;
    report.line(&format!("deleted and reloaded: {fresh}"))?;
    bed.save_json("reloaded")?;
    assert!(listing_matches(&listing, changed, &ports()), "{listing:?}");
    assert_eq!(
        fresh,
        Counts::default(),
        "a recreated table counts from zero"
    );
    Ok(())
}

/// Loads the base program over the reloaded table and sends through the three accept rules the IPv4 traffic never reaches, `static_v4`, `allow_v6` and `static_v6`, and to the drop rule from an unlisted IPv6 source.
///
/// Each listed source goes alone and the counters are read after it, so each rule shows that it admits its own source and counts it once; `p2p` holds the node's IPv4 and IPv6 sockets on the first P2P port.
fn ipv6_and_static(
    bed: &Bed,
    report: &mut Report,
    base: &Sets,
    p2p: [&UdpSocket; 2],
    other6: &UdpSocket,
) -> TestResult {
    let [p2p4, p2p6] = p2p;
    bed.load("v6-static", base)?;
    let listing = bed.listing()?;
    assert!(
        listing_matches(&listing, base, &ports()),
        "v6-static: the listing does not match the base sets: {listing:?}"
    );
    assert_eq!(
        counts(&listing)?,
        Counts::default(),
        "v6-static: the reloaded table did not count from zero"
    );

    warm_up(bed, LISTED6.into(), other6)?;
    for (from, set, count, expected) in [
        (IpAddr::V6(LISTED6), "allow_v6", 2, Counts::of(2, 108, 0, 0)),
        (
            IpAddr::V4(STATIC4),
            "static_v4",
            1,
            Counts::of(3, 142, 0, 0),
        ),
        (
            IpAddr::V6(STATIC6),
            "static_v6",
            1,
            Counts::of(4, 196, 0, 0),
        ),
    ] {
        bed.send(from, P2P_PORT, count, LISTED_PAYLOAD)?;
        let now = bed.wait_for(expected.accept.packets, 0)?;
        report.line(&format!("v6-static: {count} from {from} in {set}: {now}"))?;
        assert_eq!(
            now, expected,
            "v6-static: {from} in {set} was not accepted and counted once per datagram"
        );
    }
    bed.send(UNLISTED6, P2P_PORT, 2, UNLISTED_PAYLOAD)?;
    let dropped = bed.wait_for(4, 2)?;
    report.line(&format!(
        "v6-static: 2 from the unlisted {UNLISTED6}: {dropped}"
    ))?;
    bed.save_json("v6-static")?;
    assert_eq!(
        dropped,
        Counts::of(4, 196, 2, 112),
        "v6-static: the unlisted IPv6 source was not dropped and counted"
    );

    let listed = |ip: IpAddr| (ip, LISTED_PAYLOAD.to_owned());
    assert_eq!(
        drain(p2p4)?,
        vec![listed(IpAddr::V4(STATIC4))],
        "v6-static: the IPv4 socket got other than the static_v4 source's datagram"
    );
    let mut delivered = drain(p2p6)?;
    delivered.sort();
    let mut expected = vec![
        listed(IpAddr::V6(LISTED6)),
        listed(IpAddr::V6(LISTED6)),
        listed(IpAddr::V6(STATIC6)),
    ];
    expected.sort();
    assert_eq!(
        delivered, expected,
        "v6-static: the IPv6 socket got other than the listed sources' datagrams"
    );
    Ok(())
}

/// Loads static sets whose networks touch, [`ADJACENT_V4`] and [`ADJACENT_V6`], and checks that nft lists each network as its own prefix, so the idempotence compare matches and a sync does not re-apply on every run.
///
/// A datagram from inside the second network is accepted and one from the gap after it dropped.
fn adjacent(bed: &Bed, report: &mut Report, p2p: &UdpSocket) -> TestResult {
    let mut sets = base_sets()?;
    sets.static_v4 = ADJACENT_V4
        .iter()
        .map(|net| parse_cidr_or_host(net))
        .collect::<Result<_, _>>()?;
    sets.static_v6 = ADJACENT_V6
        .iter()
        .map(|net| parse_cidr_or_host(net))
        .collect::<Result<_, _>>()?;
    bed.load("adjacent", &sets)?;
    let listing = bed.listing()?;
    bed.save_json("adjacent")?;
    let matches = listing_matches(&listing, &sets, &ports());
    report.line(&format!(
        "adjacent: static_v4 lists as {:?}, static_v6 as {:?}; the idempotence compare matches: {matches}",
        listing.sets.get("static_v4"),
        listing.sets.get("static_v6")
    ))?;
    for (name, networks) in [
        ("static_v4", &sets.static_v4),
        ("static_v6", &sets.static_v6),
    ] {
        let mut prefixes: Vec<Element> = networks
            .iter()
            .map(|cidr| Element::Cidr {
                net: cidr.net,
                prefix: cidr.prefix,
            })
            .collect();
        prefixes.sort_unstable();
        assert_eq!(
            listing.sets.get(name),
            Some(&prefixes),
            "adjacent: {name} does not list its networks as separate prefixes"
        );
    }
    // a mismatch would re-apply the program on every run
    assert!(
        matches,
        "adjacent: the idempotence compare does not match {listing:?}"
    );

    let before = counts(&listing)?;
    bed.send(IN_ADJACENT, P2P_PORT, 1, LISTED_PAYLOAD)?;
    bed.send(IN_GAP, P2P_PORT, 1, UNLISTED_PAYLOAD)?;
    let after = bed.wait_for(5, 3)?;
    report.line(&format!(
        "adjacent: one from {IN_ADJACENT} and one from {IN_GAP}: before {before}; after {after}"
    ))?;
    assert_eq!(
        after,
        Counts::of(5, 230, 3, 148),
        "adjacent: {IN_ADJACENT} was not accepted or {IN_GAP} not dropped"
    );
    assert_eq!(
        drain(p2p)?,
        vec![(IpAddr::V4(IN_ADJACENT), LISTED_PAYLOAD.to_owned())],
        "adjacent: the node got other than the datagram from inside the networks"
    );
    Ok(())
}

/// Checks every golden program with [`Nft::check_program`], `nft -c -f -` through the runner's pipe, against the live table, then a program with a syntax error, and checks that none of it touched the table.
///
/// The goldens hold other elements than the live table and the refused program carries [`ADDED`] before its error, so a check that loaded anything would show in the listing; the rule handles would change too.
fn syntax(bed: &Bed, report: &mut Report, changed: &Sets) -> TestResult {
    let before = bed.listing()?;
    let handles = rule_handles(&bed.raw_json()?)?;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden");
    let mut goldens = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "nft") {
            goldens.push(path);
        }
    }
    goldens.sort();
    assert!(
        !goldens.is_empty(),
        "syntax: no golden program in {}",
        dir.display()
    );
    let mut names = Vec::new();
    for path in &goldens {
        let text = fs::read_to_string(path)?;
        bed.nft
            .check_program(&text)
            .map_err(|e| format!("syntax: nft -c refused {}: {e}", path.display()))?;
        names.push(
            path.file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        );
    }

    let bogus = format!(
        "{}flush tabel inet tn_p2p_allow\n",
        changed.program(&ports())
    );
    let refused = bed.nft.check_program(&bogus);
    report.line(&format!(
        "syntax: nft -c -f - accepted {}; a syntax error: {}",
        names.join(", "),
        refused
            .as_ref()
            .map_or_else(ToString::to_string, |()| "accepted".to_owned())
    ))?;
    assert!(
        matches!(&refused, Err(NftError::Failed { stderr, .. }) if stderr.contains("syntax error")),
        "syntax: nft -c did not refuse a syntax error: {refused:?}"
    );
    assert_eq!(bed.listing()?, before, "syntax: nft -c changed the table");
    assert_eq!(
        rule_handles(&bed.raw_json()?)?,
        handles,
        "syntax: nft -c replaced the rules"
    );
    Ok(())
}

/// Loads static networks that end at the top of the address space, `/0` and the upper half, and checks that the sync's idempotence compare matches how nft lists them.
///
/// nft 1.0.9 prints such a network as a prefix and nft 1.0.2 as a range up to the last address, so either form passes here, and the compare must match both.
/// The summary records which one this nft printed.
fn top_of_space(bed: &Bed, report: &mut Report) -> TestResult {
    for (name, v4, v6) in [
        ("catch-all", "0.0.0.0/0", "::/0"),
        ("upper-half", "128.0.0.0/1", "8000::/1"),
    ] {
        let (v4, v6) = (parse_cidr_or_host(v4)?, parse_cidr_or_host(v6)?);
        let mut sets = base_sets()?;
        sets.static_v4 = BTreeSet::from([v4]);
        sets.static_v6 = BTreeSet::from([v6]);
        bed.load(name, &sets)?;
        let listing = bed.listing()?;
        let matches = listing_matches(&listing, &sets, &ports());
        report.line(&format!(
            "{name}: static_v4 {v4} lists as {:?}, static_v6 {v6} as {:?}; the idempotence compare matches: {matches}",
            listing.sets.get("static_v4"),
            listing.sets.get("static_v6")
        ))?;
        bed.save_json(name)?;
        for (set, Cidr { net, prefix }) in [("static_v4", v4), ("static_v6", v6)] {
            let last = match net {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::BROADCAST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::from(u128::MAX)),
            };
            let listed = listing.sets.get(set);
            assert!(
                listed == Some(&vec![Element::Cidr { net, prefix }])
                    || listed == Some(&vec![Element::Range(net, last)]),
                "{set} lists {net}/{prefix} as {listed:?}"
            );
        }
        // a mismatch would re-apply the program on every run
        assert!(
            matches,
            "{name}: the idempotence compare does not match {listing:?}"
        );
    }
    Ok(())
}

/// Runs the built binary's own `sync` twice with the system's nft, against a [`FakeRpc`] on `127.0.0.1` serving `tests/fixtures/snapshot/good.json`.
///
/// The binary and the fake node share the test's network namespace, where the table lives too; the senders' namespace plays no part.
/// The first run replaces whatever program the table holds and says `result=applied`; the second says `result=unchanged` and runs no `nft -f`.
/// With the real nft its calls cannot be counted, so the witness is the table itself: every load flushes the table and adds the rules again under new handles, so equal rule handles, an equal listing and a `current.nft` that was not rewritten mean nothing was loaded.
fn binary_sync(bed: &Bed, report: &mut Report) -> TestResult {
    let bin = Path::new(BIN);
    if !bin.is_file() {
        return report.line(&format!(
            "binary-sync: skipped, {BIN} is absent (the test binary was moved away from its build)"
        ));
    }
    let state = bed.temp.join("state");
    fs::create_dir(&state)?;
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
    let answers = || vec![fake_rpc::result(SNAPSHOT), fake_rpc::result(EPOCH_INFO)];
    let mut rpc = FakeRpc::start(answers())?;
    write_private(
        &state.join("config"),
        &format!(
            "# tn-p2p-allow-sync config, written by real_nft\nCONFIG_VERSION=1\nRPC_PORT={}\nP2P_PORTS=49590,49594\nP2P_ALLOW_FROM=\n",
            rpc.port()
        ),
    )?;
    let sync = || -> Res<(Option<i32>, String, String)> {
        let output = command(bin)
            .arg("--state-dir")
            .arg(&state)
            .arg("sync")
            .output()?;
        Ok((
            output.status.code(),
            String::from_utf8(output.stdout)?,
            String::from_utf8(output.stderr)?,
        ))
    };
    let current = state.join(CURRENT_FILE);
    let stamp = || -> Res<(SystemTime, u64)> {
        let meta = fs::metadata(&current)?;
        Ok((meta.modified()?, meta.ino()))
    };

    let (code, first, stderr) = sync()?;
    assert!(
        code == Some(0) && first.starts_with("result=applied "),
        "binary-sync: the first run did not apply (exit {code:?}): {first}{stderr}"
    );
    assert!(
        bed.nft.table_present()?,
        "binary-sync: the first run left no table"
    );
    let applied = bed.listing()?;
    let handles = rule_handles(&bed.save_json("binary-sync")?)?;
    let written = stamp()?;
    for (key, set) in [("v4", "allow_v4"), ("v6", "allow_v6")] {
        assert_eq!(
            applied.sets.get(set).map(Vec::len),
            Some(field(&first, key)?),
            "binary-sync: {set} does not hold the {key}= addresses of {first}"
        );
    }
    assert!(
        applied.chain.is_some()
            && applied.rules.len() == INPUT_RULES
            && COUNTERS
                .iter()
                .all(|name| applied.counters.contains_key(*name)),
        "binary-sync: the binary's table lacks chain input, one of its rules or a counter: {applied:?}"
    );

    rpc.set(answers())?;
    let (code, second, stderr) = sync()?;
    report.line(&format!(
        "binary-sync: {BIN}: first run {}, second run {}",
        first.trim(),
        second.trim()
    ))?;
    assert!(
        code == Some(0) && second.starts_with("result=unchanged "),
        "binary-sync: the second run was not unchanged (exit {code:?}): {second}{stderr}"
    );
    assert_eq!(
        stamp()?,
        written,
        "binary-sync: the unchanged run rewrote {CURRENT_FILE}"
    );
    assert_eq!(
        rule_handles(&bed.raw_json()?)?,
        handles,
        "binary-sync: the unchanged run loaded the program again"
    );
    assert_eq!(
        bed.listing()?,
        applied,
        "binary-sync: the unchanged run changed the table"
    );
    assert_eq!(
        rpc.methods(),
        [
            SNAPSHOT_METHOD,
            EPOCH_INFO_METHOD,
            SNAPSHOT_METHOD,
            EPOCH_INFO_METHOD
        ],
        "binary-sync: the runs did not ask the node for the snapshot and the epoch info once each"
    );
    Ok(())
}

/// Times loads of 10000 IPv4 plus 10000 IPv6 elements: over the live table, the same program again, and from nothing.
fn big_load(bed: &Bed, report: &mut Report) -> TestResult {
    let sets = big_sets();
    let over = bed.load("big", &sets)?;
    let again = bed.load("big", &sets)?;
    bed.nft.delete_table()?;
    let fresh = bed.load("big", &sets)?;
    let started = Instant::now();
    let listing = bed.listing()?;
    let listed = started.elapsed();
    report.line(&format!(
        "load of {BIG} IPv4 + {BIG} IPv6 elements: over the live table {:.3} s, the same program again {:.3} s, from nothing {:.3} s; list and parse {:.3} s",
        over.as_secs_f64(),
        again.as_secs_f64(),
        fresh.as_secs_f64(),
        listed.as_secs_f64()
    ))?;
    bed.save_json("big")?;
    let size = |name: &str| listing.sets.get(name).map(Vec::len);
    assert_eq!(size("allow_v4"), Some(10_000));
    assert_eq!(size("allow_v6"), Some(10_000));
    assert!(
        listing_matches(&listing, &sets, &ports()),
        "the big table does not match its sets"
    );
    for (what, took) in [
        ("over the live table", over),
        ("again", again),
        ("from nothing", fresh),
    ] {
        assert!(took < LOAD_LIMIT, "the big load {what} took {took:?}");
    }
    Ok(())
}

/// Times a load of [`LARGE`] IPv4 plus [`LARGE_V6`] IPv6 elements over the live big table and then again over itself, and the listing and parse of the result, as `load-65k seconds=…` in the summary.
///
/// The program's size has no cap of its own, so this records how nft scales towards what a long-running memory could hold.
fn large_load(bed: &Bed, report: &mut Report) -> TestResult {
    let sets = Sets {
        allow_v4: (1..=LARGE)
            .map(|i| Ipv4Addr::from(0x0a00_0000 | i))
            .collect(),
        allow_v6: (1..=LARGE_V6)
            .map(|i| Ipv6Addr::from(0x2001_0db8_0000_0000_0000_0000_0000_0000_u128 | u128::from(i)))
            .collect(),
        ..Sets::default()
    };
    let program = sets.program(&ports()).len();
    let over = bed.load("large", &sets)?;
    let again = bed.load("large", &sets)?;
    let started = Instant::now();
    let listing = bed.listing()?;
    let listed = started.elapsed();
    let json = bed.save_json("large")?;
    report.line(&format!(
        "load-65k seconds={:.3} again-seconds={:.3} list-seconds={:.3} listing-bytes={} program-bytes={program} ({LARGE} IPv4 + {LARGE_V6} IPv6 elements over the 20000-element table, then over themselves)",
        over.as_secs_f64(),
        again.as_secs_f64(),
        listed.as_secs_f64(),
        json.len()
    ))?;
    let size = |name: &str| listing.sets.get(name).map(Vec::len);
    assert_eq!(
        size("allow_v4"),
        Some(usize::try_from(LARGE)?),
        "load-65k: allow_v4 lost elements"
    );
    assert_eq!(
        size("allow_v6"),
        Some(usize::try_from(LARGE_V6)?),
        "load-65k: allow_v6 lost elements"
    );
    assert!(
        listing_matches(&listing, &sets, &ports()),
        "load-65k: the large table does not match its sets"
    );
    for (what, took) in [("over the big table", over), ("again", again)] {
        assert!(
            took < LARGE_LIMIT,
            "load-65k: the load {what} took {took:?}"
        );
    }
    Ok(())
}

/// [`PORTS`] as the set the program and the idempotence check take.
fn ports() -> BTreeSet<u16> {
    BTreeSet::from(PORTS)
}

/// The base program's sets: one listed address per family and one network in each static set, none of which covers [`UNLISTED`].
fn base_sets() -> Res<Sets> {
    Ok(Sets {
        allow_v4: BTreeSet::from([LISTED]),
        allow_v6: BTreeSet::from([LISTED6]),
        static_v4: BTreeSet::from([parse_cidr_or_host("198.51.100.0/24")?]),
        static_v6: BTreeSet::from([parse_cidr_or_host("2001:db8:5::/48")?]),
    })
}

/// The base sets with [`ADDED`] in `allow_v4`.
fn changed_sets() -> Res<Sets> {
    let mut sets = base_sets()?;
    sets.allow_v4.insert(ADDED);
    Ok(sets)
}

/// [`BIG`] addresses per family, `10.0.0.1` and `2001:db8::1` onwards.
fn big_sets() -> Sets {
    Sets {
        allow_v4: (1..=BIG).map(|i| Ipv4Addr::from(0x0a00_0000 | i)).collect(),
        allow_v6: (1..=BIG)
            .map(|i| Ipv6Addr::from(0x2001_0db8_0000_0000_0000_0000_0000_0000_u128 | u128::from(i)))
            .collect(),
        ..Sets::default()
    }
}

/// The two named counters of one listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    accept: Counter,
    drop: Counter,
}

impl Counts {
    /// The accept counter at `accept_packets` and `accept_bytes`, the drop counter at `drop_packets` and `drop_bytes`.
    const fn of(
        accept_packets: u64,
        accept_bytes: u64,
        drop_packets: u64,
        drop_bytes: u64,
    ) -> Self {
        Self {
            accept: Counter {
                packets: accept_packets,
                bytes: accept_bytes,
            },
            drop: Counter {
                packets: drop_packets,
                bytes: drop_bytes,
            },
        }
    }
}

impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [accept, drop] = COUNTERS;
        write!(
            f,
            "{accept} {} packets {} bytes, {drop} {} packets {} bytes",
            self.accept.packets, self.accept.bytes, self.drop.packets, self.drop.bytes
        )
    }
}

fn counts(listing: &Listing) -> Res<Counts> {
    assert!(listing.chain.is_some(), "chain input is missing");
    assert_eq!(listing.rules.len(), INPUT_RULES, "chain input lost rules");
    let get = |name: &str| {
        listing
            .counters
            .get(name)
            .copied()
            .ok_or_else(|| format!("counter {name} is missing"))
    };
    let [accept, drop] = COUNTERS;
    Ok(Counts {
        accept: get(accept)?,
        drop: get(drop)?,
    })
}

/// The test's world: a namespace holding the senders, the veth pair into it, and the runner for the table.
///
/// Dropping it deletes the table and the namespace, whose end of the pair takes the other end with it.
struct Bed {
    ip: PathBuf,
    nft: Nft,
    ns: String,
    host_if: String,
    peer_if: String,
    temp: TempDir,
}

impl Bed {
    /// Creates the namespace and the pair: [`HOST`] and [`HOST6`] on this end, every sender on the far one, and on this end the [`ROUTES`] back to the senders outside its subnets.
    ///
    /// IPv6 addresses skip duplicate address detection, so they are usable at once.
    fn setup(&self) -> TestResult {
        let (ns, host, peer) = (
            self.ns.as_str(),
            self.host_if.as_str(),
            self.peer_if.as_str(),
        );
        self.ip(&["netns", "add", ns])?;
        self.ip(&["link", "add", host, "type", "veth", "peer", "name", peer])?;
        self.ip(&["link", "set", peer, "netns", ns])?;
        // a host or container may turn IPv6 off for new interfaces; a new namespace starts with it on
        fs::write(format!("/proc/sys/net/ipv6/conf/{host}/disable_ipv6"), "0")?;
        self.ip(&["addr", "add", &format!("{HOST}/24"), "dev", host])?;
        self.ip(&["addr", "add", &format!("{HOST6}/64"), "dev", host, "nodad"])?;
        self.ip(&["link", "set", host, "up"])?;
        for route in ROUTES {
            self.ip(&["route", "add", route, "dev", host])?;
        }
        self.ip(&["-n", ns, "link", "set", "lo", "up"])?;
        let on_link = [LISTED, UNLISTED, ADDED].map(|ip| format!("{ip}/24"));
        let off_link = [STATIC4, IN_ADJACENT, IN_GAP].map(|ip| format!("{ip}/32"));
        for source in on_link.iter().chain(&off_link) {
            self.ip(&["-n", ns, "addr", "add", source, "dev", peer])?;
        }
        let on_link6 = [LISTED6, UNLISTED6].map(|ip| format!("{ip}/64"));
        for source in on_link6.iter().chain([&format!("{STATIC6}/128")]) {
            self.ip(&["-n", ns, "addr", "add", source, "dev", peer, "nodad"])?;
        }
        self.ip(&["-n", ns, "link", "set", peer, "up"])?;
        Ok(())
    }

    /// Runs `ip` with `args` and fails unless it exits 0.
    fn ip(&self, args: &[&str]) -> Res<Output> {
        checked(
            command(&self.ip).args(args),
            &format!("ip {}", args.join(" ")),
        )
    }

    /// Writes the program for `sets` to `<name>.nft` and loads it with [`Nft::load_file`], returning how long nft took.
    fn load(&self, name: &str, sets: &Sets) -> Res<Duration> {
        let path = self.temp.join(&format!("{name}.nft"));
        fs::write(&path, sets.program(&BTreeSet::from(PORTS)))?;
        let started = Instant::now();
        self.nft.load_file(&path)?;
        Ok(started.elapsed())
    }

    fn listing(&self) -> Res<Listing> {
        Ok(self.nft.list_table()?)
    }

    /// nft's own `-j` listing of the table, as it printed it.
    fn raw_json(&self) -> Res<Vec<u8>> {
        let output = checked(
            command(Path::new(nft::DEFAULT_PATH)).args([
                "-j",
                "list",
                "table",
                "inet",
                "tn_p2p_allow",
            ]),
            "nft -j list table",
        )?;
        Ok(output.stdout)
    }

    /// Saves nft's own `-j` listing of the table as `target/real-nft/<name>.json` and returns it.
    fn save_json(&self, name: &str) -> Res<Vec<u8>> {
        let json = self.raw_json()?;
        fs::write(out_dir().join(format!("{name}.json")), &json)?;
        Ok(json)
    }

    /// Sends `count` datagrams carrying `payload` from `from` in the namespace to the node's address of the same family, [`HOST`] or [`HOST6`], on `port`.
    fn send(&self, from: impl Into<IpAddr>, port: u16, count: u32, payload: &str) -> TestResult {
        let from = from.into();
        let to = match from {
            IpAddr::V4(_) => SocketAddr::from((HOST, port)),
            IpAddr::V6(_) => SocketAddr::from((HOST6, port)),
        };
        let job = format!("{from} {to} {count} {payload}");
        checked(
            command(&self.ip)
                .args(["netns", "exec", &self.ns])
                .arg(std::env::current_exe()?)
                .args([
                    "--exact",
                    TEST_NAME,
                    "--ignored",
                    "--test-threads=1",
                    "--quiet",
                ])
                .env(SEND_VAR, &job),
            &format!("sender {job}"),
        )?;
        Ok(())
    }

    /// The counters once they reach `accept` and `drop` packets, or after [`COUNTER_WAIT`]; the caller checks the values.
    fn wait_for(&self, accept: u64, drop: u64) -> Res<Counts> {
        let started = Instant::now();
        loop {
            let now = counts(&self.listing()?)?;
            if now.accept.packets >= accept && now.drop.packets >= drop {
                // a datagram counted twice, or one too many, would show up by now
                thread::sleep(Duration::from_millis(200));
                return counts(&self.listing()?);
            }
            if started.elapsed() > COUNTER_WAIT {
                return Ok(now);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        // best effort, so whatever the test got to is undone; the table never existed before it
        let _ = self.nft.delete_table();
        let _ = self.ip(&["netns", "del", &self.ns]);
        // still there only when the far end never reached the namespace
        let _ = self.ip(&["link", "del", &self.host_if]);
    }
}

/// Sends from `from` to [`OTHER_PORT`] until one datagram arrives on `other`, the node's socket of that family, so the link is up and the neighbour resolved before anything is counted.
fn warm_up(bed: &Bed, from: IpAddr, other: &UdpSocket) -> TestResult {
    other.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut buf = [0_u8; 64];
    for _ in 0..40 {
        bed.send(from, OTHER_PORT, 1, "warm-up")?;
        match other.recv_from(&mut buf) {
            Ok(_) => {
                drain(other)?;
                return Ok(());
            }
            Err(e) if timed_out(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err("no datagram crossed the veth pair in 40 tries".into())
}

/// Every datagram waiting on `socket`, until none arrives for 300 ms, as source address and payload.
fn drain(socket: &UdpSocket) -> Res<Vec<(IpAddr, String)>> {
    socket.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut got = Vec::new();
    let mut buf = [0_u8; 64];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                let payload = buf.get(..len).unwrap_or_default();
                got.push((from.ip(), String::from_utf8_lossy(payload).into_owned()));
            }
            Err(e) if timed_out(&e) => return Ok(got),
            Err(e) => return Err(e.into()),
        }
    }
}

fn timed_out(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// The sender's whole job, inside the namespace: bind the source address and send.
fn send_datagrams(job: &OsString) -> TestResult {
    let job = job.to_str().ok_or("the sender's job is not UTF-8")?;
    let mut words = job.split_whitespace();
    let (Some(from), Some(to), Some(count), Some(payload), None) = (
        words.next(),
        words.next(),
        words.next(),
        words.next(),
        words.next(),
    ) else {
        return Err(format!("{SEND_VAR}={job:?} is not SRC DST:PORT COUNT PAYLOAD").into());
    };
    let to: SocketAddr = to.parse()?;
    let socket = UdpSocket::bind(SocketAddr::new(from.parse()?, 0))?;
    for _ in 0..count.parse::<u32>()? {
        let sent = socket.send_to(payload.as_bytes(), to)?;
        if sent != payload.len() {
            return Err(format!("sent {sent} of {} bytes", payload.len()).into());
        }
    }
    Ok(())
}

/// The `ip` binary, or why the test cannot run here.
fn prerequisites() -> Res<Result<PathBuf, String>> {
    let euid = effective_uid()?;
    if euid != 0 {
        return Ok(Err(format!(
            "needs root (effective uid {euid}); run the test binary under sudo"
        )));
    }
    if !Path::new(nft::DEFAULT_PATH).is_file() {
        return Ok(Err(format!(
            "{} is missing; install nftables",
            nft::DEFAULT_PATH
        )));
    }
    Ok(IP_PATHS
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("ip is not in {}; install iproute2", IP_PATHS.join(", "))))
}

/// The effective uid, the second field of the `Uid:` line of `/proc/self/status`.
fn effective_uid() -> Res<u32> {
    let status = fs::read_to_string("/proc/self/status")?;
    let uids = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .ok_or("/proc/self/status has no Uid line")?;
    let euid = uids
        .split_whitespace()
        .nth(1)
        .ok_or("the Uid line is short")?;
    Ok(euid.parse()?)
}

/// Records the kernel, the nft version and the OS the run saw.
fn environment(bed: &Bed, report: &mut Report) -> TestResult {
    let version = checked(command(bed.nft.path()).arg("--version"), "nft --version")?;
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")?;
    let os = fs::read_to_string("/etc/os-release")?;
    let pretty = os
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .unwrap_or("unknown")
        .trim_matches('"');
    report.line(&format!("os: {pretty}"))?;
    report.line(&format!("kernel: {}", kernel.trim()))?;
    report.line(&format!(
        "nft: {}",
        String::from_utf8_lossy(&version.stdout).trim()
    ))
}

/// What the test prints, also kept in `target/real-nft/summary.txt` line by line so a failed run leaves it behind.
struct Report {
    file: File,
}

impl Report {
    fn create(dir: &Path) -> Res<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            file: File::create(dir.join("summary.txt"))?,
        })
    }

    fn line(&mut self, text: &str) -> TestResult {
        println!("{text}");
        writeln!(self.file, "{text}")?;
        Ok(())
    }
}

/// `target/real-nft` of the checkout, whatever `--target` the binary was built for.
fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("real-nft")
}

/// A command for `program` with a cleared environment, so nothing the test was started with reaches it.
fn command(program: &Path) -> Command {
    // the test starts ip, nft, its own senders and the binary under test, always from a fixed path
    #[allow(clippy::disallowed_methods)]
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", CHILD_PATH)
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    command
}

/// Runs `command` and returns its output when it exits 0; otherwise fails naming `what` with its stderr.
fn checked(command: &mut Command, what: &str) -> Res<Output> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(output);
    }
    Err(format!(
        "{what}: {}\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

/// The handle of every rule in an `nft -j` listing, in listing order.
///
/// A load flushes the table and adds the rules again, and nft never hands out a handle twice in a table, so equal handles mean no load happened in between.
fn rule_handles(json: &[u8]) -> Res<Vec<u64>> {
    let doc: Value = serde_json::from_slice(json)?;
    let items = doc
        .get("nftables")
        .and_then(Value::as_array)
        .ok_or("the listing has no nftables array")?;
    let handles: Vec<u64> = items
        .iter()
        .filter_map(|item| item.get("rule")?.get("handle")?.as_u64())
        .collect();
    if handles.len() != INPUT_RULES {
        return Err(format!("the listing has {} rule handles", handles.len()).into());
    }
    Ok(handles)
}

/// The number after `key=` in a `sync` summary line.
fn field(summary: &str, key: &str) -> Res<usize> {
    let value = summary
        .split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
        .ok_or_else(|| format!("{summary:?} has no {key}="))?;
    Ok(value.parse()?)
}

/// Writes `text` to `path` with mode 0600, as the deployment writes the config.
fn write_private(path: &Path, text: &str) -> TestResult {
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

// the test reads only its two variables: a sender's job and the CI switch
#[allow(clippy::disallowed_methods)]
fn env_var(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}
