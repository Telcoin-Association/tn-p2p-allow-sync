//! Integration tests of `sync` and `render`: the built binary against a fake JSON-RPC server and a fake nft.
//!
//! Each test gets its own directory holding the state dir, mode 0700, and a link to `tests/support/fake-nft.sh`, whose files, kept next to the link, show every call the tool made and the table it left loaded.
//! The node is a [`FakeRpc`] that answers each call with the next canned response.

mod support;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use support::fake_rpc::{self, FakeRpc, Response};
use support::{FakeNft, TempDir, read_optional};

type TestResult = Result<(), Box<dyn Error>>;
type Res<T> = Result<T, Box<dyn Error>>;

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_tn-p2p-allow-sync");

/// The program of the contract; everything before its first `add element` is the same for every program with the default ports.
const GOLDEN: &str = include_str!("golden/program-6.6.nft");

const STAGED: &str = ".current.nft.new";

/// The epoch info every good run asks for after the snapshot.
const EPOCH_INFO: &str = r#"{"epochId":42,"epochDuration":86400}"#;

/// What one run of the binary did.
#[derive(Debug)]
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ran {
    /// Whether stderr holds a line starting with `start`.
    fn logged(&self, start: &str) -> bool {
        self.stderr.lines().any(|line| line.starts_with(start))
    }
}

/// When and as which inode a file was last written; a rewrite through a rename changes both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    inode: u64,
}

/// One test's world: the state dir, the fake nft and the fake node.
struct Bed {
    temp: TempDir,
    state: PathBuf,
    nft: FakeNft,
    rpc: FakeRpc,
}

impl Bed {
    /// A state dir holding only a config with the default ports and no static entries.
    fn new(tag: &str) -> Res<Self> {
        let temp = TempDir::new(tag)?;
        let state = temp.join("state");
        fs::create_dir(&state)?;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        let nft = FakeNft::install(temp.path())?;
        let rpc = FakeRpc::start(Vec::new())?;
        let bed = Self {
            temp,
            state,
            nft,
            rpc,
        };
        bed.config("")?;
        Ok(bed)
    }

    /// Writes the config with the default ports and `allow_from` as `P2P_ALLOW_FROM`.
    fn config(&self, allow_from: &str) -> Res<()> {
        let text = format!(
            "# tn-p2p-allow-sync config, written by firewall-setup.sh\nCONFIG_VERSION=1\nRPC_PORT={}\nP2P_PORTS=49590,49594\nP2P_ALLOW_FROM={allow_from}\n",
            self.rpc.port()
        );
        write_private(&self.file("config"), &text)
    }

    /// Runs the binary with `--state-dir` and `--nft` set and `args` after them.
    fn run(&self, args: &[&str]) -> Res<Ran> {
        // the binary under test is the one program these tests start
        #[allow(clippy::disallowed_methods)]
        let mut command = Command::new(BIN);
        let output = command
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--nft")
            .arg(self.nft.path())
            .args(args)
            // stderr is a pipe, never the journal
            .env_remove("JOURNAL_STREAM")
            .stdin(Stdio::null())
            .output()?;
        Ok(Ran {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        })
    }

    /// Queues `responses` on the node, then runs `sync` with `args`.
    fn sync_with(&mut self, responses: Vec<Response>, args: &[&str]) -> Res<Ran> {
        self.rpc.set(responses)?;
        let mut all = vec!["sync"];
        all.extend_from_slice(args);
        self.run(&all)
    }

    /// Queues `responses` on the node, then runs a plain `sync`.
    fn sync(&mut self, responses: Vec<Response>) -> Res<Ran> {
        self.sync_with(responses, &[])
    }

    /// Runs `sync` against a node that serves `snap` and the epoch info.
    fn sync_snapshot(&mut self, snap: &Value) -> Res<Ran> {
        self.sync(answers(snap))
    }

    fn file(&self, name: &str) -> PathBuf {
        self.state.join(name)
    }

    fn read(&self, name: &str) -> Res<Option<String>> {
        Ok(read_optional(&self.file(name))?)
    }

    /// The `status` file as a map; empty when it does not exist.
    fn status(&self) -> Res<BTreeMap<String, String>> {
        Ok(self
            .read("status")?
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect())
    }

    /// One value of `status`, empty when the key or the file is missing.
    fn status_value(&self, key: &str) -> Res<String> {
        Ok(self.status()?.remove(key).unwrap_or_default())
    }

    fn stamp(&self, name: &str) -> Res<Option<Stamp>> {
        match fs::symlink_metadata(self.file(name)) {
            Ok(meta) => Ok(Some(Stamp {
                modified: meta.modified()?,
                inode: meta.ino(),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn mode(&self, name: &str) -> Res<u32> {
        Ok(fs::symlink_metadata(self.file(name))?.mode() & 0o7777)
    }

    /// The names in the state dir, sorted.
    fn names(&self) -> Res<Vec<String>> {
        let mut names = fs::read_dir(&self.state)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Res<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    /// The nft calls made after the first `since`.
    fn calls_since(&self, since: usize) -> Res<Vec<String>> {
        Ok(self.nft.argv()?.into_iter().skip(since).collect())
    }

    fn calls(&self) -> Res<usize> {
        Ok(self.nft.argv()?.len())
    }

    /// The `-f` call that loads the staged program.
    fn load_staged(&self) -> String {
        format!("-f {}", self.file(STAGED).display())
    }

    /// The `-f` call that reloads `current.nft`.
    fn reload_current(&self) -> String {
        format!("-f {}", self.file("current.nft").display())
    }
}

fn write_private(path: &Path, text: &str) -> Res<()> {
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// A `bls_pubkey` made of the byte `n` and zeros.
fn key(n: u8) -> String {
    format!("0x{n:02x}{}", "0".repeat(190))
}

/// The QUIC multiaddr of an IPv4 literal.
fn ip4(ip: &str) -> String {
    format!("/ip4/{ip}/udp/49590/quic-v1")
}

/// The QUIC multiaddr of an IPv6 literal.
fn ip6(ip: &str) -> String {
    format!("/ip6/{ip}/udp/49590/quic-v1")
}

/// An entry for key `n` with these multiaddrs, resolved from its record.
fn entry(n: u8, multiaddrs: &[String]) -> Value {
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

/// An entry for key `n` that the node could not resolve: listed, without addresses.
fn unresolved(n: u8) -> Value {
    json!({
        "bls_pubkey": key(n),
        "slots": ["eligible"],
        "peer_id": null,
        "network_pubkey": null,
        "multiaddrs": [],
        "record_timestamp": null,
        "source": "unresolved",
        "admitted": false,
    })
}

/// An available swarm whose pool is published for `epoch`.
fn swarm(network: &str, worker_id: Option<u16>, epoch: u64, entries: Vec<Value>) -> Value {
    json!({
        "network": network,
        "worker_id": worker_id,
        "available": true,
        "tracked_epoch": epoch,
        "admission": {
            "configured": "Closed",
            "effective": "Closed",
            "epoch": epoch,
            "fallback": null,
            "resolved_current": 4,
            "required_current": 3,
            "connected_current": 3,
            "contradicted": 0,
        },
        "entries": entries,
    })
}

fn primary(epoch: u64, entries: Vec<Value>) -> Value {
    swarm("primary", None, epoch, entries)
}

fn snapshot(epoch: u64, swarms: Vec<Value>) -> Value {
    json!({"version": 1, "epoch": epoch, "swarms": swarms})
}

/// A snapshot whose primary lists `entries` at `epoch`.
fn primary_only(epoch: u64, entries: Vec<Value>) -> Value {
    snapshot(epoch, vec![primary(epoch, entries)])
}

/// Two keys at epoch 42: 0x11 at 203.0.113.10 and 0x22 at 198.51.100.7.
fn two_keys() -> Value {
    primary_only(
        42,
        vec![
            entry(0x11, &[ip4("203.0.113.10")]),
            entry(0x22, &[ip4("198.51.100.7")]),
        ],
    )
}

/// The node's answers to a good run: the snapshot, then the epoch info.
fn answers(snap: &Value) -> Vec<Response> {
    vec![
        fake_rpc::result(&snap.to_string()),
        fake_rpc::result(EPOCH_INFO),
    ]
}

/// The program for these element blocks, each as `(set, elements)`, with the default ports.
fn program(blocks: &[(&str, &[&str])]) -> Res<String> {
    let (preamble, _) = GOLDEN
        .split_once("add element")
        .ok_or("the golden program has no element block")?;
    let mut text = preamble.to_owned();
    for (set, elements) in blocks {
        match elements {
            [] => {}
            [one] => text.push_str(&format!(
                "add element inet tn_p2p_allow {set} {{ {one} }}\n"
            )),
            many => {
                text.push_str(&format!("add element inet tn_p2p_allow {set} {{\n"));
                text.push_str(
                    &many
                        .iter()
                        .map(|e| format!("    {e}"))
                        .collect::<Vec<_>>()
                        .join(",\n"),
                );
                text.push_str("\n}\n");
            }
        }
    }
    Ok(text)
}

/// The fake's table for `program`, as the script parses it.
fn table_of(program: &str) -> String {
    let mut text = String::from(
        "set allow_v4 ipv4_addr\nset allow_v6 ipv6_addr\nset static_v4 ipv4_addr interval\nset static_v6 ipv6_addr interval\n\
         counter tn_accept_p2p_listed\ncounter tn_drop_p2p_unlisted\nchain input filter input -20 accept\n",
    );
    for rule in [
        "ip saddr @allow_v4 counter name \"tn_accept_p2p_listed\" accept",
        "ip saddr @static_v4 counter name \"tn_accept_p2p_listed\" accept",
        "ip6 saddr @allow_v6 counter name \"tn_accept_p2p_listed\" accept",
        "ip6 saddr @static_v6 counter name \"tn_accept_p2p_listed\" accept",
        "counter name \"tn_drop_p2p_unlisted\" drop",
    ] {
        text.push_str(&format!("rule udp dport {{ 49590, 49594 }} {rule}\n"));
    }
    let mut set = None;
    for line in program.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("add element inet tn_p2p_allow ") {
            let (name, rest) = rest.split_once(' ').unwrap_or((rest, ""));
            set = Some(name.to_owned());
            for element in rest
                .trim_matches(|c| c == '{' || c == '}' || c == ' ')
                .split(',')
            {
                if !element.trim().is_empty() {
                    text.push_str(&format!("elem {name} {}\n", element.trim()));
                }
            }
            if rest.ends_with('}') {
                set = None;
            }
        } else if line == "}" {
            set = None;
        } else if let Some(name) = &set {
            text.push_str(&format!("elem {name} {}\n", line.trim_end_matches(',')));
        }
    }
    text
}

#[test]
fn the_first_apply_loads_one_sorted_program_and_records_it() -> TestResult {
    let mut bed = Bed::new("first-apply")?;
    bed.config("198.18.0.0/15,192.0.2.99")?;
    let snap = primary_only(
        42,
        vec![
            entry(0x11, &[ip4("203.0.113.10")]),
            entry(0x22, &[ip4("198.51.100.7")]),
            entry(0x33, &[ip4("192.0.2.10")]),
        ],
    );
    let ran = bed.sync_snapshot(&snap)?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=4 v6=0 static=1 floor=0 keys=3 dns_skipped=0 unparsed=0\n"
    );
    let expected = program(&[
        (
            "allow_v4",
            &["192.0.2.10", "192.0.2.99", "198.51.100.7", "203.0.113.10"],
        ),
        ("static_v4", &["198.18.0.0/15"]),
    ])?;
    // one listing of the tables, then exactly one load; nothing else ran
    assert_eq!(
        bed.nft.argv()?,
        vec!["-j list tables inet".to_owned(), bed.load_staged()]
    );
    assert_eq!(bed.nft.loaded()?, vec![expected.clone()]);
    assert_eq!(bed.read("current.nft")?.as_deref(), Some(expected.as_str()));
    let table = bed.nft.table()?.ok_or("no table loaded")?;
    assert_eq!(table, table_of(&expected));
    assert!(
        table.contains(
            "rule udp dport { 49590, 49594 } counter name \"tn_drop_p2p_unlisted\" drop\n"
        )
    );
    assert_eq!(table.lines().filter(|l| l.starts_with("rule ")).count(), 5);
    assert_eq!(
        table.lines().filter(|l| l.starts_with("counter ")).count(),
        2
    );

    let state = format!(
        "{{\"version\":1,\"epoch\":42,\"keys\":{{\"{}\":{{\"primary\":{{\"absent\":0,\"ips\":{{\"203.0.113.10\":42}}}}}},\"{}\":{{\"primary\":{{\"absent\":0,\"ips\":{{\"198.51.100.7\":42}}}}}},\"{}\":{{\"primary\":{{\"absent\":0,\"ips\":{{\"192.0.2.10\":42}}}}}}}}}}\n",
        key(0x11),
        key(0x22),
        key(0x33)
    );
    assert_eq!(bed.read("state.json")?, Some(state));
    let status = bed.status()?;
    let get = |key: &str| status.get(key).map(String::as_str);
    assert_eq!(get("STATUS_VERSION"), Some("1"));
    assert_eq!(get("TOOL_VERSION"), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(get("RESULT"), Some("applied"));
    assert_eq!(get("EPOCH"), Some("42"));
    assert_eq!(get("EPOCH_DURATION"), Some("86400"));
    assert_eq!(get("STALE_SINCE"), Some(""));
    assert_eq!(get("STALE_REASON"), Some(""));
    assert_eq!(get("ALLOWED_V4"), Some("4"));
    assert_eq!(get("STATIC_V4"), Some("1"));
    assert_eq!(get("KEYS"), Some("3"));
    assert!(get("LAST_GOOD").is_some_and(|good| !good.is_empty()));
    assert_eq!(get("LAST_GOOD"), get("LAST_RUN"));
    for name in ["current.nft", "state.json", "status"] {
        assert_eq!(bed.mode(name)?, 0o600, "{name}");
    }
    assert_eq!(
        bed.names()?,
        ["config", "current.nft", "lock", "state.json", "status"]
    );
    assert_eq!(
        bed.rpc.methods(),
        ["tn_admissionSnapshot", "tn_getCurrentEpochInfo"]
    );
    Ok(())
}

#[test]
fn an_unchanged_snapshot_runs_no_load_and_rewrites_only_status() -> TestResult {
    let mut bed = Bed::new("unchanged")?;
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let current = bed.stamp("current.nft")?;
    let state = bed.stamp("state.json")?;
    let status = bed.stamp("status")?;
    let before = bed.calls()?;

    let ran = bed.sync_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=unchanged reason= epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    );
    assert_eq!(
        bed.calls_since(before)?,
        ["-j list tables inet", "-j list table inet tn_p2p_allow"]
    );
    assert_eq!(bed.nft.loaded()?.len(), 1);
    assert_eq!(bed.stamp("current.nft")?, current);
    assert_eq!(bed.stamp("state.json")?, state);
    assert_ne!(bed.stamp("status")?, status);
    assert_eq!(bed.status_value("RESULT")?, "unchanged");
    Ok(())
}

#[test]
fn a_departed_key_leaves_on_the_second_authoritative_run() -> TestResult {
    let mut bed = Bed::new("departed")?;
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let without = primary_only(42, vec![entry(0x11, &[ip4("203.0.113.10")])]);

    // the first strike keeps the address, so the program stays; the strike itself is saved
    let state = bed.read("state.json")?;
    let before = bed.calls()?;
    let ran = bed.sync_snapshot(&without)?;
    assert_eq!(
        ran.stdout,
        "result=unchanged reason= epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    );
    assert!(
        !bed.calls_since(before)?
            .iter()
            .any(|call| call.starts_with("-f"))
    );
    let struck = bed.read("state.json")?.ok_or("no state.json")?;
    assert_ne!(Some(&struck), state.as_ref());
    assert!(struck.contains(&format!("\"{}\":{{\"primary\":{{\"absent\":1,", key(0x22))));

    let before = bed.calls()?;
    let ran = bed.sync_snapshot(&without)?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=1 v6=0 static=0 floor=0 keys=1 dns_skipped=0 unparsed=0\n"
    );
    let loads = bed
        .calls_since(before)?
        .into_iter()
        .filter(|call| call.starts_with("-f"))
        .count();
    assert_eq!(loads, 1);
    let expected = program(&[("allow_v4", &["203.0.113.10"])])?;
    assert_eq!(bed.read("current.nft")?, Some(expected.clone()));
    assert_eq!(bed.nft.table()?, Some(table_of(&expected)));
    assert!(
        !bed.read("state.json")?
            .unwrap_or_default()
            .contains(&key(0x22))
    );
    Ok(())
}

#[test]
fn an_unresolved_key_keeps_its_address_for_three_epochs() -> TestResult {
    let mut bed = Bed::new("unresolved")?;
    let anchor = || entry(0x11, &[ip4("192.0.2.10")]);
    let ran = bed.sync_snapshot(&primary_only(
        10,
        vec![anchor(), entry(0x22, &[ip4("198.51.100.7")])],
    ))?;
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=10 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    );
    for epoch in [11, 12] {
        let ran = bed.sync_snapshot(&primary_only(epoch, vec![anchor(), unresolved(0x22)]))?;
        assert_eq!(ran.code, Some(0), "{ran:?}");
        assert_eq!(
            ran.stdout,
            format!(
                "result=unchanged reason= epoch={epoch} v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
            )
        );
        assert!(
            bed.nft
                .table()?
                .unwrap_or_default()
                .contains("elem allow_v4 198.51.100.7\n")
        );
    }
    let ran = bed.sync_snapshot(&primary_only(13, vec![anchor(), unresolved(0x22)]))?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=13 v4=1 v6=0 static=0 floor=0 keys=1 dns_skipped=0 unparsed=0\n"
    );
    let expected = program(&[("allow_v4", &["192.0.2.10"])])?;
    assert_eq!(bed.read("current.nft")?, Some(expected.clone()));
    assert_eq!(bed.nft.table()?, Some(table_of(&expected)));
    Ok(())
}

#[test]
fn stale_snapshots_keep_the_last_good_program_and_success_clears_the_staleness() -> TestResult {
    let mut bed = Bed::new("stale")?;
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let current = bed.stamp("current.nft")?;
    let state = bed.stamp("state.json")?;
    let table = bed.nft.table()?;
    let last_good = bed.status_value("LAST_GOOD")?;

    let mut version_2 = two_keys();
    version_2
        .as_object_mut()
        .ok_or("not an object")?
        .insert("version".to_owned(), json!(2));
    let regressed = primary_only(41, vec![entry(0x11, &[ip4("203.0.113.10")])]);
    let cases = [
        (vec![Response::Down], "rpc-unreachable"),
        (vec![fake_rpc::error(-32001)], "rpc-error--32001"),
        (vec![fake_rpc::error(-32601)], "rpc-method-missing"),
        (answers(&version_2), "snapshot-version-2"),
        (answers(&regressed), "epoch-regressed"),
        (vec![Response::Status(500)], "rpc-http-500"),
    ];
    for (n, (responses, reason)) in cases.into_iter().enumerate() {
        let before = bed.calls()?;
        let ran = bed.sync(responses)?;
        assert_eq!(ran.code, Some(3), "{reason}: {ran:?}");
        assert_eq!(
            ran.stdout,
            format!(
                "result=stale reason={reason} epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
            )
        );
        assert!(
            ran.logged(&format!("stale reason={reason} action=kept")),
            "{reason}: {ran:?}"
        );
        // the table is present, so the stale path lists the tables again after the node answered and touches nothing
        assert_eq!(
            bed.calls_since(before)?,
            ["-j list tables inet", "-j list tables inet"],
            "{reason}"
        );
        assert_eq!(bed.nft.table()?, table, "{reason}");
        assert_eq!(bed.stamp("current.nft")?, current, "{reason}");
        assert_eq!(bed.stamp("state.json")?, state, "{reason}");
        let status = bed.status()?;
        assert_eq!(status.get("RESULT").map(String::as_str), Some("stale"));
        assert_eq!(status.get("STALE_REASON").map(String::as_str), Some(reason));
        assert_eq!(status.get("LAST_GOOD"), Some(&last_good), "{reason}");
        let since = status.get("STALE_SINCE").cloned().unwrap_or_default();
        if n == 0 {
            assert!(!since.is_empty());
            // a past value proves that later stale runs never rewrite it
            let text = bed.read("status")?.unwrap_or_default();
            let edited = text.replace(&format!("STALE_SINCE={since}\n"), "STALE_SINCE=1000\n");
            assert_ne!(edited, text);
            write_private(&bed.file("status"), &edited)?;
        } else {
            assert_eq!(since, "1000", "{reason}");
        }
    }

    let ran = bed.sync_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=unchanged reason= epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    );
    let status = bed.status()?;
    assert_eq!(status.get("STALE_SINCE").map(String::as_str), Some(""));
    assert_eq!(status.get("STALE_REASON").map(String::as_str), Some(""));
    assert_eq!(status.get("RESULT").map(String::as_str), Some("unchanged"));
    let good = |s: &str| s.parse::<u64>().unwrap_or(0);
    assert!(good(&bed.status_value("LAST_GOOD")?) >= good(&last_good));
    Ok(())
}

#[test]
fn a_node_that_never_answers_is_unreachable() -> TestResult {
    let mut bed = Bed::new("hang")?;
    let start = Instant::now();
    let ran = bed.sync(vec![Response::Hang])?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=stale reason=rpc-unreachable epoch= v4=0 v6=0 static=0 floor=0 keys=0 dns_skipped=0 unparsed=0\n"
    );
    // the read timeout, well inside the overall deadline
    assert!(start.elapsed() < Duration::from_secs(12));
    assert_eq!(bed.rpc.methods(), ["tn_admissionSnapshot"]);
    Ok(())
}

#[test]
fn a_dns4_address_is_skipped_and_counted() -> TestResult {
    let mut bed = Bed::new("dns4")?;
    let snap = primary_only(
        42,
        vec![entry(
            0x11,
            &[
                "/dns4/validator.example/udp/49590/quic-v1".to_owned(),
                ip4("192.0.2.10"),
            ],
        )],
    );
    let ran = bed.sync_snapshot(&snap)?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=1 v6=0 static=0 floor=0 keys=1 dns_skipped=1 unparsed=0\n"
    );
    assert!(ran.logged("absorbed kind=dns-skipped count=1"), "{ran:?}");
    assert_eq!(bed.status_value("DNS_SKIPPED")?, "1");
    let expected = program(&[("allow_v4", &["192.0.2.10"])])?;
    assert_eq!(bed.read("current.nft")?, Some(expected));
    Ok(())
}

#[test]
fn ipv6_is_written_compressed_and_a_mapped_address_as_ipv4() -> TestResult {
    let mut bed = Bed::new("ipv6")?;
    let snap = primary_only(
        42,
        vec![
            entry(0x11, &[ip6("2001:0db8:0000:0000:0001:0000:0000:0001")]),
            entry(0x22, &[ip6("::ffff:192.0.2.2")]),
            entry(0x33, &[ip6("2001:DB8:0:0:0:0:0:1")]),
        ],
    );
    let ran = bed.sync_snapshot(&snap)?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=1 v6=2 static=0 floor=0 keys=3 dns_skipped=0 unparsed=0\n"
    );
    let expected = program(&[
        ("allow_v4", &["192.0.2.2"]),
        ("allow_v6", &["2001:db8::1", "2001:db8::1:0:0:1"]),
    ])?;
    assert_eq!(bed.read("current.nft")?, Some(expected.clone()));
    assert_eq!(bed.nft.table()?, Some(table_of(&expected)));
    let state = bed.read("state.json")?.unwrap_or_default();
    assert!(state.contains("\"2001:db8::1\":42") && state.contains("\"192.0.2.2\":42"));
    // the listing's IPv6 elements compare as values, so the next run loads nothing
    let ran = bed.sync_snapshot(&snap)?;
    assert!(ran.stdout.starts_with("result=unchanged "), "{ran:?}");
    Ok(())
}

/// A bootstrap peers file: a primary and a worker address, and a name that is never resolved.
const BOOTSTRAP_PEERS: &str = "\
pDmpE29YEhr93MPVPCGkWx3BsCcow4oExmxv5viJz2GTjsaXwc3YwKxE1CFdVuTVSudsFDutFSVmtgF98Abs56JZPfGQs6GzbqXDFkGA1eZx2edkwfP2Q6eRLo8coLhvTNj:
  primary:
    network_address: /ip4/192.0.2.50/udp/49590/quic-v1/p2p/12D3KooWCnizjv4ctsBTwwYJxZP3duCuoebJfbk1fUQv6Vg1dVUh
    network_key: 4XTTM47Qeb4XhWEK7ybzj5bGsnVraHKaknpMFPgpg8jHYbGFo
  workers:
    - network_address: /ip6/2001:db8::50/udp/49594/quic-v1
      network_key: 4XTTM8bKFPoKfVL1UwN1vBMjCWCFVKbKpKUha12qr2Bi86ip4
    - network_address: /dns4/boot.example/udp/49594/quic-v1
      network_key: 4XTTM8bKFPoKfVL1UwN1vBMjCWCFVKbKpKUha12qr2Bi86ip4
";

/// A genesis `committee.yaml`: its only multiaddrs are the bootstrap servers.
const COMMITTEE: &str = "\
---
authorities:
  rZdhyVGJAtz7kyFtFARWVVcw5DB3o9E4GvPN6zuKfUhUBGFRvEK2PgEV44CJt9wu9GMyssFQBFXRknrenuhJixo9KCADrZzHdtcvAwAYDmcLc7Ugh9epGc5LRZC5TK2hnYA:
    protocol_key: rZdhyVGJAtz7kyFtFARWVVcw5DB3o9E4GvPN6zuKfUhUBGFRvEK2PgEV44CJt9wu9GMyssFQBFXRknrenuhJixo9KCADrZzHdtcvAwAYDmcLc7Ugh9epGc5LRZC5TK2hnYA
    execution_address: \"0x89dab9f6fdc569c1bcdbd6493f25b7040b55dc79\"
epoch: 0
bootstrap_servers:
  rZdhyVGJAtz7kyFtFARWVVcw5DB3o9E4GvPN6zuKfUhUBGFRvEK2PgEV44CJt9wu9GMyssFQBFXRknrenuhJixo9KCADrZzHdtcvAwAYDmcLc7Ugh9epGc5LRZC5TK2hnYA:
    primary:
      network_address: /ip4/192.0.2.60/udp/49590/quic-v1/p2p/12D3KooWBRH7XpYhxCPTrw364UnmaSYDR4hCotoTHxTWTjKfrcUm
      network_key: 4XTTM2jxmNy1nZZX7tbVWBWgbj3Bstjgew7Qh2AsGVxwCpPFs
    workers:
      - network_address: /ip4/192.0.2.61/udp/49594/quic-v1
        network_key: 4XTTM2jxmNy1nZZX7tbVWBWgbj3Bstjgew7Qh2AsGVxwCpPFs
num_workers: 1
";

#[test]
fn the_floor_comes_from_bootstrap_peers_or_from_genesis() -> TestResult {
    let mut bed = Bed::new("floor")?;
    let peers = bed.temp.join("bootstrap-peers.yaml");
    fs::write(&peers, BOOTSTRAP_PEERS)?;
    let ran = bed.run(&["capture-floor", &peers.to_string_lossy()])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        bed.read("floor")?.as_deref(),
        Some("# floor 1\n192.0.2.50\n2001:db8::50\n")
    );
    let snap = primary_only(42, vec![entry(0x11, &[ip4("203.0.113.10")])]);
    let ran = bed.sync_snapshot(&snap)?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=2 v6=1 static=0 floor=2 keys=1 dns_skipped=0 unparsed=0\n"
    );
    let expected = program(&[
        ("allow_v4", &["192.0.2.50", "203.0.113.10"]),
        ("allow_v6", &["2001:db8::50"]),
    ])?;
    assert_eq!(bed.read("current.nft")?, Some(expected));

    let committee = bed.temp.join("committee.yaml");
    fs::write(&committee, COMMITTEE)?;
    let ran = bed.run(&["capture-floor", &committee.to_string_lossy()])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        bed.read("floor")?.as_deref(),
        Some("# floor 1\n192.0.2.60\n192.0.2.61\n")
    );
    let ran = bed.sync_snapshot(&snap)?;
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=3 v6=0 static=0 floor=2 keys=1 dns_skipped=0 unparsed=0\n"
    );
    let expected = program(&[("allow_v4", &["192.0.2.60", "192.0.2.61", "203.0.113.10"])])?;
    assert_eq!(bed.read("current.nft")?, Some(expected.clone()));
    assert_eq!(bed.nft.table()?, Some(table_of(&expected)));
    Ok(())
}

#[test]
fn a_deleted_floor_is_an_empty_floor_with_one_warning_per_run() -> TestResult {
    let mut bed = Bed::new("floor-missing")?;
    let peers = bed.temp.join("bootstrap-peers.yaml");
    fs::write(&peers, BOOTSTRAP_PEERS)?;
    assert_eq!(
        bed.run(&["capture-floor", &peers.to_string_lossy()])?.code,
        Some(0)
    );
    let snap = primary_only(42, vec![entry(0x11, &[ip4("203.0.113.10")])]);
    let ran = bed.sync_snapshot(&snap)?;
    assert!(ran.stdout.contains(" floor=2 "), "{ran:?}");
    assert!(!ran.logged("floor-missing"), "{ran:?}");
    let warnings = |ran: &Ran| {
        ran.stderr
            .lines()
            .filter(|line| line.starts_with("floor-missing "))
            .count()
    };

    // the floor's addresses leave the table, and every run says why
    fs::remove_file(bed.file("floor"))?;
    for result in ["applied", "unchanged"] {
        let ran = bed.sync_snapshot(&snap)?;
        assert_eq!(ran.code, Some(0), "{ran:?}");
        assert_eq!(
            ran.stdout,
            format!(
                "result={result} reason= epoch=42 v4=1 v6=0 static=0 floor=0 keys=1 dns_skipped=0 unparsed=0\n"
            )
        );
        assert_eq!(warnings(&ran), 1, "{ran:?}");
        assert!(
            ran.logged(
                "floor-missing detail=\"no floor file; the floor is empty until capture-floor writes one\""
            ),
            "{ran:?}"
        );
    }
    bed.rpc.set(vec![fake_rpc::result(&snap.to_string())])?;
    let ran = bed.run(&["render"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(warnings(&ran), 1, "{ran:?}");

    // a floor file without addresses is an empty floor too, without the warning
    write_private(&bed.file("floor"), "# floor 1\n")?;
    let ran = bed.sync_snapshot(&snap)?;
    assert!(ran.stdout.starts_with("result=unchanged "), "{ran:?}");
    assert_eq!(warnings(&ran), 0, "{ran:?}");
    Ok(())
}

#[test]
fn the_floor_and_the_static_list_alone_never_build_a_table() -> TestResult {
    let mut bed = Bed::new("floor-alone")?;
    bed.config("198.51.100.0/24,192.0.2.99")?;
    let peers = bed.temp.join("bootstrap-peers.yaml");
    fs::write(&peers, BOOTSTRAP_PEERS)?;
    assert_eq!(
        bed.run(&["capture-floor", &peers.to_string_lossy()])?.code,
        Some(0)
    );

    // listed without addresses: memory holds nothing, so the dynamic set is empty
    let ran = bed.sync_snapshot(&primary_only(42, vec![unresolved(0x11)]))?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=stale reason=empty-set epoch= v4=0 v6=0 static=0 floor=0 keys=0 dns_skipped=0 unparsed=0\n"
    );
    assert!(ran.logged("stale reason=empty-set action=none"), "{ran:?}");
    assert_eq!(
        bed.nft.argv()?,
        ["-j list tables inet", "-j list tables inet"]
    );
    assert_eq!(bed.nft.table()?, None);
    assert_eq!(bed.names()?, ["config", "floor", "lock", "status"]);
    assert_eq!(bed.status_value("STALE_REASON")?, "empty-set");

    // with a program loaded, the key's departure empties the set and the program stays
    assert_eq!(
        bed.sync_snapshot(&primary_only(42, vec![entry(0x11, &[ip4("203.0.113.10")])]))?
            .code,
        Some(0)
    );
    let table = bed.nft.table()?;
    let current = bed.stamp("current.nft")?;
    let nobody = primary_only(42, Vec::new());
    assert!(
        bed.sync_snapshot(&nobody)?
            .stdout
            .starts_with("result=unchanged ")
    );
    let before = bed.calls()?;
    let ran = bed.sync_snapshot(&nobody)?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert!(
        ran.stdout
            .starts_with("result=stale reason=empty-set epoch=42 "),
        "{ran:?}"
    );
    assert!(ran.logged("stale reason=empty-set action=kept"), "{ran:?}");
    assert_eq!(
        bed.calls_since(before)?,
        ["-j list tables inet", "-j list tables inet"]
    );
    assert_eq!(bed.nft.table()?, table);
    assert_eq!(bed.stamp("current.nft")?, current);
    Ok(())
}

#[test]
fn cidr_entries_go_to_the_static_sets_masked_and_hosts_to_the_allow_sets() -> TestResult {
    let mut bed = Bed::new("cidrs")?;
    bed.config("203.0.113.7/24,2001:db8:100::1/48,198.51.100.9,10.0.0.0/8")?;
    let snap = primary_only(42, vec![entry(0x11, &[ip4("192.0.2.10")])]);
    let ran = bed.sync_snapshot(&snap)?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=2 v6=0 static=3 floor=0 keys=1 dns_skipped=0 unparsed=0\n"
    );
    let expected = program(&[
        ("allow_v4", &["192.0.2.10", "198.51.100.9"]),
        ("static_v4", &["10.0.0.0/8", "203.0.113.0/24"]),
        ("static_v6", &["2001:db8:100::/48"]),
    ])?;
    assert_eq!(bed.read("current.nft")?, Some(expected.clone()));
    assert_eq!(bed.nft.table()?, Some(table_of(&expected)));
    let status = bed.status()?;
    assert_eq!(status.get("STATIC_V4").map(String::as_str), Some("2"));
    assert_eq!(status.get("STATIC_V6").map(String::as_str), Some("1"));
    // the listing's prefixes compare as values, so the next run loads nothing
    assert!(
        bed.sync_snapshot(&snap)?
            .stdout
            .starts_with("result=unchanged ")
    );
    Ok(())
}

#[test]
fn a_worker_swarm_that_is_missing_or_unavailable_keeps_its_addresses() -> TestResult {
    let mut bed = Bed::new("worker")?;
    let main = || entry(0x11, &[ip4("192.0.2.10")]);
    let worker = |entries| swarm("worker-0", Some(0), 42, entries);
    let both = snapshot(
        42,
        vec![
            primary(42, vec![main()]),
            worker(vec![entry(0x11, &[ip4("192.0.2.20")])]),
        ],
    );
    let ran = bed.sync_snapshot(&both)?;
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=2 v6=0 static=0 floor=0 keys=1 dns_skipped=0 unparsed=0\n"
    );
    let unavailable = json!({
        "network": "worker-0",
        "worker_id": 0,
        "available": false,
        "tracked_epoch": null,
        "admission": null,
        "entries": [],
    });
    let missing = primary_only(42, vec![main()]);
    let timed_out = snapshot(42, vec![primary(42, vec![main()]), unavailable]);
    // additions-only: no strike however often the worker is absent
    for snap in [&missing, &timed_out, &missing, &timed_out] {
        let ran = bed.sync_snapshot(snap)?;
        assert_eq!(ran.code, Some(0), "{ran:?}");
        assert_eq!(
            ran.stdout,
            "result=unchanged reason= epoch=42 v4=2 v6=0 static=0 floor=0 keys=1 dns_skipped=0 unparsed=0\n"
        );
    }
    assert!(
        !bed.read("state.json")?
            .unwrap_or_default()
            .contains("\"absent\":1")
    );
    // an authoritative worker that stops listing the key removes it on the second run
    let left = snapshot(42, vec![primary(42, vec![main()]), worker(Vec::new())]);
    assert!(
        bed.sync_snapshot(&left)?
            .stdout
            .starts_with("result=unchanged ")
    );
    let ran = bed.sync_snapshot(&left)?;
    assert!(
        ran.stdout
            .starts_with("result=applied reason= epoch=42 v4=1 "),
        "{ran:?}"
    );
    Ok(())
}

#[test]
fn a_flushed_table_is_reloaded_from_current_nft_while_the_node_is_down() -> TestResult {
    let mut bed = Bed::new("reload")?;
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let table = bed.nft.table()?;
    let current = bed.stamp("current.nft")?;
    let state = bed.stamp("state.json")?;
    bed.nft.delete_table()?;

    let before = bed.calls()?;
    let ran = bed.sync(vec![Response::Down])?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=stale reason=rpc-unreachable epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    );
    assert!(
        ran.logged("stale reason=rpc-unreachable action=reloaded"),
        "{ran:?}"
    );
    assert_eq!(
        bed.calls_since(before)?,
        [
            "-j list tables inet".to_owned(),
            "-j list tables inet".to_owned(),
            bed.reload_current()
        ]
    );
    assert_eq!(bed.nft.table()?, table);
    assert_eq!(bed.stamp("current.nft")?, current);
    assert_eq!(bed.stamp("state.json")?, state);
    Ok(())
}

#[test]
fn no_current_nft_and_the_node_down_gives_no_table() -> TestResult {
    let mut bed = Bed::new("no-current")?;
    let ran = bed.sync(vec![Response::Down])?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=stale reason=rpc-unreachable epoch= v4=0 v6=0 static=0 floor=0 keys=0 dns_skipped=0 unparsed=0\n"
    );
    assert!(
        ran.logged("stale reason=rpc-unreachable action=none"),
        "{ran:?}"
    );
    assert_eq!(
        bed.nft.argv()?,
        ["-j list tables inet", "-j list tables inet"]
    );
    assert_eq!(bed.nft.table()?, None);
    assert_eq!(bed.names()?, ["config", "lock", "status"]);
    let status = bed.status()?;
    assert_eq!(status.get("RESULT").map(String::as_str), Some("stale"));
    assert_eq!(
        status.get("STALE_REASON").map(String::as_str),
        Some("rpc-unreachable")
    );
    assert!(
        status
            .get("STALE_SINCE")
            .is_some_and(|since| !since.is_empty())
    );
    assert_eq!(status.get("LAST_GOOD").map(String::as_str), Some(""));
    Ok(())
}

#[test]
fn a_table_deleted_while_the_node_is_asked_is_reloaded_by_the_stale_run() -> TestResult {
    let mut bed = Bed::new("gone-stale")?;
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let table = bed.nft.table()?;
    let current = bed.stamp("current.nft")?;
    let state = bed.stamp("state.json")?;
    let mut version_2 = two_keys();
    version_2
        .as_object_mut()
        .ok_or("not an object")?
        .insert("version".to_owned(), json!(2));

    // the run lists the tables before it asks the node, and the table goes while the node answers
    let gone = bed.nft.table_path();
    let before = bed.calls()?;
    let ran = bed.sync(vec![Response::Removing(
        gone,
        Box::new(fake_rpc::result(&version_2.to_string())),
    )])?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=stale reason=snapshot-version-2 epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    );
    assert!(
        ran.logged("stale reason=snapshot-version-2 action=reloaded"),
        "{ran:?}"
    );
    assert_eq!(
        bed.calls_since(before)?,
        [
            "-j list tables inet".to_owned(),
            "-j list tables inet".to_owned(),
            bed.reload_current()
        ]
    );
    assert_eq!(bed.nft.table()?, table);
    assert_eq!(bed.stamp("current.nft")?, current);
    assert_eq!(bed.stamp("state.json")?, state);
    Ok(())
}

#[test]
fn a_table_deleted_while_the_node_is_asked_is_loaded_again_when_the_program_is_unchanged()
-> TestResult {
    let mut bed = Bed::new("gone-unchanged")?;
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let table = bed.nft.table()?;

    // the tables were listed with the table present, so the unchanged program asks for the listing,
    // which fails on the missing table; the tables are listed again and the program loaded
    let gone = bed.nft.table_path();
    let before = bed.calls()?;
    let ran = bed.sync(vec![
        Response::Removing(gone, Box::new(fake_rpc::result(&two_keys().to_string()))),
        fake_rpc::result(EPOCH_INFO),
    ])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    );
    assert_eq!(
        bed.calls_since(before)?,
        [
            "-j list tables inet".to_owned(),
            "-j list table inet tn_p2p_allow".to_owned(),
            "-j list tables inet".to_owned(),
            bed.load_staged()
        ]
    );
    assert_eq!(bed.nft.table()?, table);
    assert_eq!(bed.status_value("RESULT")?, "applied");
    assert_eq!(bed.status_value("STALE_REASON")?, "");
    Ok(())
}

#[test]
fn a_hand_deleted_element_is_healed_by_the_next_authoritative_run() -> TestResult {
    let mut bed = Bed::new("heal")?;
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let table = bed.nft.table()?.ok_or("no table loaded")?;
    let current = bed.read("current.nft")?;
    let edited = table.replace("elem allow_v4 198.51.100.7\n", "");
    assert_ne!(edited, table);
    bed.nft.set_table(&edited)?;

    let before = bed.calls()?;
    let ran = bed.sync_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    );
    assert_eq!(
        bed.calls_since(before)?,
        [
            "-j list tables inet".to_owned(),
            "-j list table inet tn_p2p_allow".to_owned(),
            bed.load_staged(),
        ]
    );
    assert_eq!(bed.nft.table()?, Some(table));
    assert_eq!(bed.read("current.nft")?, current);
    assert!(
        bed.sync_snapshot(&two_keys())?
            .stdout
            .starts_with("result=unchanged ")
    );
    Ok(())
}

#[test]
fn a_rule_or_the_policy_changed_by_hand_is_healed_by_the_next_authoritative_run() -> TestResult {
    let mut bed = Bed::new("heal-rules")?;
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let table = bed.nft.table()?.ok_or("no table loaded")?;
    let current = bed.read("current.nft")?;
    // each edit keeps the five rules, the two counters and every element
    for (from, to) in [
        (
            "counter name \"tn_drop_p2p_unlisted\" drop\n",
            "counter name \"tn_drop_p2p_unlisted\" accept\n",
        ),
        (
            "chain input filter input -20 accept\n",
            "chain input filter input -20 drop\n",
        ),
        (
            "rule udp dport { 49590, 49594 } ip saddr @allow_v4 ",
            "rule udp dport { 49590 } ip saddr @allow_v4 ",
        ),
        (
            "rule udp dport { 49590, 49594 } ip6 saddr @static_v6 ",
            "rule udp dport { 49590, 49594 } ip6 saddr @allow_v6 ",
        ),
    ] {
        let edited = table.replace(from, to);
        assert_ne!(edited, table, "{from}");
        bed.nft.set_table(&edited)?;
        let before = bed.calls()?;
        let ran = bed.sync_snapshot(&two_keys())?;
        assert_eq!(ran.code, Some(0), "{to}: {ran:?}");
        assert_eq!(
            ran.stdout,
            "result=applied reason= epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n",
            "{to}"
        );
        assert_eq!(
            bed.calls_since(before)?,
            [
                "-j list tables inet".to_owned(),
                "-j list table inet tn_p2p_allow".to_owned(),
                bed.load_staged(),
            ],
            "{to}"
        );
        assert_eq!(bed.nft.table()?.as_ref(), Some(&table), "{to}");
        assert_eq!(bed.read("current.nft")?, current, "{to}");
    }
    assert!(
        bed.sync_snapshot(&two_keys())?
            .stdout
            .starts_with("result=unchanged ")
    );

    // one P2P port: nft lists it as a number rather than a set, and the next run is unchanged
    let config = format!(
        "# tn-p2p-allow-sync config, written by firewall-setup.sh\nCONFIG_VERSION=1\nRPC_PORT={}\nP2P_PORTS=49590\nP2P_ALLOW_FROM=\n",
        bed.rpc.port()
    );
    write_private(&bed.file("config"), &config)?;
    assert!(
        bed.sync_snapshot(&two_keys())?
            .stdout
            .starts_with("result=applied ")
    );
    let before = bed.calls()?;
    let ran = bed.sync_snapshot(&two_keys())?;
    assert!(ran.stdout.starts_with("result=unchanged "), "{ran:?}");
    assert_eq!(
        bed.calls_since(before)?,
        [
            "-j list tables inet".to_owned(),
            "-j list table inet tn_p2p_allow".to_owned(),
        ]
    );
    Ok(())
}

#[test]
fn a_held_lock_exits_busy_and_writes_nothing() -> TestResult {
    let mut bed = Bed::new("busy")?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(bed.file("lock"))?;
    lock.try_lock()?;
    let ran = bed.sync_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(4), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(ran.logged("busy "), "{ran:?}");
    assert_eq!(bed.names()?, ["config", "lock"]);
    assert_eq!(bed.nft.argv()?, Vec::<String>::new());
    assert_eq!(bed.rpc.requests(), Vec::<String>::new());
    drop(lock);
    // the lock is never deleted, and a free one lets the next run through; it waits, because a
    // sibling test's child holds a duplicate of `lock` from its fork until its exec
    assert_eq!(
        bed.sync_with(answers(&two_keys()), &["--wait", "5"])?.code,
        Some(0)
    );
    Ok(())
}

#[test]
fn require_authoritative_refuses_a_stale_snapshot_and_writes_nothing() -> TestResult {
    let mut bed = Bed::new("require")?;
    // before any good run: no status appears, and stdout carries the reason `on` maps
    let refusals = [
        (vec![fake_rpc::error(-32601)], "rpc-method-missing"),
        (
            answers(&primary_only(42, vec![unresolved(0x11)])),
            "empty-set",
        ),
    ];
    for (responses, reason) in refusals {
        let before = bed.calls()?;
        let ran = bed.sync_with(responses, &["--require-authoritative"])?;
        assert_eq!(ran.code, Some(1), "{reason}: {ran:?}");
        assert_eq!(
            ran.stdout,
            format!(
                "result=stale reason={reason} epoch= v4=0 v6=0 static=0 floor=0 keys=0 dns_skipped=0 unparsed=0\n"
            )
        );
        assert!(ran.logged(&format!("refused reason={reason}")), "{ran:?}");
        assert_eq!(bed.calls_since(before)?, ["-j list tables inet"]);
        assert_eq!(bed.names()?, ["config", "lock"], "{reason}");
    }

    // after one: status, current.nft and state.json stay as they were, and the table is not reloaded
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    bed.nft.delete_table()?;
    let stamps = ["status", "current.nft", "state.json"]
        .map(|name| bed.stamp(name).map(|stamp| (name, stamp)));
    let status = bed.read("status")?;
    let before = bed.calls()?;
    let ran = bed.sync_with(vec![Response::Down], &["--require-authoritative"])?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=stale reason=rpc-unreachable epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    );
    assert_eq!(bed.calls_since(before)?, ["-j list tables inet"]);
    assert_eq!(bed.nft.table()?, None);
    assert_eq!(bed.read("status")?, status);
    for stamp in stamps {
        let (name, stamp) = stamp?;
        assert_eq!(bed.stamp(name)?, stamp, "{name}");
    }
    // a fresh snapshot passes, loads the program and writes everything
    let ran = bed.sync_with(answers(&two_keys()), &["--require-authoritative"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert!(ran.stdout.starts_with("result=applied "), "{ran:?}");
    Ok(())
}

#[test]
fn a_failed_load_leaves_the_program_and_memory_untouched() -> TestResult {
    let mut bed = Bed::new("apply-failed")?;
    let one = primary_only(42, vec![entry(0x11, &[ip4("203.0.113.10")])]);
    assert_eq!(bed.sync_snapshot(&one)?.code, Some(0));
    let table = bed.nft.table()?;
    let current = bed.stamp("current.nft")?;
    let state = bed.stamp("state.json")?;
    let last_good = bed.status_value("LAST_GOOD")?;
    bed.nft.knob("fail-next")?;

    let before = bed.calls()?;
    let ran = bed.sync_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=error reason=nft-apply-failed epoch=42 v4=1 v6=0 static=0 floor=0 keys=1 dns_skipped=0 unparsed=0\n"
    );
    assert!(
        ran.logged("error reason=nft-apply-failed action=kept detail=\"nft exited with status 1: "),
        "{ran:?}"
    );
    // the failed load lists the tables again before the stale path keeps the table
    assert_eq!(
        bed.calls_since(before)?,
        [
            "-j list tables inet".to_owned(),
            bed.load_staged(),
            "-j list tables inet".to_owned()
        ]
    );
    let two = program(&[("allow_v4", &["198.51.100.7", "203.0.113.10"])])?;
    assert_eq!(bed.nft.loaded()?.last(), Some(&two));
    assert_eq!(bed.nft.table()?, table);
    assert_eq!(bed.stamp("current.nft")?, current);
    assert_eq!(bed.stamp("state.json")?, state);
    assert_eq!(
        bed.names()?,
        ["config", "current.nft", "lock", "state.json", "status"]
    );
    let status = bed.status()?;
    assert_eq!(status.get("RESULT").map(String::as_str), Some("error"));
    assert_eq!(
        status.get("STALE_REASON").map(String::as_str),
        Some("nft-apply-failed")
    );
    assert!(
        status
            .get("STALE_SINCE")
            .is_some_and(|since| !since.is_empty())
    );
    assert_eq!(status.get("LAST_GOOD"), Some(&last_good));

    let ran = bed.sync_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert!(
        ran.stdout
            .starts_with("result=applied reason= epoch=42 v4=2 "),
        "{ran:?}"
    );
    assert_eq!(bed.read("current.nft")?, Some(two));
    assert_eq!(bed.status_value("STALE_SINCE")?, "");
    Ok(())
}

#[test]
fn render_prints_the_program_and_writes_nothing() -> TestResult {
    let mut bed = Bed::new("render")?;
    let expected = program(&[("allow_v4", &["198.51.100.7", "203.0.113.10"])])?;
    bed.rpc
        .set(vec![fake_rpc::result(&two_keys().to_string())])?;
    let ran = bed.run(&["render"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, expected);
    assert!(
        ran.logged(
            "result=rendered reason= epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0"
        ),
        "{ran:?}"
    );
    assert_eq!(bed.rpc.methods(), ["tn_admissionSnapshot"]);
    assert_eq!(bed.nft.argv()?, Vec::<String>::new());
    assert_eq!(bed.names()?, ["config"]);

    // from a file, a bare result or a whole response, after a sync: still nothing written
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let stamps = bed
        .names()?
        .into_iter()
        .map(|name| Ok((bed.stamp(&name)?, name)))
        .collect::<Res<Vec<_>>>()?;
    let calls = bed.calls()?;
    let requests = bed.rpc.requests().len();
    let file = bed.temp.join("snapshot.json");
    for text in [
        two_keys().to_string(),
        json!({"jsonrpc": "2.0", "id": 1, "result": two_keys()}).to_string(),
    ] {
        fs::write(&file, text)?;
        let ran = bed.run(&["render", "--snapshot", &file.to_string_lossy()])?;
        assert_eq!(ran.code, Some(0), "{ran:?}");
        assert_eq!(ran.stdout, expected);
    }
    let after = bed
        .names()?
        .into_iter()
        .map(|name| Ok((bed.stamp(&name)?, name)))
        .collect::<Res<Vec<_>>>()?;
    assert_eq!(after, stamps);
    assert_eq!(bed.calls()?, calls);
    assert_eq!(bed.rpc.requests().len(), requests);

    // a stale snapshot prints no program
    bed.rpc.set(vec![Response::Down])?;
    let ran = bed.run(&["render"])?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(
        ran.logged("result=stale reason=rpc-unreachable "),
        "{ran:?}"
    );
    Ok(())
}

#[test]
fn render_refuses_an_empty_set_with_the_epoch_and_counts_of_state_json() -> TestResult {
    let mut bed = Bed::new("render-empty")?;
    let render = |bed: &mut Bed, snap: &Value| -> Res<Ran> {
        bed.rpc.set(vec![fake_rpc::result(&snap.to_string())])?;
        bed.run(&["render"])
    };
    // no state.json: epoch 0 and nothing counted, whatever epoch the snapshot names
    let ran = render(&mut bed, &primary_only(42, vec![unresolved(0x11)]))?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(
        ran.logged(
            "result=stale reason=empty-set epoch=0 v4=0 v6=0 static=0 floor=0 keys=0 dns_skipped=0 unparsed=0"
        ),
        "{ran:?}"
    );

    // two keys stored at epoch 42 that expire by epoch 50: the summary is still memory as stored
    assert_eq!(bed.sync_snapshot(&two_keys())?.code, Some(0));
    let ran = render(&mut bed, &primary_only(50, Vec::new()))?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(
        ran.logged(
            "result=stale reason=empty-set epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0"
        ),
        "{ran:?}"
    );
    Ok(())
}

#[test]
fn render_check_has_nft_check_the_program_and_loads_nothing() -> TestResult {
    let mut bed = Bed::new("render-check")?;
    bed.rpc
        .set(vec![fake_rpc::result(&two_keys().to_string())])?;
    let ran = bed.run(&["render", "--check"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    let expected = program(&[("allow_v4", &["198.51.100.7", "203.0.113.10"])])?;
    assert_eq!(ran.stdout, expected);
    assert_eq!(bed.nft.argv()?, ["-j list tables inet", "-c -f -"]);
    assert_eq!(bed.nft.loaded()?, vec![expected]);
    assert_eq!(bed.nft.table()?, None);
    assert_eq!(bed.names()?, ["config"]);
    Ok(())
}

#[test]
fn a_hostname_in_the_static_list_is_config_invalid() -> TestResult {
    let mut bed = Bed::new("hostname")?;
    bed.config("203.0.113.0/24,validator.example")?;
    let ran = bed.sync_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=error reason=config-invalid epoch= v4=0 v6=0 static=0 floor=0 keys=0 dns_skipped=0 unparsed=0\n"
    );
    assert!(
        ran.logged("error reason=config-invalid action=none detail="),
        "{ran:?}"
    );
    assert!(ran.stderr.contains("validator.example"), "{ran:?}");
    // the node is never asked; the stale path only looks for a table to protect
    assert_eq!(bed.rpc.requests(), Vec::<String>::new());
    assert_eq!(bed.nft.argv()?, ["-j list tables inet"]);
    assert_eq!(bed.names()?, ["config", "lock", "status"]);
    let status = bed.status()?;
    assert_eq!(status.get("RESULT").map(String::as_str), Some("error"));
    assert_eq!(
        status.get("STALE_REASON").map(String::as_str),
        Some("config-invalid")
    );
    Ok(())
}

#[test]
fn a_catch_all_static_entry_is_loaded_with_a_warning() -> TestResult {
    let mut bed = Bed::new("catch-all")?;
    bed.config("0.0.0.0/0,::/0")?;
    let snap = primary_only(42, vec![entry(0x11, &[ip4("192.0.2.10")])]);
    let ran = bed.sync_snapshot(&snap)?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=applied reason= epoch=42 v4=1 v6=0 static=2 floor=0 keys=1 dns_skipped=0 unparsed=0\n"
    );
    assert!(
        ran.logged("static-catch-all detail=\"P2P_ALLOW_FROM admits every source\""),
        "{ran:?}"
    );
    let expected = program(&[
        ("allow_v4", &["192.0.2.10"]),
        ("static_v4", &["0.0.0.0/0"]),
        ("static_v6", &["::/0"]),
    ])?;
    assert_eq!(bed.read("current.nft")?, Some(expected));
    // warned on every run, not only the one that loads
    let ran = bed.sync_snapshot(&snap)?;
    assert!(ran.stdout.starts_with("result=unchanged "), "{ran:?}");
    assert!(ran.logged("static-catch-all "), "{ran:?}");
    Ok(())
}

#[test]
fn stderr_never_exceeds_eight_lines() -> TestResult {
    let mut bed = Bed::new("log-cap")?;
    // the static list warns twice: a catch-all, and the two networks it covers
    bed.config("0.0.0.0/0,10.0.0.0/8,10.3.0.0/16")?;
    let long: Vec<String> = (1..=33_u8).map(|n| ip4(&format!("192.0.2.{n}"))).collect();
    let mut odd = entry(
        0x11,
        &[
            ip4("198.51.100.1"),
            "/dns4/validator.example/udp/49590/quic-v1".to_owned(),
            "not-a-multiaddr".to_owned(),
        ],
    );
    odd.as_object_mut()
        .ok_or("not an object")?
        .insert("slots".to_owned(), json!(["current", "observer"]));
    let snap = primary_only(
        42,
        vec![
            odd,
            entry(0x22, &long),
            entry(0x22, &[ip4("198.51.100.2")]),
            json!({
                "bls_pubkey": format!("0x{:02X}{}", 0xab, "0".repeat(190)),
                "slots": ["eligible"],
                "multiaddrs": [ip4("198.51.100.3")],
                "source": "record",
                "admitted": true,
            }),
        ],
    );
    // and the epoch info fails, one more warning
    let ran = bed.sync(vec![
        fake_rpc::result(&snap.to_string()),
        fake_rpc::error(-32601),
    ])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert!(ran.stdout.starts_with("result=applied "), "{ran:?}");
    for warning in [
        "nft-path ",
        "floor-missing ",
        "static-catch-all ",
        "static-covered dropped=2",
        "absorbed kind=unknown-slots count=1",
        "absorbed kind=uppercase-keys count=1",
    ] {
        assert!(ran.logged(warning), "{warning}: {ran:?}");
    }
    // eight or more lines were logged: six are written and the seventh says how many were dropped;
    // the eighth slot is the closing line's, and a run that applies has none
    let lines: Vec<&str> = ran.stderr.lines().collect();
    assert_eq!(lines.len(), 7, "{ran:?}");
    assert!(
        lines
            .last()
            .is_some_and(|last| last.starts_with("log-capped dropped=")),
        "{ran:?}"
    );
    Ok(())
}

#[test]
fn nft_refusing_the_caller_exits_2_with_run_as_root_and_writes_nothing() -> TestResult {
    let mut bed = Bed::new("eperm")?;
    bed.nft.knob("eperm")?;
    let ran = bed.sync_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(
        ran.logged(
            "error reason=nft-unavailable detail=\"nft: operation not permitted, run as root\""
        ),
        "{ran:?}"
    );
    assert_eq!(bed.names()?, ["config", "lock"]);
    assert_eq!(bed.rpc.requests(), Vec::<String>::new());
    let ran = bed.run(&["render", "--check"])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(ran.logged("error reason=nft-unavailable "), "{ran:?}");
    Ok(())
}
