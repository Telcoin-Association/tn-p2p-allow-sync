//! The `state.json` of the last release, read by this build: `tests/fixtures/state/golden.json` seeded into a state dir, then `sync` run over it with the built binary, a [`FakeRpc`] and the fake nft.
//!
//! The golden is a canonical file for three keys over `primary`, `worker-2` and `worker-10`, with IPv4 and IPv6 addresses and one strike.
//! A build that read it as `state-invalid` would leave every upgraded validator at exit 2 until `off` and `on`.
//! So a run whose snapshot holds what the golden holds must read it, find nothing to do and leave the file alone, and a run that changes memory must rewrite it in the same canonical form.
//!
//! Why the unchanged run leaves `state.json` byte for byte as it is, by the rules of the contract's Memory section:
//!
//! - The snapshot is at the golden's epoch, 42, so the state's epoch stays and no `seen` ages (rule 2).
//! - The authoritative `primary` and `worker-2` list exactly the keys and addresses the golden holds for them, each already `seen` at 42 with no strike, so rule 1 writes the same values and rule 3 strikes nothing.
//! - `worker-10` is unavailable, so it is not authoritative and only adds (rule 4).
//!   It lists 0x11 at the address the golden already holds at 42, which changes nothing.
//!   It does not list 0x33, whose strike from an earlier authoritative run (`absent: 1`) therefore neither grows into the second, removing strike nor resets to 0.
//!   That memory's address was seen at 40, two epochs ago, which the TTL still keeps.
//! - `config`, `floor` and `current.nft` give the program those addresses render, and the fake table holds it, so the run loads nothing.
//!
//! The unit tests in `src/memory.rs` pin the same file against the writer byte for byte and fold the same snapshots to reach it.

mod support;

use std::error::Error;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use tn_p2p_allow_sync::memory::{load_state, serialize};

use support::fake_rpc::{self, FakeRpc};
use support::{FakeNft, TempDir, read_optional};

type TestResult = Result<(), Box<dyn Error>>;
type Res<T> = Result<T, Box<dyn Error>>;

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_tn-p2p-allow-sync");

/// The canonical `state.json` the last release wrote, at epoch 42.
const GOLDEN_STATE: &str = include_str!("fixtures/state/golden.json");

/// The program of the contract; everything before its first `add element` is the same for every program with the default ports.
const PROGRAM_6_6: &str = include_str!("golden/program-6.6.nft");

/// The floor, one bootstrap peer.
const FLOOR: &str = "# floor 1\n192.0.2.50\n";

/// `allow_v4` of the golden's memory and the floor, in numeric order.
const GOLDEN_V4: &[&str] = &["192.0.2.50", "198.51.100.7", "198.51.100.8", "203.0.113.10"];

/// `allow_v6` of the golden's memory.
const GOLDEN_V6: &[&str] = &["2001:db8::10", "2001:db8::22"];

/// The primary address of 0x33 that the golden holds.
const ADDR_0X33: &str = "/ip4/198.51.100.7/udp/49590/quic-v1";

/// The epoch info every good run asks for after the snapshot.
const EPOCH_INFO: &str = r#"{"epochId":42,"epochDuration":86400}"#;

/// What one run of a program did.
#[derive(Debug)]
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// A state dir seeded as the last release left it after a good run: config, floor, `current.nft`, the golden `state.json`, and the fake table loaded from `current.nft`.
struct Seeded {
    // held for its Drop, which removes the directory
    _temp: TempDir,
    state: PathBuf,
    nft: FakeNft,
    rpc: FakeRpc,
}

impl Seeded {
    fn new(tag: &str) -> Res<Self> {
        let temp = TempDir::new(tag)?;
        let state = temp.join("state");
        fs::create_dir(&state)?;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        let nft = FakeNft::install(temp.path())?;
        let rpc = FakeRpc::start(Vec::new())?;
        let config = format!(
            "# tn-p2p-allow-sync config, written by firewall-setup.sh\nCONFIG_VERSION=1\nRPC_PORT={}\nP2P_PORTS=49590,49594\nP2P_ALLOW_FROM=\n",
            rpc.port()
        );
        write_private(&state.join("config"), &config)?;
        write_private(&state.join("floor"), FLOOR)?;
        let current = state.join("current.nft");
        write_private(&current, &program(GOLDEN_V4, GOLDEN_V6)?)?;
        let golden = state.join("state.json");
        write_private(&golden, GOLDEN_STATE)?;
        // a file a release wrote long ago: a rewrite now cannot keep this mtime, however coarse the clock
        let long_ago = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(1_790_000_000))
            .ok_or("no such time")?;
        File::options()
            .write(true)
            .open(&golden)?
            .set_modified(long_ago)?;
        // the table the last release loaded, through the fake's own parser
        let loaded = run(&nft.path(), &[OsStr::new("-f"), current.as_os_str()])?;
        if loaded.code != Some(0) || nft.table()?.is_none() {
            return Err(format!("the fake nft did not load current.nft: {loaded:?}").into());
        }
        Ok(Self {
            _temp: temp,
            state,
            nft,
            rpc,
        })
    }

    fn file(&self, name: &str) -> PathBuf {
        self.state.join(name)
    }

    /// Runs `sync` against a node that serves `snap` and the epoch info.
    fn sync(&mut self, snap: &Value) -> Res<Ran> {
        self.rpc.set(vec![
            fake_rpc::result(&snap.to_string()),
            fake_rpc::result(EPOCH_INFO),
        ])?;
        let nft = self.nft.path();
        run(
            Path::new(BIN),
            &[
                OsStr::new("--state-dir"),
                self.state.as_os_str(),
                OsStr::new("--nft"),
                nft.as_os_str(),
                OsStr::new("sync"),
            ],
        )
    }

    /// One value of the `status` file, empty when the key or the file is missing.
    fn status_value(&self, key: &str) -> Res<String> {
        let status = read_optional(&self.file("status"))?.unwrap_or_default();
        Ok(status
            .lines()
            .filter_map(|line| line.split_once('='))
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.to_owned())
            .unwrap_or_default())
    }
}

/// Runs `program` with `args`, stdin closed and stderr a pipe, never the journal.
fn run(program: &Path, args: &[&OsStr]) -> Res<Ran> {
    // the binary under test, and the fake nft that loads the seeded table, are the programs this test starts
    #[allow(clippy::disallowed_methods)]
    let mut command = Command::new(program);
    let output = command
        .args(args)
        .env_remove("JOURNAL_STREAM")
        .stdin(Stdio::null())
        .output()?;
    Ok(Ran {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

fn write_private(path: &Path, text: &str) -> Res<()> {
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// The inode and mtime of `path`: a rewrite through a rename changes the first, a write in place the second.
fn stamp(path: &Path) -> Res<(u64, SystemTime)> {
    let meta = fs::metadata(path)?;
    Ok((meta.ino(), meta.modified()?))
}

/// The program for these allow sets, with the default ports and empty static sets.
///
/// Each set holds two or more elements, which the program lists one per line.
fn program(v4: &[&str], v6: &[&str]) -> Res<String> {
    let (preamble, _) = PROGRAM_6_6
        .split_once("add element")
        .ok_or("the contract's program has no element block")?;
    let mut text = preamble.to_owned();
    for (set, elements) in [("allow_v4", v4), ("allow_v6", v6)] {
        text.push_str(&format!(
            "add element inet tn_p2p_allow {set} {{\n    {}\n}}\n",
            elements.join(",\n    ")
        ));
    }
    Ok(text)
}

/// A golden key: `0x`, the byte `n` and zeros.
fn key(n: u8) -> String {
    format!("0x{n:02x}{}", "0".repeat(190))
}

/// An entry for key `n`, resolved from its record to these multiaddrs.
fn entry(n: u8, multiaddrs: &[&str]) -> Value {
    json!({
        "bls_pubkey": key(n),
        "slots": ["current", "eligible"],
        "peer_id": null,
        "network_pubkey": null,
        "multiaddrs": multiaddrs,
        "record_timestamp": 1_793_750_400_u64,
        "source": "record",
        "admitted": true,
    })
}

/// An available swarm whose pool is published for epoch 42, so it is authoritative.
fn available(network: &str, worker_id: Option<u16>, entries: Vec<Value>) -> Value {
    json!({
        "network": network,
        "worker_id": worker_id,
        "available": true,
        "tracked_epoch": 42,
        "admission": {
            "configured": "Closed",
            "effective": "Closed",
            "epoch": 42,
            "fallback": null,
            "resolved_current": 3,
            "required_current": 3,
            "connected_current": 3,
            "contradicted": 0,
        },
        "entries": entries,
    })
}

/// The snapshot the golden came from, at its epoch, with `addrs_0x33` as the primary addresses of 0x33.
fn snapshot(addrs_0x33: &[&str]) -> Value {
    json!({
        "version": 1,
        "epoch": 42,
        "swarms": [
            available("primary", None, vec![
                entry(0x11, &["/ip4/203.0.113.10/udp/49590/quic-v1", "/ip6/2001:db8::10/udp/49590/quic-v1"]),
                entry(0x22, &["/ip6/2001:db8::22/udp/49590/quic-v1"]),
                entry(0x33, addrs_0x33),
            ]),
            available("worker-2", Some(2), vec![
                entry(0x11, &["/ip4/203.0.113.10/udp/49594/quic-v1"]),
                entry(0x22, &["/ip6/2001:db8::22/udp/49594/quic-v1"]),
            ]),
            // timed out: its entries are carried, but it is not authoritative, so 0x33's strike stays where it is
            {
                "network": "worker-10",
                "worker_id": 10,
                "available": false,
                "tracked_epoch": null,
                "admission": null,
                "entries": [entry(0x11, &["/ip4/203.0.113.10/udp/49594/quic-v1"])],
            },
        ],
    })
}

#[test]
fn a_state_json_of_the_last_release_is_read_unchanged() -> TestResult {
    let mut bed = Seeded::new("state-golden-unchanged")?;
    let golden = bed.file("state.json");
    let before = stamp(&golden)?;
    let calls = bed.nft.argv()?.len();

    let ran = bed.sync(&snapshot(&[ADDR_0X33]))?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=unchanged reason= epoch=42 v4=4 v6=2 static=0 floor=1 keys=3 dns_skipped=0 unparsed=0\n"
    );
    // the two listings of the idempotence check, and no load
    let since: Vec<String> = bed.nft.argv()?.into_iter().skip(calls).collect();
    assert_eq!(
        since,
        ["-j list tables inet", "-j list table inet tn_p2p_allow"]
    );
    assert_eq!(bed.nft.loaded()?.len(), 1);
    assert_eq!(fs::read_to_string(&golden)?, GOLDEN_STATE);
    assert_eq!(stamp(&golden)?, before, "state.json was rewritten");
    assert_eq!(bed.status_value("RESULT")?, "unchanged");
    assert_eq!(bed.status_value("EPOCH")?, "42");
    assert_eq!(bed.status_value("KEYS")?, "3");
    // no warning about the file or the floor: the one line is the notice of the fake's path
    assert!(
        ran.stderr.lines().all(|line| line.starts_with("nft-path ")),
        "{ran:?}"
    );
    Ok(())
}

#[test]
fn one_more_address_rewrites_the_golden_in_canonical_form() -> TestResult {
    let mut bed = Seeded::new("state-golden-applied")?;
    let golden = bed.file("state.json");
    let before = stamp(&golden)?;

    let ran = bed.sync(&snapshot(&[ADDR_0X33, "/ip4/192.0.2.20/udp/49590/quic-v1"]))?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=5 v6=2 static=0 floor=1 keys=3 dns_skipped=0 unparsed=0\n"
    );
    let applied = program(
        &[
            "192.0.2.20",
            "192.0.2.50",
            "198.51.100.7",
            "198.51.100.8",
            "203.0.113.10",
        ],
        GOLDEN_V6,
    )?;
    assert_eq!(fs::read_to_string(bed.file("current.nft"))?, applied);

    // the one new address, in numeric order inside 0x33's primary memory; nothing else moves
    let expected = GOLDEN_STATE.replacen(
        r#"{"198.51.100.7":42}"#,
        r#"{"192.0.2.20":42,"198.51.100.7":42}"#,
        1,
    );
    assert_ne!(expected, GOLDEN_STATE);
    let written = fs::read(&golden)?;
    assert_eq!(String::from_utf8(written.clone())?, expected);
    let (inode, _) = stamp(&golden)?;
    assert_ne!(inode, before.0, "state.json was not replaced");
    assert_eq!(serialize(&load_state(Some(&written))?), written);
    assert_eq!(bed.status_value("RESULT")?, "applied");
    Ok(())
}
