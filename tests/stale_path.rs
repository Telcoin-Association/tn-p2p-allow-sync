//! Integration tests of the stale path and of the local errors that take it, through the built binary.
//!
//! Each test gets its own directory holding the state dir, mode 0700, and a link to `tests/support/fake-nft.sh`.
//! The fake's knobs make nft fail where a test needs it: a reload, a table listing, the stale path's re-listing, an unreadable or a dormant listing.
//! Every test checks the exit code, the stdout line, the log lines that matter with the closing line last, the `status` keys, the files the contract says stay untouched, and the nft calls the fake recorded.

// tests build fixtures with std::fs directly
#![allow(clippy::disallowed_methods)]

mod support;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use serde_json::{Value, json};

use support::fake_rpc::{self, FakeRpc, Response};
use support::{FakeNft, TempDir, read_optional};

type TestResult = Result<(), Box<dyn Error>>;
type Res<T> = Result<T, Box<dyn Error>>;

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_tn-p2p-allow-sync");

/// The epoch info every good run asks for after the snapshot.
const EPOCH_INFO: &str = r#"{"epochId":42,"epochDuration":86400}"#;

/// A snapshot whose primary has not published its pool: `pool-unpublished`.
const POOL_UNPUBLISHED: &str = include_str!("fixtures/snapshot/pool-unpublished.json");

/// The listing of the tables, the first nft call of every run.
const TABLES: &str = "-j list tables inet";

/// The listing of the table, made only when the program is unchanged.
const TABLE: &str = "-j list table inet tn_p2p_allow";

/// The counts on the stdout line of every run after the good run of [`two_keys`].
const COUNTS: &str = "epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0";

/// What every file holds after the good run of [`two_keys`].
const AFTER_GOOD: [&str; 6] = [
    "config",
    "current.nft",
    "floor",
    "lock",
    "state.json",
    "status",
];

/// What one run of the binary did.
#[derive(Debug)]
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ran {
    /// The log lines, without the `nft-path` warning every run with `--nft` starts with.
    fn events(&self) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| !line.starts_with("nft-path path="))
            .collect()
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
    /// A state dir holding a config with the default ports and no static entries, and an empty floor, so a good run logs nothing of its own.
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
        write_private(&bed.file("floor"), "# floor 1\n")?;
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

    /// Queues `responses` on the node, then runs the binary with `args`.
    fn run_with(&mut self, responses: Vec<Response>, args: &[&str]) -> Res<Ran> {
        self.rpc.set(responses)?;
        self.run(args)
    }

    /// Queues `responses` on the node, then runs a plain `sync`.
    fn sync(&mut self, responses: Vec<Response>) -> Res<Ran> {
        self.run_with(responses, &["sync"])
    }

    /// The good run every test starts from: [`two_keys`] applied, quietly.
    fn good(&mut self) -> TestResult {
        let ran = self.sync(answers(&two_keys()))?;
        assert_eq!(ran.code, Some(0), "{ran:?}");
        assert_eq!(ran.stdout, format!("result=applied reason= {COUNTS}\n"));
        assert_eq!(ran.events(), Vec::<&str>::new(), "{ran:?}");
        assert_eq!(self.names()?, AFTER_GOOD);
        Ok(())
    }

    fn file(&self, name: &str) -> PathBuf {
        self.state.join(name)
    }

    fn read(&self, name: &str) -> Res<Option<String>> {
        Ok(read_optional(&self.file(name))?)
    }

    /// The `status` file as its lines split at the first `=`, in file order; empty when it does not exist.
    fn status_lines(&self) -> Res<Vec<(String, String)>> {
        Ok(self
            .read("status")?
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect())
    }

    /// The `status` file as a map; empty when it does not exist.
    fn status(&self) -> Res<BTreeMap<String, String>> {
        Ok(self.status_lines()?.into_iter().collect())
    }

    /// One value of `status`, empty when the key or the file is missing.
    fn status_value(&self, key: &str) -> Res<String> {
        Ok(self.status()?.remove(key).unwrap_or_default())
    }

    /// Replaces the value of `key` in `status`, as a past run would have left it.
    fn edit_status(&self, key: &str, value: &str) -> TestResult {
        let text = self.read("status")?.ok_or("no status")?;
        let edited = text
            .lines()
            .map(|line| match line.split_once('=') {
                Some((name, _)) if name == key => format!("{key}={value}\n"),
                _ => format!("{line}\n"),
            })
            .collect::<String>();
        assert_ne!(edited, text, "{key} is not in status");
        write_private(&self.file("status"), &edited)
    }

    /// The stamp of `name` itself, a symlink included; `None` when it does not exist.
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

    /// The names in the state dir, sorted.
    fn names(&self) -> Res<Vec<String>> {
        let mut names = fs::read_dir(&self.state)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Res<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    /// How many nft calls were made so far.
    fn calls(&self) -> Res<usize> {
        Ok(self.nft.argv()?.len())
    }

    /// The nft calls made after the first `since`.
    fn calls_since(&self, since: usize) -> Res<Vec<String>> {
        Ok(self.nft.argv()?.into_iter().skip(since).collect())
    }

    /// The `-f` call that loads the staged program.
    fn load_staged(&self) -> String {
        format!("-f {}", self.file(".current.nft.new").display())
    }

    /// The `-f` call that reloads `current.nft`.
    fn reload_current(&self) -> String {
        format!("-f {}", self.file("current.nft").display())
    }

    /// The methods the node was asked for after the first `since` calls.
    fn methods_since(&self, since: usize) -> Vec<String> {
        self.rpc.methods().into_iter().skip(since).collect()
    }
}

/// What a run must leave as it was: the program, memory and the live table.
#[derive(Debug, PartialEq, Eq)]
struct Kept {
    current: Option<(Stamp, String)>,
    state: Option<(Stamp, String)>,
    table: Option<String>,
}

impl Kept {
    /// `current.nft` and `state.json` with their bytes, and the fake's live table, as they are now.
    fn now(bed: &Bed) -> Res<Self> {
        let file = |name: &str| -> Res<Option<(Stamp, String)>> {
            Ok(match bed.stamp(name)? {
                Some(stamp) => Some((stamp, fs::read_to_string(bed.file(name))?)),
                None => None,
            })
        };
        Ok(Self {
            current: file("current.nft")?,
            state: file("state.json")?,
            table: bed.nft.table()?,
        })
    }
}

fn write_private(path: &Path, text: &str) -> TestResult {
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

/// A snapshot whose available primary, its pool published for `epoch`, lists `entries`.
fn primary_only(epoch: u64, entries: Vec<Value>) -> Value {
    json!({
        "version": 1,
        "epoch": epoch,
        "swarms": [{
            "network": "primary",
            "worker_id": null,
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
        }],
    })
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

#[test]
fn a_failed_reload_overrides_the_reason_with_reload_failed() -> TestResult {
    let mut bed = Bed::new("reload-failed")?;
    bed.good()?;
    let last_good = bed.status_value("LAST_GOOD")?;
    let program = bed.read("current.nft")?.ok_or("no current.nft")?;
    bed.nft.delete_table()?;
    let kept = Kept::now(&bed)?;

    // a node that is down, and one that answers with a pool it has not published
    let cases = [
        (vec![Response::Down], "rpc-unreachable"),
        (vec![fake_rpc::result(POOL_UNPUBLISHED)], "pool-unpublished"),
    ];
    for (responses, reason) in cases {
        bed.nft.knob("fail-next")?;
        let before = bed.calls()?;
        let ran = bed.sync(responses)?;
        assert_eq!(ran.code, Some(2), "{reason}: {ran:?}");
        assert_eq!(
            ran.stdout,
            format!("result=error reason=reload-failed {COUNTS}\n"),
            "{reason}"
        );
        // the original reason stays in the first line, which is an ordinary line; the failed
        // reload closes the run
        let events = ran.events();
        assert_eq!(events.len(), 2, "{reason}: {ran:?}");
        assert_eq!(
            events.first().copied(),
            Some(format!("stale reason={reason} action=reload-failed").as_str()),
            "{reason}"
        );
        let closing = events.last().copied().unwrap_or_default();
        assert!(
            closing.starts_with("error reason=reload-failed detail=\"nft exited with status 1: ")
                && closing.ends_with("Error: simulated failure\""),
            "{reason}: {closing}"
        );
        // the tables are listed again before the reload, which nft refused
        assert_eq!(
            bed.calls_since(before)?,
            [TABLES.to_owned(), TABLES.to_owned(), bed.reload_current()],
            "{reason}"
        );
        assert!(!bed.nft.has_knob("fail-next"), "{reason}");
        assert_eq!(bed.nft.loaded()?.last(), Some(&program), "{reason}");
        assert_eq!(Kept::now(&bed)?, kept, "{reason}");
        assert_eq!(bed.names()?, AFTER_GOOD, "{reason}");
        let status = bed.status()?;
        assert_eq!(status.get("RESULT").map(String::as_str), Some("error"));
        assert_eq!(
            status.get("STALE_REASON").map(String::as_str),
            Some("reload-failed"),
            "{reason}"
        );
        assert!(
            status
                .get("STALE_SINCE")
                .is_some_and(|since| !since.is_empty()),
            "{reason}"
        );
        assert_eq!(status.get("LAST_GOOD"), Some(&last_good), "{reason}");
        assert_eq!(status.get("EPOCH").map(String::as_str), Some("42"));
    }

    // nft taking the reload again ends the failure: the original reason, exit 3
    let before = bed.calls()?;
    let ran = bed.sync(vec![Response::Down])?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        format!("result=stale reason=rpc-unreachable {COUNTS}\n")
    );
    assert_eq!(
        ran.events(),
        ["stale reason=rpc-unreachable action=reloaded"]
    );
    assert_eq!(
        bed.calls_since(before)?,
        [TABLES.to_owned(), TABLES.to_owned(), bed.reload_current()]
    );
    assert!(bed.nft.table()?.is_some());
    assert_eq!(bed.status_value("RESULT")?, "stale");
    assert_eq!(bed.status_value("STALE_REASON")?, "rpc-unreachable");
    Ok(())
}

#[test]
fn local_errors_take_the_stale_path_and_reload_an_absent_table() -> TestResult {
    let mut bed = Bed::new("local-errors")?;
    bed.good()?;
    let table = bed.nft.table()?;
    let last_good = bed.status_value("LAST_GOOD")?;
    let good_state = bed.read("state.json")?.ok_or("no state.json")?;
    // the good memory, kept outside the state dir for the symlink to point at
    let outside = bed.temp.join("state.json");
    write_private(&outside, &good_state)?;
    // valid JSON with the same content, but not the bytes the tool writes
    let spaced = good_state.replacen('{', "{ ", 1);
    assert_ne!(spaced, good_state);

    type Break = fn(&Bed, &Path, &str) -> TestResult;
    type Mend = fn(&Bed, &str) -> TestResult;
    let cases: [(&str, &str, Break, Mend); 3] = [
        (
            "floor-invalid",
            "the first line is not",
            |bed, _, _| write_private(&bed.file("floor"), "# floor 2\n192.0.2.50\n"),
            |bed, _| write_private(&bed.file("floor"), "# floor 1\n"),
        ),
        (
            "state-invalid",
            "state.json is not in the form this tool writes; recover with off, then on",
            |bed, _, spaced| write_private(&bed.file("state.json"), spaced),
            |bed, good| write_private(&bed.file("state.json"), good),
        ),
        (
            "state-io-failed",
            "state.json\\\": cannot read: is a symlink",
            |bed, outside, _| {
                fs::remove_file(bed.file("state.json"))?;
                std::os::unix::fs::symlink(outside, bed.file("state.json"))?;
                Ok(())
            },
            |bed, good| {
                fs::remove_file(bed.file("state.json"))?;
                write_private(&bed.file("state.json"), good)
            },
        ),
    ];
    for (reason, detail, breaks, mends) in cases {
        bed.nft.delete_table()?;
        breaks(&bed, &outside, &spaced)?;
        let kept = Kept::now(&bed)?;
        let asked = bed.rpc.requests().len();
        let before = bed.calls()?;
        let ran = bed.sync(answers(&two_keys()))?;
        assert_eq!(ran.code, Some(2), "{reason}: {ran:?}");
        assert_eq!(
            ran.stdout,
            format!("result=error reason={reason} {COUNTS}\n"),
            "{reason}"
        );
        let events = ran.events();
        assert_eq!(events.len(), 1, "{reason}: {ran:?}");
        let closing = events.last().copied().unwrap_or_default();
        assert!(
            closing.starts_with(&format!("error reason={reason} action=reloaded detail=")),
            "{reason}: {closing}"
        );
        assert!(closing.contains(detail), "{reason}: {closing}");
        // the inputs are read before nft is asked anything, so the stale path's listing is the
        // run's only one, and the node is never asked
        assert_eq!(
            bed.calls_since(before)?,
            [TABLES.to_owned(), bed.reload_current()],
            "{reason}"
        );
        assert_eq!(bed.rpc.requests().len(), asked, "{reason}");
        assert_eq!(bed.nft.table()?, table, "{reason}");
        // the broken input stays as it is: the run writes neither memory nor the program
        assert_eq!(
            Kept {
                table: table.clone(),
                ..kept
            },
            Kept::now(&bed)?,
            "{reason}"
        );
        assert_eq!(bed.names()?, AFTER_GOOD, "{reason}");
        let status = bed.status()?;
        assert_eq!(
            status.get("RESULT").map(String::as_str),
            Some("error"),
            "{reason}"
        );
        assert_eq!(status.get("STALE_REASON").map(String::as_str), Some(reason));
        assert!(
            status
                .get("STALE_SINCE")
                .is_some_and(|since| !since.is_empty()),
            "{reason}"
        );
        assert_eq!(status.get("LAST_GOOD"), Some(&last_good), "{reason}");
        assert_eq!(status.get("EPOCH").map(String::as_str), Some("42"));
        mends(&bed, &good_state)?;
    }

    // with every input mended, the next run finds the program loaded
    let ran = bed.sync(answers(&two_keys()))?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, format!("result=unchanged reason= {COUNTS}\n"));
    assert_eq!(bed.status_value("STALE_SINCE")?, "");
    Ok(())
}

#[test]
fn require_authoritative_still_takes_the_stale_path_on_a_local_error() -> TestResult {
    let mut bed = Bed::new("require-local")?;
    bed.good()?;
    let table = bed.nft.table()?;
    let last_good = bed.status_value("LAST_GOOD")?;

    // a local error before the node is asked: exit 2, not the refusal's 1, and the absent table
    // is reloaded
    bed.nft.delete_table()?;
    bed.config("203.0.113.0/24,validator.example")?;
    let kept = Kept::now(&bed)?;
    let status = bed.stamp("status")?;
    let asked = bed.rpc.requests().len();
    let before = bed.calls()?;
    let ran = bed.run_with(answers(&two_keys()), &["sync", "--require-authoritative"])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(
        ran.stdout,
        format!("result=error reason=config-invalid {COUNTS}\n")
    );
    let events = ran.events();
    assert_eq!(events.len(), 1, "{ran:?}");
    let closing = events.last().copied().unwrap_or_default();
    assert!(
        closing.starts_with("error reason=config-invalid action=reloaded detail=")
            && closing.contains("validator.example"),
        "{closing}"
    );
    assert_eq!(
        bed.calls_since(before)?,
        [TABLES.to_owned(), bed.reload_current()]
    );
    assert_eq!(bed.rpc.requests().len(), asked);
    assert_eq!(
        Kept {
            table: table.clone(),
            ..kept
        },
        Kept::now(&bed)?
    );
    assert_ne!(bed.stamp("status")?, status);
    assert_eq!(bed.status_value("RESULT")?, "error");
    assert_eq!(bed.status_value("STALE_REASON")?, "config-invalid");
    assert_eq!(bed.status_value("LAST_GOOD")?, last_good);

    // a load nft refuses is a local error too: exit 2 with the present table kept
    bed.config("")?;
    bed.nft.knob("fail-next")?;
    let three = primary_only(
        42,
        vec![
            entry(0x11, &[ip4("203.0.113.10")]),
            entry(0x22, &[ip4("198.51.100.7")]),
            entry(0x33, &[ip4("192.0.2.10")]),
        ],
    );
    let kept = Kept::now(&bed)?;
    let status = bed.stamp("status")?;
    let before = bed.calls()?;
    let ran = bed.run_with(answers(&three), &["sync", "--require-authoritative"])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(
        ran.stdout,
        format!("result=error reason=nft-apply-failed {COUNTS}\n")
    );
    let events = ran.events();
    assert_eq!(events.len(), 1, "{ran:?}");
    assert!(
        events.last().is_some_and(|line| line.starts_with(
            "error reason=nft-apply-failed action=kept detail=\"nft exited with status 1: "
        )),
        "{ran:?}"
    );
    assert_eq!(
        bed.calls_since(before)?,
        [TABLES.to_owned(), bed.load_staged(), TABLES.to_owned()]
    );
    assert_eq!(Kept::now(&bed)?, kept);
    assert_ne!(bed.stamp("status")?, status);
    assert_eq!(bed.status_value("RESULT")?, "error");
    assert_eq!(bed.status_value("STALE_REASON")?, "nft-apply-failed");
    assert_eq!(bed.status_value("LAST_GOOD")?, last_good);
    Ok(())
}

#[test]
fn a_stale_run_changes_only_the_status_keys_the_contract_names() -> TestResult {
    /// The keys whose values differ between `before` and `after`, in file order; both must hold the same keys in the same order.
    fn changed(before: &[(String, String)], after: &[(String, String)]) -> Vec<String> {
        let names = |lines: &[(String, String)]| -> Vec<String> {
            lines.iter().map(|(key, _)| key.clone()).collect()
        };
        assert_eq!(names(before), names(after));
        before
            .iter()
            .zip(after)
            .filter(|((_, old), (_, new))| old != new)
            .map(|((key, _), _)| key.clone())
            .collect()
    }

    let mut bed = Bed::new("status-diff")?;
    bed.good()?;
    assert_eq!(bed.status_lines()?.len(), 18);
    // values an older build and an earlier run left, so the stale run visibly rewrites them
    bed.edit_status("TOOL_VERSION", "0.0.1")?;
    bed.edit_status("LAST_RUN", "1000")?;
    let before = bed.status_lines()?;
    let state = bed.stamp("state.json")?;

    let ran = bed.sync(vec![Response::Down])?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        format!("result=stale reason=rpc-unreachable {COUNTS}\n")
    );
    let after = bed.status_lines()?;
    // docs/contract.md, The status file: a stale or failed run changes only TOOL_VERSION, RESULT,
    // LAST_RUN, STALE_SINCE (when empty), STALE_REASON and EPOCH_DURATION (when learned in that
    // run); the file has no STALE key, which only `status` computes
    assert_eq!(
        changed(&before, &after),
        [
            "TOOL_VERSION",
            "RESULT",
            "LAST_RUN",
            "STALE_SINCE",
            "STALE_REASON"
        ]
    );
    let status: BTreeMap<_, _> = after.into_iter().collect();
    let value = |key: &str| status.get(key).map(String::as_str);
    assert_eq!(value("TOOL_VERSION"), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(value("RESULT"), Some("stale"));
    assert_eq!(value("STALE_REASON"), Some("rpc-unreachable"));
    assert_ne!(value("LAST_RUN"), Some("1000"));
    // the series began with this run
    assert_eq!(value("STALE_SINCE"), value("LAST_RUN"));
    assert_eq!(bed.stamp("state.json")?, state);

    // an empty set learns the epoch duration before it goes stale; STALE_SINCE is set already
    bed.edit_status("TOOL_VERSION", "0.0.1")?;
    bed.edit_status("LAST_RUN", "1000")?;
    let before = bed.status_lines()?;
    // three epochs on, the two keys are listed without addresses, so memory forgets both
    let empty = primary_only(45, vec![unresolved(0x11), unresolved(0x22)]);
    let ran = bed.sync(vec![
        fake_rpc::result(&empty.to_string()),
        fake_rpc::result(r#"{"epochId":45,"epochDuration":7200}"#),
    ])?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        format!("result=stale reason=empty-set {COUNTS}\n")
    );
    let after = bed.status_lines()?;
    assert_eq!(
        changed(&before, &after),
        ["TOOL_VERSION", "LAST_RUN", "EPOCH_DURATION", "STALE_REASON"]
    );
    let status: BTreeMap<_, _> = after.into_iter().collect();
    let value = |key: &str| status.get(key).map(String::as_str);
    assert_eq!(value("EPOCH_DURATION"), Some("7200"));
    assert_eq!(value("STALE_REASON"), Some("empty-set"));
    assert_eq!(value("EPOCH"), Some("42"));
    assert_eq!(bed.stamp("state.json")?, state);
    Ok(())
}

#[test]
fn a_failing_table_listing_with_the_table_present_is_nft_unavailable_and_touches_nothing()
-> TestResult {
    let mut bed = Bed::new("list-fails")?;
    bed.good()?;
    let kept = Kept::now(&bed)?;
    let status = (bed.stamp("status")?, bed.read("status")?);
    let loads = bed.nft.loaded()?.len();
    bed.nft.knob("list-fails")?;

    // the program is unchanged, so the run lists the table; that fails while the tables still
    // list it, so nft is broken rather than the table gone
    let asked = bed.rpc.methods().len();
    let before = bed.calls()?;
    let ran = bed.sync(answers(&two_keys()))?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    // docs/contract.md, The sync line: no line is printed on nft-unavailable
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.events(),
        [
            "error reason=nft-unavailable detail=\"nft exited with status 1: netlink: Error: cache initialization failed: No buffer space available\""
        ]
    );
    assert_eq!(bed.calls_since(before)?, [TABLES, TABLE, TABLES]);
    assert_eq!(
        bed.methods_since(asked),
        ["tn_admissionSnapshot", "tn_getCurrentEpochInfo"]
    );
    assert_eq!(bed.nft.loaded()?.len(), loads);
    assert_eq!(Kept::now(&bed)?, kept);
    assert_eq!((bed.stamp("status")?, bed.read("status")?), status);
    assert_eq!(bed.names()?, AFTER_GOOD);

    // a listing that works again finds the program loaded
    bed.nft.clear_knob("list-fails")?;
    let ran = bed.sync(answers(&two_keys()))?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, format!("result=unchanged reason= {COUNTS}\n"));
    Ok(())
}

#[test]
fn an_unreadable_listing_re_applies_the_program_with_a_warning() -> TestResult {
    let mut bed = Bed::new("list-garbage")?;
    bed.good()?;
    let program = bed.read("current.nft")?.ok_or("no current.nft")?;
    let table = bed.nft.table()?;
    bed.nft.knob("list-garbage")?;

    let before = bed.calls()?;
    let ran = bed.sync(answers(&two_keys()))?;
    // CONTRACT MISMATCH: docs/contract.md, How nft is run, says "Any failure of either listing
    // call is nft-unavailable, exit 2, nothing touched" with one exception (a table gone), and its
    // Log lines event list has no `listing-unreadable`. Observed (contract-notes T4b): a listing
    // nft printed but the parser cannot read is the warning `listing-unreadable detail=…`, and the
    // program is applied again, exit 0.
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, format!("result=applied reason= {COUNTS}\n"));
    let events = ran.events();
    assert_eq!(events.len(), 1, "{ran:?}");
    assert!(
        events
            .first()
            .is_some_and(|line| line.starts_with("listing-unreadable detail=")),
        "{ran:?}"
    );
    assert_eq!(
        bed.calls_since(before)?,
        [TABLES.to_owned(), TABLE.to_owned(), bed.load_staged()]
    );
    assert_eq!(bed.nft.loaded()?.last(), Some(&program));
    assert_eq!(bed.read("current.nft")?, Some(program));
    assert_eq!(bed.nft.table()?, table);
    assert_eq!(bed.status_value("RESULT")?, "applied");
    assert_eq!(bed.status_value("STALE_REASON")?, "");
    Ok(())
}

#[test]
fn a_dormant_table_is_re_applied_and_check_reports_it_dormant() -> TestResult {
    let mut bed = Bed::new("dormant")?;
    bed.good()?;
    let program = bed.read("current.nft")?.ok_or("no current.nft")?;
    let table = bed.nft.table()?.ok_or("no table loaded")?;

    // CONTRACT MISMATCH: docs/contract.md, Idempotence, lists no condition on the table's flags,
    // its check verdict table has no `dormant`, and its exit table gives check exit 1 only for
    // `missing` or `absent`. Observed (contract-notes T4b): a dormant table re-applies, and check
    // says `result=dormant`, exit 1.
    bed.nft.knob("dormant")?;
    let before = bed.calls()?;
    let ran = bed.sync(answers(&two_keys()))?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, format!("result=applied reason= {COUNTS}\n"));
    assert_eq!(ran.events(), Vec::<&str>::new(), "{ran:?}");
    assert_eq!(
        bed.calls_since(before)?,
        [TABLES.to_owned(), TABLE.to_owned(), bed.load_staged()]
    );
    assert_eq!(bed.nft.loaded()?.last(), Some(&program));
    // the load woke the table
    assert!(!bed.nft.has_knob("dormant"));
    assert_eq!(bed.nft.table()?.as_deref(), Some(table.as_str()));
    assert_eq!(bed.status_value("RESULT")?, "applied");

    // check shows what a dormant table lacks, and says dormant whatever the snapshot says
    bed.nft.knob("dormant")?;
    let hand_edited = table.replace("elem allow_v4 198.51.100.7\n", "");
    assert_ne!(hand_edited, table);
    bed.nft.set_table(&hand_edited)?;
    let status = bed.stamp("status")?;
    let before = bed.calls()?;
    let ran = bed.run_with(vec![fake_rpc::result(&two_keys().to_string())], &["check"])?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "missing 198.51.100.7\nresult=dormant missing_total=1 mismatch_total=0\n"
    );
    assert_eq!(ran.events(), Vec::<&str>::new(), "{ran:?}");
    assert_eq!(bed.calls_since(before)?, [TABLES, TABLE]);

    // a node that cannot be asked changes nothing: still dormant, and no stale reason logged
    let before = bed.calls()?;
    let ran = bed.run_with(vec![Response::Down], &["check"])?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=dormant missing_total=0 mismatch_total=0\n"
    );
    assert_eq!(ran.events(), Vec::<&str>::new(), "{ran:?}");
    assert_eq!(bed.calls_since(before)?, [TABLES, TABLE]);
    // check writes nothing and wakes nothing
    assert!(bed.nft.has_knob("dormant"));
    assert_eq!(bed.stamp("status")?, status);
    Ok(())
}

#[test]
fn a_failed_listing_on_the_stale_path_is_a_warning_and_reloads_nothing() -> TestResult {
    let mut bed = Bed::new("tables-fail")?;
    bed.good()?;
    let last_good = bed.status_value("LAST_GOOD")?;

    // with the table present, and with it absent while current.nft could reload it
    for present in [true, false] {
        if !present {
            bed.nft.delete_table()?;
        }
        let kept = Kept::now(&bed)?;
        let status = bed.stamp("status")?;
        // the run's first listing works; the stale path's, the second, fails
        bed.nft.knob_with("tables-fail-nth", "2")?;
        let before = bed.calls()?;
        let ran = bed.sync(vec![Response::Down])?;
        // CONTRACT MISMATCH: docs/contract.md, How nft is run, says "Any failure of either listing
        // call is nft-unavailable, exit 2, nothing touched", and The stale path says
        // nft-unavailable never takes it. Observed (contract-notes T5, F-B-7): a failure of the
        // stale path's own listing is the warning `nft-unavailable detail=…`, the stale path goes on
        // with action=none and no reload, writes status and exits 3.
        assert_eq!(ran.code, Some(3), "present={present}: {ran:?}");
        assert_eq!(
            ran.stdout,
            format!("result=stale reason=rpc-unreachable {COUNTS}\n"),
            "present={present}"
        );
        assert_eq!(
            ran.events(),
            [
                "nft-unavailable detail=\"nft exited with status 1: netlink: Error: cache initialization failed: Device or resource busy\"",
                "stale reason=rpc-unreachable action=none"
            ],
            "present={present}"
        );
        assert_eq!(
            bed.calls_since(before)?,
            [TABLES, TABLES],
            "present={present}"
        );
        assert!(!bed.nft.has_knob("tables-fail-nth"), "present={present}");
        assert_eq!(Kept::now(&bed)?, kept, "present={present}");
        assert_eq!(bed.names()?, AFTER_GOOD, "present={present}");
        assert_ne!(bed.stamp("status")?, status, "present={present}");
        let status = bed.status()?;
        assert_eq!(
            status.get("RESULT").map(String::as_str),
            Some("stale"),
            "present={present}"
        );
        assert_eq!(
            status.get("STALE_REASON").map(String::as_str),
            Some("rpc-unreachable")
        );
        assert_eq!(status.get("LAST_GOOD"), Some(&last_good));
    }
    Ok(())
}
