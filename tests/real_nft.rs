//! Real-kernel test of the nft program: the rendered program loaded into the running kernel by the system's nft, datagrams sent through a veth pair, and the named counters read back through [`Nft::list_table`].
//!
//! It needs root, `/usr/sbin/nft` and `ip`, so it is ignored by default; CI runs it under `sudo` on each Ubuntu release (`.github/workflows/nft-real.yaml`).
//! Without root or either tool it prints why and passes, unless `TN_REAL_NFT_REQUIRED=1` makes that a failure, as CI does.
//!
//! The table is loaded in the test's own network namespace, which on a CI runner is the host's.
//! The senders live in a namespace of their own at the far end of a veth pair, so every datagram crosses the input hook; a sender is this test binary run again inside that namespace with `TN_REAL_NFT_SEND` set.
//! The test refuses to run when table `inet tn_p2p_allow` already exists, and deletes the table and the namespace when it ends, pass or fail.
//! Raw `nft -j` listings and `summary.txt` land in `target/real-nft/`, which CI uploads.

mod support;

use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tn_p2p_allow_sync::addr::{Cidr, parse_cidr_or_host};
use tn_p2p_allow_sync::nft::{self, Counter, Element, Listing, Nft};
use tn_p2p_allow_sync::sync::{COUNTERS, INPUT_RULES, Sets, listing_matches};

use support::TempDir;

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

/// The P2P ports of the program; every datagram counted goes to the first.
const PORTS: [u16; 2] = [49590, 49594];
const P2P_PORT: u16 = 49590;
/// A port outside the program, which the chain must leave alone.
const OTHER_PORT: u16 = 40000;

/// Payload of an accepted datagram; with 20 bytes of IPv4 header and 8 of UDP header it is 34 bytes to the counter.
const LISTED_PAYLOAD: &str = "listed";
/// Payload of a dropped datagram, 36 bytes to the counter.
const UNLISTED_PAYLOAD: &str = "unlisted";

/// Elements per family in the timed load.
const BIG: u32 = 10_000;
/// The longest a load of the big program may take.
const LOAD_LIMIT: Duration = Duration::from_secs(10);
/// How long the counters get to reach the expected packet counts.
const COUNTER_WAIT: Duration = Duration::from_secs(10);

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
    let counted = traffic(&bed, &mut report, &base, &p2p, &other)?;
    reapply(&bed, &mut report, &base, &changed, counted, &p2p)?;
    reload(&bed, &mut report, &changed)?;
    top_of_space(&bed, &mut report)?;
    big_load(&bed, &mut report)?;

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

    warm_up(bed, other)?;
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

/// [`PORTS`] as the set the program and the idempotence check take.
fn ports() -> BTreeSet<u16> {
    BTreeSet::from(PORTS)
}

/// The base program's sets: one listed address per family and one network in each static set, none of which covers [`UNLISTED`].
fn base_sets() -> Res<Sets> {
    Ok(Sets {
        allow_v4: BTreeSet::from([LISTED]),
        allow_v6: BTreeSet::from(["2001:db8::2".parse::<Ipv6Addr>()?]),
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
    /// Creates the namespace and the pair: [`HOST`] on this end, the three senders on the far one.
    fn setup(&self) -> TestResult {
        let (ns, host, peer) = (
            self.ns.as_str(),
            self.host_if.as_str(),
            self.peer_if.as_str(),
        );
        let host_cidr = format!("{HOST}/24");
        self.ip(&["netns", "add", ns])?;
        self.ip(&["link", "add", host, "type", "veth", "peer", "name", peer])?;
        self.ip(&["link", "set", peer, "netns", ns])?;
        self.ip(&["addr", "add", &host_cidr, "dev", host])?;
        self.ip(&["link", "set", host, "up"])?;
        self.ip(&["-n", ns, "link", "set", "lo", "up"])?;
        for source in [LISTED, UNLISTED, ADDED] {
            self.ip(&[
                "-n",
                ns,
                "addr",
                "add",
                &format!("{source}/24"),
                "dev",
                peer,
            ])?;
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

    /// Saves nft's own `-j` listing of the table as `target/real-nft/<name>.json`.
    fn save_json(&self, name: &str) -> TestResult {
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
        fs::write(out_dir().join(format!("{name}.json")), output.stdout)?;
        Ok(())
    }

    /// Sends `count` datagrams carrying `payload` from `from` in the namespace to [`HOST`] on `port`.
    fn send(&self, from: Ipv4Addr, port: u16, count: u32, payload: &str) -> TestResult {
        let job = format!("{from} {HOST}:{port} {count} {payload}");
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

/// Sends from the listed source to [`OTHER_PORT`] until one datagram arrives, so the link is up and the neighbour resolved before anything is counted.
fn warm_up(bed: &Bed, other: &UdpSocket) -> TestResult {
    other.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut buf = [0_u8; 64];
    for _ in 0..40 {
        bed.send(LISTED, OTHER_PORT, 1, "warm-up")?;
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
    // the test starts ip, nft and its own senders, always from a fixed path
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

// the test reads only its two variables: a sender's job and the CI switch
#[allow(clippy::disallowed_methods)]
fn env_var(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}
