//! Integration tests of `status` and `check`: the built binary against a fake JSON-RPC server and a fake nft, after a real `sync`.
//!
//! Each test gets its own directory holding the state dir and a link to `tests/support/fake-nft.sh`; the node is a [`FakeRpc`].

mod support;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use support::fake_rpc::{self, FakeRpc, Response};
use support::{FakeNft, TempDir};

type TestResult = Result<(), Box<dyn Error>>;
type Res<T> = Result<T, Box<dyn Error>>;

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_tn-p2p-allow-sync");

/// The epoch info a good sync asks for after the snapshot.
const EPOCH_INFO: &str = r#"{"epochId":42,"epochDuration":86400}"#;

/// The keys the deployment scripts add to `status --json` themselves.
const SCRIPT_KEYS: [&str; 4] = ["enabled", "persistent", "timer", "reason"];

/// The keys of the status file, in file order.
const FILE_KEYS: [&str; 18] = [
    "STATUS_VERSION",
    "TOOL_VERSION",
    "RESULT",
    "LAST_RUN",
    "LAST_GOOD",
    "EPOCH",
    "EPOCH_DURATION",
    "STALE_SINCE",
    "STALE_REASON",
    "ALLOWED_V4",
    "ALLOWED_V6",
    "STATIC_V4",
    "STATIC_V6",
    "FLOOR",
    "KEYS",
    "DNS_SKIPPED",
    "UNPARSED",
    "UNKNOWN_SLOTS",
];

/// The live keys `status` adds after the file's, in order.
const LIVE_KEYS: [&str; 7] = [
    "LOADED",
    "AGE_SECS",
    "STALE",
    "COUNTER_ACCEPT_PACKETS",
    "COUNTER_ACCEPT_BYTES",
    "COUNTER_DROP_PACKETS",
    "COUNTER_DROP_BYTES",
];

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

    /// Whether stderr holds nothing but the warning every run with the fake nft's path gives.
    fn quiet(&self) -> bool {
        self.stderr
            .lines()
            .all(|line| line.starts_with("nft-path path="))
    }

    /// The `KEY=VALUE` lines of stdout, in order.
    fn pairs(&self) -> Vec<(String, String)> {
        self.stdout
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    }

    /// The value of `key` in stdout, if listed.
    fn value(&self, key: &str) -> Option<String> {
        self.pairs()
            .into_iter()
            .find_map(|(k, value)| (k == key).then_some(value))
    }

    /// stdout as one JSON object.
    fn json(&self) -> Res<serde_json::Map<String, Value>> {
        match serde_json::from_str(&self.stdout)? {
            Value::Object(object) => Ok(object),
            other => Err(format!("not an object: {other}").into()),
        }
    }
}

/// A file as a run may leave it: its bytes, inode and modification time in seconds and nanoseconds.
type FileStamp = (Vec<u8>, u64, i64, i64);

/// One test's world: the state dir, the fake nft and the fake node.
struct Bed {
    _temp: TempDir,
    state: PathBuf,
    nft: FakeNft,
    rpc: FakeRpc,
}

impl Bed {
    /// A state dir holding only a config with the default ports.
    fn new(tag: &str) -> Res<Self> {
        let temp = TempDir::new(tag)?;
        let state = temp.join("state");
        fs::create_dir(&state)?;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        let nft = FakeNft::install(temp.path())?;
        let rpc = FakeRpc::start(Vec::new())?;
        let config = state.join("config");
        fs::write(
            &config,
            format!(
                "CONFIG_VERSION=1\nRPC_PORT={}\nP2P_PORTS=49590,49594\nP2P_ALLOW_FROM=\n",
                rpc.port()
            ),
        )?;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            _temp: temp,
            state,
            nft,
            rpc,
        })
    }

    /// Runs the binary with `--state-dir`, `--nft nft` and `args`.
    fn run_with_nft(&self, nft: &Path, args: &[&str]) -> Res<Ran> {
        // the binary under test is the one program these tests start
        #[allow(clippy::disallowed_methods)]
        let mut command = Command::new(BIN);
        let output = command
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--nft")
            .arg(nft)
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

    /// Runs the binary with the fake nft.
    fn run(&self, args: &[&str]) -> Res<Ran> {
        self.run_with_nft(&self.nft.path(), args)
    }

    /// A good `sync` of `snap`, checked to have applied.
    fn sync(&mut self, snap: &Value) -> Res<()> {
        self.rpc.set(vec![
            fake_rpc::result(&snap.to_string()),
            fake_rpc::result(EPOCH_INFO),
        ])?;
        let ran = self.run(&["sync"])?;
        if ran.code == Some(0) && ran.stdout.starts_with("result=applied ") {
            Ok(())
        } else {
            Err(format!("sync did not apply: {ran:?}").into())
        }
    }

    /// Runs `check` against a node that answers with `responses`.
    fn check(&mut self, responses: Vec<Response>) -> Res<Ran> {
        self.rpc.set(responses)?;
        self.run(&["check"])
    }

    /// Runs `check` against a node that serves `snap`.
    fn check_snapshot(&mut self, snap: &Value) -> Res<Ran> {
        self.check(vec![fake_rpc::result(&snap.to_string())])
    }

    /// Rewrites the fake's live table one line at a time; `edit` returns the new line or `None` to delete it.
    fn edit_table(&self, edit: impl Fn(&str) -> Option<String>) -> Res<()> {
        let table = self.nft.table()?.ok_or("no table is loaded")?;
        let mut edited = String::new();
        for line in table.lines().filter_map(edit) {
            edited.push_str(&line);
            edited.push('\n');
        }
        if edited == table {
            return Err("the edit changed nothing".into());
        }
        Ok(self.nft.set_table(&edited)?)
    }

    /// Gives the two counters traffic.
    fn count_traffic(&self) -> Res<()> {
        self.edit_table(|line| match line {
            "counter tn_accept_p2p_listed" => Some(format!("{line} 12 3456")),
            "counter tn_drop_p2p_unlisted" => Some(format!("{line} 7 420")),
            other => Some(other.to_owned()),
        })
    }

    /// Every name in the state dir with its bytes, inode and modification time.
    fn files(&self) -> Res<BTreeMap<String, FileStamp>> {
        let mut files = BTreeMap::new();
        for entry in fs::read_dir(&self.state)? {
            let entry = entry?;
            let meta = fs::symlink_metadata(entry.path())?;
            files.insert(
                entry.file_name().to_string_lossy().into_owned(),
                (
                    fs::read(entry.path())?,
                    meta.ino(),
                    meta.mtime(),
                    meta.mtime_nsec(),
                ),
            );
        }
        Ok(files)
    }
}

/// A `bls_pubkey` made of the byte `n` and zeros.
fn key(n: u8) -> String {
    format!("0x{n:02x}{}", "0".repeat(190))
}

/// The QUIC multiaddr of an IPv4 literal.
fn ip4(ip: &str) -> String {
    format!("/ip4/{ip}/udp/49590/quic-v1")
}

/// A current committee entry for key `n` dialled at `multiaddrs`.
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

/// [`entry`] with the signed record's own addresses `record` beside the dialled ones.
fn configured(n: u8, multiaddrs: &[String], record: &[String]) -> Res<Value> {
    let mut value = entry(n, multiaddrs);
    value
        .as_object_mut()
        .ok_or("an entry is an object")?
        .insert("record_multiaddrs".to_owned(), json!(record));
    Ok(value)
}

/// A snapshot whose available primary lists `entries`, its pool tracked at `tracked` and its window at `epoch`.
fn snapshot_with(epoch: u64, tracked: u64, fallback: Value, entries: Vec<Value>) -> Value {
    json!({"version": 1, "epoch": epoch, "swarms": [{
        "network": "primary",
        "worker_id": null,
        "available": true,
        "tracked_epoch": tracked,
        "admission": {
            "configured": "Closed", "effective": "Closed", "epoch": epoch, "fallback": fallback,
            "resolved_current": 4, "required_current": 3, "connected_current": 3, "contradicted": 0,
        },
        "entries": entries,
    }]})
}

/// An authoritative snapshot at `epoch` listing `entries`.
fn snapshot(epoch: u64, entries: Vec<Value>) -> Value {
    snapshot_with(epoch, epoch, Value::Null, entries)
}

/// Two current keys at epoch 42: 0x11 at 203.0.113.10 and 0x22 at 198.51.100.7.
fn two_keys() -> Value {
    snapshot(
        42,
        vec![
            entry(0x11, &[ip4("203.0.113.10")]),
            entry(0x22, &[ip4("198.51.100.7")]),
        ],
    )
}

fn keys_of(pairs: &[(String, String)]) -> Vec<&str> {
    pairs.iter().map(|(key, _)| key.as_str()).collect()
}

#[test]
fn status_text_before_any_sync_is_the_default_file_and_no_table() -> TestResult {
    let bed = Bed::new("status-fresh")?;
    let ran = bed.run(&["status"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    let pairs = ran.pairs();
    let expected: Vec<&str> = FILE_KEYS.iter().chain(&LIVE_KEYS).copied().collect();
    assert_eq!(keys_of(&pairs), expected);
    assert_eq!(ran.stdout.lines().count(), expected.len());
    assert_eq!(ran.value("STATUS_VERSION").as_deref(), Some("1"));
    assert_eq!(ran.value("RESULT").as_deref(), Some(""));
    assert_eq!(ran.value("LOADED").as_deref(), Some("false"));
    for key in LIVE_KEYS.iter().skip(1) {
        assert_eq!(ran.value(key).as_deref(), Some(""), "{key}");
    }
    assert!(ran.quiet(), "{}", ran.stderr);
    Ok(())
}

#[test]
fn status_text_after_a_sync_is_the_file_then_the_live_table() -> TestResult {
    let mut bed = Bed::new("status-text")?;
    bed.sync(&two_keys())?;
    bed.count_traffic()?;
    let ran = bed.run(&["status"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    let file = fs::read_to_string(bed.state.join("status"))?;
    assert!(ran.stdout.starts_with(&file), "{}", ran.stdout);
    let pairs = ran.pairs();
    let expected: Vec<&str> = FILE_KEYS.iter().chain(&LIVE_KEYS).copied().collect();
    assert_eq!(keys_of(&pairs), expected);
    for (key, value) in [
        ("RESULT", "applied"),
        ("EPOCH", "42"),
        ("EPOCH_DURATION", "86400"),
        ("ALLOWED_V4", "2"),
        ("KEYS", "2"),
        ("LOADED", "true"),
        ("STALE", "false"),
        ("COUNTER_ACCEPT_PACKETS", "12"),
        ("COUNTER_ACCEPT_BYTES", "3456"),
        ("COUNTER_DROP_PACKETS", "7"),
        ("COUNTER_DROP_BYTES", "420"),
    ] {
        assert_eq!(ran.value(key).as_deref(), Some(value), "{key}");
    }
    let age: u64 = ran.value("AGE_SECS").ok_or("no AGE_SECS")?.parse()?;
    assert!(age < 600, "{age}");

    // the table gone: still an answer, with the counters unknown
    bed.nft.delete_table()?;
    let ran = bed.run(&["status"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.value("LOADED").as_deref(), Some("false"));
    for key in LIVE_KEYS.iter().skip(3) {
        assert_eq!(ran.value(key).as_deref(), Some(""), "{key}");
    }
    Ok(())
}

#[test]
fn status_json_is_one_line_with_every_key_and_none_of_the_script_keys() -> TestResult {
    let mut bed = Bed::new("status-json")?;
    bed.sync(&two_keys())?;
    bed.count_traffic()?;
    let ran = bed.run(&["status", "--json"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout.matches('\n').count(), 1, "{}", ran.stdout);
    assert!(ran.stdout.starts_with('{') && ran.stdout.ends_with("}\n"));
    let object = ran.json()?;
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "age_secs",
            "allowed_v4",
            "allowed_v6",
            "counters",
            "dns_skipped",
            "epoch",
            "floor",
            "keys",
            "last_good",
            "loaded",
            "stale",
            "stale_reason",
            "stale_since",
            "static",
        ]
    );
    for key in SCRIPT_KEYS {
        assert!(!object.contains_key(key), "{key}");
    }
    for (key, value) in [
        ("loaded", json!(true)),
        ("epoch", json!(42)),
        ("allowed_v4", json!(2)),
        ("allowed_v6", json!(0)),
        ("static", json!(0)),
        ("floor", json!(0)),
        ("keys", json!(2)),
        ("dns_skipped", json!(0)),
        ("stale", json!(false)),
        ("stale_since", Value::Null),
        ("stale_reason", Value::Null),
        ("counters", json!({"accepted": 12, "dropped": 7})),
    ] {
        assert_eq!(object.get(key), Some(&value), "{key}");
    }
    assert!(object.get("last_good").is_some_and(Value::is_u64));
    assert!(object.get("age_secs").is_some_and(Value::is_u64));

    // the scripts splice their four keys in front of the tool's
    let tail = ran.stdout.strip_prefix('{').ok_or("no leading brace")?;
    let spliced =
        format!(r#"{{"enabled":true,"persistent":true,"timer":"active","reason":"",{tail}"#);
    let spliced: Value = serde_json::from_str(&spliced)?;
    assert_eq!(spliced.get("epoch"), Some(&json!(42)));
    Ok(())
}

#[test]
fn status_answers_with_null_live_fields_when_nft_is_unavailable() -> TestResult {
    let mut bed = Bed::new("status-nonft")?;
    bed.sync(&two_keys())?;
    let missing = Path::new("/nonexistent/tn-p2p-allow-sync/nft");
    let ran = bed.run_with_nft(missing, &["status", "--json"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    let object = ran.json()?;
    assert_eq!(object.get("loaded"), Some(&Value::Null));
    assert_eq!(
        object.get("counters"),
        Some(&json!({"accepted": null, "dropped": null}))
    );
    assert_eq!(object.get("epoch"), Some(&json!(42)));
    assert!(
        ran.logged(&format!("nft-path path={}", missing.display())),
        "{}",
        ran.stderr
    );
    assert!(ran.logged("nft-unavailable detail="), "{}", ran.stderr);

    let ran = bed.run_with_nft(missing, &["status"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    for key in LIVE_KEYS
        .iter()
        .filter(|key| **key != "AGE_SECS" && **key != "STALE")
    {
        assert_eq!(ran.value(key).as_deref(), Some(""), "{key}");
    }

    // refused by the kernel: the same answer, while check cannot run at all
    bed.nft.knob("eperm")?;
    let ran = bed.run(&["status", "--json"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.json()?.get("loaded"), Some(&Value::Null));
    let ran = bed.check(Vec::new())?;
    assert_eq!((ran.code, ran.stdout.as_str()), (Some(2), ""), "{ran:?}");
    assert!(ran.logged("error reason=nft-unavailable"), "{}", ran.stderr);
    assert!(ran.stderr.contains("run as root"), "{}", ran.stderr);
    Ok(())
}

#[test]
fn status_and_check_take_no_lock_and_write_nothing() -> TestResult {
    let mut bed = Bed::new("readonly")?;
    bed.sync(&two_keys())?;
    let before = bed.files()?;
    let lock = File::open(bed.state.join("lock"))?;
    lock.try_lock()?;
    for args in [&["status"][..], &["status", "--json"]] {
        let ran = bed.run(args)?;
        assert_eq!(ran.code, Some(0), "{args:?}: {ran:?}");
    }
    let ran = bed.check_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    let ran = bed.check(vec![Response::Down])?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    drop(lock);
    assert_eq!(bed.files()?, before);
    Ok(())
}

#[test]
fn check_after_a_sync_is_ok_and_asks_only_for_the_snapshot() -> TestResult {
    let mut bed = Bed::new("check-ok")?;
    bed.sync(&two_keys())?;
    let calls = bed.rpc.methods().len();
    let ran = bed.check_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, "result=ok missing_total=0 mismatch_total=0\n");
    assert!(ran.quiet(), "{}", ran.stderr);
    let methods = bed.rpc.methods();
    assert_eq!(
        methods.get(calls..),
        Some(&["tn_admissionSnapshot".to_owned()][..])
    );
    Ok(())
}

#[test]
fn check_reports_an_element_deleted_by_hand_as_missing() -> TestResult {
    let mut bed = Bed::new("check-missing")?;
    bed.sync(&two_keys())?;
    bed.edit_table(|line| (line != "elem allow_v4 203.0.113.10").then(|| line.to_owned()))?;
    let ran = bed.check_snapshot(&two_keys())?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "missing 203.0.113.10\nresult=missing missing_total=1 mismatch_total=0\n"
    );
    Ok(())
}

#[test]
fn check_is_behind_for_an_address_the_last_sync_never_held() -> TestResult {
    let mut bed = Bed::new("check-unsynced")?;
    bed.sync(&two_keys())?;
    // the same epoch, and 0x22 republished its record at a new address since the sync
    let moved = snapshot(
        42,
        vec![
            entry(0x11, &[ip4("203.0.113.10")]),
            entry(0x22, &[ip4("198.51.100.99")]),
        ],
    );
    let ran = bed.check_snapshot(&moved)?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "missing 198.51.100.99\nresult=behind missing_total=1 mismatch_total=0\n"
    );
    assert!(
        ran.logged("behind snapshot_epoch=42 applied_epoch=42"),
        "{}",
        ran.stderr
    );

    // the next sync adds it
    bed.sync(&moved)?;
    let ran = bed.check_snapshot(&moved)?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, "result=ok missing_total=0 mismatch_total=0\n");

    // with no state.json to tell, a gap at the applied epoch is `missing`
    let further = snapshot(
        42,
        vec![
            entry(0x11, &[ip4("203.0.113.10")]),
            entry(0x22, &[ip4("198.51.100.100")]),
        ],
    );
    fs::remove_file(bed.state.join("state.json"))?;
    let ran = bed.check_snapshot(&further)?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "missing 198.51.100.100\nresult=missing missing_total=1 mismatch_total=0\n"
    );
    Ok(())
}

#[test]
fn check_without_the_table_is_absent_and_asks_the_node_nothing() -> TestResult {
    let mut bed = Bed::new("check-absent")?;
    bed.sync(&two_keys())?;
    bed.nft.delete_table()?;
    let calls = bed.rpc.methods().len();
    let ran = bed.check(Vec::new())?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=absent missing_total=0 mismatch_total=0\n"
    );
    assert_eq!(bed.rpc.methods().len(), calls);

    // before the first sync there is no table either
    let fresh = Bed::new("check-absent-fresh")?;
    let ran = fresh.run(&["check"])?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "result=absent missing_total=0 mismatch_total=0\n"
    );
    Ok(())
}

#[test]
fn check_behind_the_snapshot_epoch_exits_3_and_shows_the_gap() -> TestResult {
    let mut bed = Bed::new("check-behind")?;
    bed.sync(&two_keys())?;
    let next = snapshot(
        43,
        vec![
            entry(0x11, &[ip4("203.0.113.10")]),
            entry(0x22, &[ip4("198.51.100.7")]),
            entry(0x33, &[ip4("192.0.2.33")]),
        ],
    );
    let ran = bed.check_snapshot(&next)?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "missing 192.0.2.33\nresult=behind missing_total=1 mismatch_total=0\n"
    );
    assert!(
        ran.logged("behind snapshot_epoch=43 applied_epoch=42"),
        "{}",
        ran.stderr
    );
    Ok(())
}

#[test]
fn check_cannot_judge_without_an_authoritative_snapshot() -> TestResult {
    let mut bed = Bed::new("check-stale")?;
    bed.sync(&two_keys())?;
    let entries = || {
        vec![
            entry(0x11, &[ip4("203.0.113.10")]),
            entry(0x22, &[ip4("198.51.100.7")]),
        ]
    };
    let cases = [
        (vec![Response::Down], "rpc-unreachable"),
        (vec![fake_rpc::error(-32601)], "rpc-method-missing"),
        (
            vec![fake_rpc::result(
                &snapshot_with(42, 42, json!("Missing"), entries()).to_string(),
            )],
            "fallback-missing",
        ),
        // the pool still lists the previous epoch's sets while the window moved on
        (
            vec![fake_rpc::result(
                &snapshot_with(43, 42, Value::Null, entries()).to_string(),
            )],
            "pool-unpublished",
        ),
        (
            vec![fake_rpc::result(&snapshot(41, entries()).to_string())],
            "epoch-regressed",
        ),
    ];
    for (responses, reason) in cases {
        let ran = bed.check(responses)?;
        assert_eq!(ran.code, Some(3), "{reason}: {ran:?}");
        assert_eq!(
            ran.stdout, "result=stale missing_total=0 mismatch_total=0\n",
            "{reason}"
        );
        assert!(
            ran.logged(&format!("stale reason={reason}")),
            "{reason}: {}",
            ran.stderr
        );
    }
    Ok(())
}

#[test]
fn check_lists_a_mismatch_without_failing() -> TestResult {
    let mut bed = Bed::new("check-mismatch")?;
    let snap = snapshot(
        42,
        vec![
            configured(0x11, &[ip4("203.0.113.10")], &[ip4("192.0.2.1")])?,
            entry(0x22, &[ip4("198.51.100.7")]),
        ],
    );
    // the sync admits the dialled and the signed addresses alike
    bed.sync(&snap)?;
    let ran = bed.check_snapshot(&snap)?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "mismatch 0x1100000000000000\nresult=ok missing_total=0 mismatch_total=1\n"
    );
    Ok(())
}

#[test]
fn check_caps_both_kinds_of_line_at_five_and_counts_them_all() -> TestResult {
    let mut bed = Bed::new("check-caps")?;
    let mut entries = vec![
        entry(0x11, &[ip4("203.0.113.10")]),
        entry(0x22, &[ip4("198.51.100.7")]),
    ];
    for n in 1..=7_u8 {
        entries.push(configured(
            0x30 + n,
            &[ip4(&format!("192.0.2.{n}"))],
            &[ip4(&format!("198.18.0.{n}"))],
        )?);
    }
    let snap = snapshot(42, entries);
    // synced, then lost by hand: addresses the last sync never held would be `behind`
    bed.sync(&snap)?;
    bed.edit_table(|line| {
        let lost = line.starts_with("elem allow_v4 192.0.2.")
            || line.starts_with("elem allow_v4 198.18.0.");
        (!lost).then(|| line.to_owned())
    })?;
    let ran = bed.check_snapshot(&snap)?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    let expected = [
        "missing 192.0.2.1",
        "missing 192.0.2.2",
        "missing 192.0.2.3",
        "missing 192.0.2.4",
        "missing 192.0.2.5",
        "mismatch 0x3100000000000000",
        "mismatch 0x3200000000000000",
        "mismatch 0x3300000000000000",
        "mismatch 0x3400000000000000",
        "mismatch 0x3500000000000000",
        "result=missing missing_total=14 mismatch_total=7",
    ];
    assert_eq!(ran.stdout.lines().collect::<Vec<_>>(), expected);
    Ok(())
}
