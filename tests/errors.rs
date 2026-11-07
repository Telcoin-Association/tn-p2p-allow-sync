//! Integration tests of error handling and outputs through the built binary: the best-effort epoch info, a `status` or `lock` that cannot be written, `render --check` refused, and the exit code of each failure the contract's table names.
//!
//! Each test gets its own directory holding the state dir, mode 0700, with a config and an empty floor, a link to `tests/support/fake-nft.sh` and a [`FakeRpc`] for the node, as in `tests/sync.rs`.
//! A stdout that cannot be written is `/dev/full`, where every write fails with ENOSPC, or a pipe whose read end is closed before the run, where every write fails with EPIPE.
//! A closed fd 1 is not among them: closing it in the child needs `unsafe`, and the Rust runtime reopens a closed standard fd on `/dev/null` anyway.

mod support;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use support::fake_rpc::{self, FakeRpc, Response};
use support::{FakeNft, TempDir, read_optional};

type TestResult = Result<(), Box<dyn Error>>;
type Res<T> = Result<T, Box<dyn Error>>;

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_tn-p2p-allow-sync");

/// The program of the contract; everything before its first `add element` is the same for every program with the default ports.
const GOLDEN: &str = include_str!("golden/program-6.6.nft");

/// `epochDuration` before the node has reported one.
const DEFAULT_DURATION: &str = "21600";

/// The most bytes `render --snapshot` reads, the RPC body cap of 8 MiB.
const SNAPSHOT_CAP: usize = 8_388_608;

/// Where the stdout of a run goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    /// A pipe the test reads.
    Captured,
    /// `/dev/full`: every write fails with "No space left on device".
    Full,
    /// A pipe whose read end is closed before the run starts: every write fails with "Broken pipe".
    Broken,
}

/// The two stdouts that cannot be written.
const FAILING: [Sink; 2] = [Sink::Full, Sink::Broken];

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

    /// The last line on stderr, the one that closes the run; empty when stderr is.
    fn closing(&self) -> &str {
        self.stderr.lines().last().unwrap_or_default()
    }
}

/// One test's world: the state dir, the fake nft and the fake node.
struct Bed {
    temp: TempDir,
    state: PathBuf,
    nft: FakeNft,
    rpc: FakeRpc,
}

impl Bed {
    /// A state dir holding a config with the default ports and no static entries, and an empty floor.
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
        bed.write("config", &bed.config_text())?;
        bed.write("floor", "# floor 1\n")?;
        Ok(bed)
    }

    /// The config with the node's port and the default ports.
    fn config_text(&self) -> String {
        format!(
            "# tn-p2p-allow-sync config, written by firewall-setup.sh\nCONFIG_VERSION=1\nRPC_PORT={}\nP2P_PORTS=49590,49594\nP2P_ALLOW_FROM=\n",
            self.rpc.port()
        )
    }

    /// Writes `text` to the state dir's `name`, mode 0600.
    fn write(&self, name: &str, text: &str) -> Res<()> {
        let path = self.file(name);
        fs::write(&path, text)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// Runs the binary with `--state-dir` and `--nft` set, `args` after them and stdout captured.
    fn run(&self, args: &[&str]) -> Res<Ran> {
        self.run_to(Sink::Captured, args)
    }

    /// Runs the binary with `--state-dir` and `--nft` set, `args` after them and stdout going to `sink`.
    fn run_to(&self, sink: Sink, args: &[&str]) -> Res<Ran> {
        let mut all = vec![
            "--state-dir".to_owned(),
            self.state.to_string_lossy().into_owned(),
            "--nft".to_owned(),
            self.nft.path().to_string_lossy().into_owned(),
        ];
        all.extend(args.iter().map(|&arg| arg.to_owned()));
        run_binary(sink, &all)
    }

    /// Queues `responses` on the node, then runs `sync` with stdout going to `sink`.
    fn sync_to(&mut self, sink: Sink, responses: Vec<Response>) -> Res<Ran> {
        self.rpc.set(responses)?;
        self.run_to(sink, &["sync"])
    }

    /// Queues `responses` on the node, then runs a plain `sync`.
    fn sync(&mut self, responses: Vec<Response>) -> Res<Ran> {
        self.sync_to(Sink::Captured, responses)
    }

    /// Queues `responses` on the node, then runs `check`.
    fn check(&mut self, responses: Vec<Response>) -> Res<Ran> {
        self.rpc.set(responses)?;
        self.run(&["check"])
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

    /// The names in the state dir, sorted.
    fn names(&self) -> Res<Vec<String>> {
        let mut names = fs::read_dir(&self.state)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Res<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    /// Every file of the state dir with its bytes, so a test sees that a run wrote nothing; a directory reads as `None`.
    fn contents(&self) -> Res<BTreeMap<String, Option<Vec<u8>>>> {
        self.names()?
            .into_iter()
            .map(|name| {
                let path = self.file(&name);
                let bytes = if fs::symlink_metadata(&path)?.is_file() {
                    Some(fs::read(&path)?)
                } else {
                    None
                };
                Ok((name, bytes))
            })
            .collect()
    }

    /// A file outside the state dir holding `text`, for `render --snapshot`.
    fn outside(&self, name: &str, text: &[u8]) -> Res<String> {
        let path = self.temp.join(name);
        fs::write(&path, text)?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// Puts a non-empty directory at the temporary name of `name`, which the writer may not remove, so every write of `name` fails.
    fn block(&self, name: &str) -> Res<PathBuf> {
        let blocker = self.file(&format!(".{name}.new"));
        fs::create_dir(&blocker)?;
        fs::write(blocker.join("in-the-way"), "")?;
        Ok(blocker)
    }
}

/// Runs the binary with `args` and stdout going to `sink`; stderr is always captured.
fn run_binary(sink: Sink, args: &[String]) -> Res<Ran> {
    // the binary under test is the one program these tests start
    #[allow(clippy::disallowed_methods)]
    let mut command = Command::new(BIN);
    command
        .args(args)
        // stderr is a pipe, never the journal
        .env_remove("JOURNAL_STREAM")
        .stdin(Stdio::null());
    match sink {
        Sink::Captured => {}
        Sink::Full => {
            command.stdout(Stdio::from(
                OpenOptions::new().write(true).open("/dev/full")?,
            ));
        }
        Sink::Broken => {
            let (reader, writer) = std::io::pipe()?;
            // no read end is left, so the child's first write fails with EPIPE
            drop(reader);
            command.stdout(Stdio::from(writer));
        }
    }
    let output = command.output()?;
    Ok(Ran {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

/// A `bls_pubkey` made of the byte `n` and zeros.
fn key(n: u8) -> String {
    format!("0x{n:02x}{}", "0".repeat(190))
}

/// An entry for key `n` in the current committee at the IPv4 literal `ip`, resolved from its record.
fn entry(n: u8, ip: &str) -> Value {
    json!({
        "bls_pubkey": key(n),
        "slots": ["current", "eligible"],
        "peer_id": null,
        "network_pubkey": null,
        "multiaddrs": [format!("/ip4/{ip}/udp/49590/quic-v1")],
        "record_timestamp": 1_793_750_400_u64,
        "source": "record",
        "admitted": true,
    })
}

/// A snapshot whose only swarm, the primary, is published for `epoch` and lists `entries`.
fn snapshot(epoch: u64, entries: Vec<Value>) -> Value {
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
    snapshot(
        42,
        vec![entry(0x11, "203.0.113.10"), entry(0x22, "198.51.100.7")],
    )
}

/// The node's answer to `tn_admissionSnapshot`.
fn snap(snapshot: &Value) -> Response {
    fake_rpc::result(&snapshot.to_string())
}

/// The node's answer to `tn_getCurrentEpochInfo` with this `result`.
fn info(result: &str) -> Response {
    fake_rpc::result(result)
}

/// The node's answers to a good run: the snapshot, then an epoch info of `duration` seconds.
fn answers(snapshot: &Value, duration: u64) -> Vec<Response> {
    vec![
        snap(snapshot),
        info(&format!(r#"{{"epochId":42,"epochDuration":{duration}}}"#)),
    ]
}

/// The program of [`two_keys`], with the default ports and nothing static.
fn two_keys_program() -> Res<String> {
    let (preamble, _) = GOLDEN
        .split_once("add element")
        .ok_or("the golden program has no element block")?;
    Ok(format!(
        "{preamble}add element inet tn_p2p_allow allow_v4 {{\n    198.51.100.7,\n    203.0.113.10\n}}\n"
    ))
}

/// The stdout line of a run that loaded or kept [`two_keys`] and ended as `result` with `reason`.
fn two_keys_line(result: &str, reason: &str) -> String {
    format!(
        "result={result} reason={reason} epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
    )
}

// ---- the epoch info is best effort ----

#[test]
fn an_epoch_info_error_is_a_warning_and_the_duration_stays_the_default() -> TestResult {
    let mut bed = Bed::new("epoch-error")?;
    let ran = bed.sync(vec![snap(&two_keys()), fake_rpc::error(-32601)])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, two_keys_line("applied", ""));
    assert!(
        ran.logged("epoch-info-failed reason=rpc-method-missing"),
        "{ran:?}"
    );
    assert!(!ran.logged("error ") && !ran.logged("stale "), "{ran:?}");
    assert_eq!(
        bed.rpc.methods(),
        ["tn_admissionSnapshot", "tn_getCurrentEpochInfo"]
    );
    let status = bed.status()?;
    for (key, value) in [
        ("RESULT", "applied"),
        ("EPOCH", "42"),
        ("EPOCH_DURATION", DEFAULT_DURATION),
        ("STALE_SINCE", ""),
        ("STALE_REASON", ""),
    ] {
        assert_eq!(status.get(key).map(String::as_str), Some(value), "{key}");
    }
    assert_eq!(bed.read("current.nft")?, Some(two_keys_program()?));

    // every other way the call can fail: a warning with the RPC token, still the default
    for (answer, reason) in [
        (vec![fake_rpc::error(-32000)], "rpc-error--32000"),
        (vec![Response::Status(500)], "rpc-http-500"),
        // nothing queued: the fake answers 503
        (Vec::new(), "rpc-http-503"),
        (
            vec![Response::Json(
                r#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_owned(),
            )],
            "snapshot-malformed",
        ),
        (vec![Response::Down], "rpc-unreachable"),
    ] {
        let mut responses = vec![snap(&two_keys())];
        responses.extend(answer);
        let ran = bed.sync(responses)?;
        assert_eq!(ran.code, Some(0), "{reason}: {ran:?}");
        assert_eq!(ran.stdout, two_keys_line("unchanged", ""), "{reason}");
        assert!(
            ran.logged(&format!("epoch-info-failed reason={reason}")),
            "{reason}: {ran:?}"
        );
        assert_eq!(bed.status_value("RESULT")?, "unchanged", "{reason}");
        assert_eq!(
            bed.status_value("EPOCH_DURATION")?,
            DEFAULT_DURATION,
            "{reason}"
        );
        assert_eq!(bed.status_value("STALE_REASON")?, "", "{reason}");
    }

    // once learned, a failure keeps the learned value, not the default
    assert_eq!(bed.sync(answers(&two_keys(), 86400))?.code, Some(0));
    assert_eq!(bed.status_value("EPOCH_DURATION")?, "86400");
    let ran = bed.sync(vec![snap(&two_keys()), fake_rpc::error(-32601)])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert!(
        ran.logged("epoch-info-failed reason=rpc-method-missing"),
        "{ran:?}"
    );
    assert_eq!(bed.status_value("EPOCH_DURATION")?, "86400");
    Ok(())
}

#[test]
fn an_epoch_info_without_a_positive_integer_duration_keeps_the_learned_one() -> TestResult {
    let mut bed = Bed::new("epoch-shape")?;
    let ran = bed.sync(answers(&two_keys(), 86400))?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert!(!ran.logged("epoch-info-failed"), "{ran:?}");
    assert_eq!(bed.status_value("EPOCH_DURATION")?, "86400");

    for result in [
        r#"{"epochId":42,"epochDuration":0}"#,
        r#"{"epochId":42}"#,
        "{}",
        r#"{"epochId":42,"epochDuration":"86400"}"#,
        r#"{"epochId":42,"epochDuration":-1}"#,
        r#"{"epochId":42,"epochDuration":1.5}"#,
        r#"{"epochId":42,"epochDuration":null}"#,
    ] {
        let ran = bed.sync(vec![snap(&two_keys()), info(result)])?;
        assert_eq!(ran.code, Some(0), "{result}: {ran:?}");
        assert_eq!(ran.stdout, two_keys_line("unchanged", ""), "{result}");
        assert!(
            ran.logged("epoch-info-failed reason=no-epoch-duration"),
            "{result}: {ran:?}"
        );
        assert_eq!(bed.status_value("EPOCH_DURATION")?, "86400", "{result}");
        assert_eq!(bed.status_value("RESULT")?, "unchanged", "{result}");
        assert_eq!(bed.status_value("STALE_SINCE")?, "", "{result}");
    }
    Ok(())
}

#[test]
fn a_valid_epoch_info_updates_the_duration_on_every_good_run() -> TestResult {
    let mut bed = Bed::new("epoch-valid")?;
    for (duration, result) in [(86400, "applied"), (43200, "unchanged"), (1, "unchanged")] {
        let ran = bed.sync(answers(&two_keys(), duration))?;
        assert_eq!(ran.code, Some(0), "{duration}: {ran:?}");
        assert_eq!(ran.stdout, two_keys_line(result, ""), "{duration}");
        assert!(!ran.logged("epoch-info-failed"), "{duration}: {ran:?}");
        assert_eq!(
            bed.status_value("EPOCH_DURATION")?,
            duration.to_string(),
            "{duration}"
        );
    }
    // the status sees it too: STALE compares AGE_SECS with twice this duration
    let ran = bed.run(&["status"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert!(
        ran.stdout.lines().any(|line| line == "EPOCH_DURATION=1"),
        "{ran:?}"
    );
    Ok(())
}

// ---- a status or lock that cannot be written ----

#[test]
fn a_status_that_cannot_be_written_prints_state_io_failed_after_the_program_and_memory()
-> TestResult {
    let mut bed = Bed::new("status-write")?;
    let blocker = bed.block("status")?;
    let ran = bed.sync(answers(&two_keys(), 86400))?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    // the contract fixes only `result=error reason=state-io-failed`; the counts are those of
    // the status the run failed to write, not of the previous one (there is none)
    assert_eq!(ran.stdout, two_keys_line("error", "state-io-failed"));
    assert!(
        ran.closing()
            .starts_with("error reason=state-io-failed detail="),
        "{ran:?}"
    );
    assert!(
        ran.closing().contains("a directory is in the way"),
        "{ran:?}"
    );
    // the program and memory went to disk before status, and the table holds the program
    assert_eq!(bed.read("current.nft")?, Some(two_keys_program()?));
    assert!(bed.read("state.json")?.is_some());
    assert!(bed.nft.table()?.is_some());
    assert_eq!(bed.status()?, BTreeMap::new());
    assert!(blocker.is_dir());
    assert_eq!(
        bed.names()?,
        [
            ".status.new",
            "config",
            "current.nft",
            "floor",
            "lock",
            "state.json"
        ]
    );

    // unblocked, the next run finds the program loaded and writes status
    fs::remove_dir_all(&blocker)?;
    let ran = bed.sync(answers(&two_keys(), 86400))?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, two_keys_line("unchanged", ""));
    let before = bed.read("status")?;
    assert_eq!(bed.status_value("RESULT")?, "unchanged");

    // blocked again: an unchanged run leaves the last status on disk as it was
    bed.block("status")?;
    let ran = bed.sync(answers(&two_keys(), 86400))?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, two_keys_line("error", "state-io-failed"));
    assert!(
        ran.closing().starts_with("error reason=state-io-failed "),
        "{ran:?}"
    );
    assert_eq!(bed.read("status")?, before);

    // a stale run whose status write fails: its stale line, then the error closes; exit 2, not 3
    let ran = bed.sync(vec![Response::Down])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, two_keys_line("error", "state-io-failed"));
    let lines = ran.stderr.lines().collect::<Vec<_>>();
    let [.., stale, closing] = lines.as_slice() else {
        return Err(format!("too few lines: {ran:?}").into());
    };
    assert_eq!(*stale, "stale reason=rpc-unreachable action=kept");
    assert!(
        closing.starts_with("error reason=state-io-failed detail="),
        "{ran:?}"
    );
    assert_eq!(bed.read("status")?, before);
    assert_eq!(bed.read("current.nft")?, Some(two_keys_program()?));
    Ok(())
}

#[test]
fn a_lock_that_cannot_be_taken_writes_nothing() -> TestResult {
    let mut bed = Bed::new("lock-fail")?;
    // a directory, or a symlink that the no-follow open refuses
    let elsewhere = bed.temp.join("elsewhere");
    fs::write(&elsewhere, "")?;
    for what in ["a directory", "a symlink"] {
        let lock = bed.file("lock");
        if what == "a directory" {
            fs::create_dir(&lock)?;
        } else {
            std::os::unix::fs::symlink(&elsewhere, &lock)?;
        }
        let before = bed.contents()?;
        let ran = bed.sync(answers(&two_keys(), 86400))?;
        assert_eq!(ran.code, Some(2), "{what}: {ran:?}");
        assert_eq!(ran.stdout, "", "{what}");
        assert!(
            ran.closing()
                .starts_with("error reason=state-io-failed detail="),
            "{what}: {ran:?}"
        );
        assert_eq!(bed.contents()?, before, "{what}");
        assert_eq!(bed.nft.argv()?, Vec::<String>::new(), "{what}");
        assert_eq!(bed.rpc.requests(), Vec::<String>::new(), "{what}");

        // capture-floor takes the same lock: its one failure line, and no floor written
        let peers = bed.outside(
            "peers.yaml",
            b"network_address: /ip4/192.0.2.10/udp/49590/quic-v1\n",
        )?;
        let ran = bed.run(&["capture-floor", &peers])?;
        assert_eq!(ran.code, Some(2), "{what}: {ran:?}");
        assert_eq!(ran.stdout, "", "{what}");
        assert!(
            ran.stderr.starts_with("capture-floor: state-io-failed: ")
                && ran.stderr.lines().count() == 1,
            "{what}: {ran:?}"
        );
        assert_eq!(bed.contents()?, before, "{what}");

        if what == "a directory" {
            fs::remove_dir(&lock)?;
        } else {
            fs::remove_file(&lock)?;
        }
    }
    assert_eq!(fs::read(&elsewhere)?, b"");
    Ok(())
}

// ---- render --check ----

#[test]
fn render_check_refused_by_nft_prints_nothing_and_exits_2() -> TestResult {
    let mut bed = Bed::new("render-check-fails")?;
    bed.nft.knob("check-fails")?;
    bed.rpc.set(vec![snap(&two_keys())])?;
    let before = bed.contents()?;
    let ran = bed.run(&["render", "--check"])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(
        ran.closing()
            .starts_with("error reason=nft-check-failed detail="),
        "{ran:?}"
    );
    assert!(!ran.logged("result="), "{ran:?}");
    // nft was given the whole program on stdin and loaded nothing
    assert_eq!(bed.nft.argv()?, ["-j list tables inet", "-c -f -"]);
    assert_eq!(bed.nft.loaded()?, vec![two_keys_program()?]);
    assert!(!bed.nft.has_knob("check-fails"));
    assert_eq!(bed.nft.table()?, None);
    assert_eq!(bed.contents()?, before);
    Ok(())
}

#[test]
fn render_check_that_passes_prints_the_program_on_stdout_and_the_summary_last_on_stderr()
-> TestResult {
    let mut bed = Bed::new("render-check-ok")?;
    bed.rpc.set(vec![snap(&two_keys())])?;
    let ran = bed.run(&["render", "--check"])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, two_keys_program()?);
    assert_eq!(
        format!("{}\n", ran.closing()),
        two_keys_line("rendered", "")
    );
    // nothing but the --nft warning comes before it
    let lines = ran.stderr.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{ran:?}");
    assert!(ran.logged("nft-path path="), "{ran:?}");
    assert_eq!(bed.nft.loaded()?, vec![two_keys_program()?]);
    Ok(())
}

#[test]
fn render_check_with_a_bad_input_or_no_nft_asks_neither_and_prints_nothing() -> TestResult {
    // an invalid config: refused before nft or the node is asked
    let mut bed = Bed::new("render-check-input")?;
    bed.write("config", "CONFIG_VERSION=1\nRPC_PORT=not-a-port\n")?;
    bed.rpc.set(vec![snap(&two_keys())])?;
    let ran = bed.run(&["render", "--check"])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(
        ran.closing()
            .starts_with("error reason=config-invalid detail="),
        "{ran:?}"
    );
    assert_eq!(bed.nft.argv()?, Vec::<String>::new());
    assert_eq!(bed.rpc.requests(), Vec::<String>::new());

    // nft refusing the listing: nft-unavailable before the node is asked
    let bed = Bed::new("render-check-eperm")?;
    bed.nft.knob("eperm")?;
    let ran = bed.run(&["render", "--check"])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(
        ran.closing()
            .starts_with("error reason=nft-unavailable detail="),
        "{ran:?}"
    );
    assert!(ran.closing().contains("run as root"), "{ran:?}");
    assert_eq!(bed.nft.argv()?, ["-j list tables inet"]);
    assert_eq!(bed.rpc.requests(), Vec::<String>::new());
    Ok(())
}

// ---- exit codes of check and render with a bad local input ----

#[test]
fn check_with_an_invalid_config_exits_2_with_nothing_on_stdout() -> TestResult {
    let mut bed = Bed::new("check-config")?;
    for (what, config) in [
        ("garbage", Some("not a config\n")),
        ("another version", Some("CONFIG_VERSION=2\n")),
        ("missing", None),
    ] {
        match config {
            Some(text) => bed.write("config", text)?,
            None => fs::remove_file(bed.file("config"))?,
        }
        let ran = bed.check(vec![snap(&two_keys())])?;
        assert_eq!(ran.code, Some(2), "{what}: {ran:?}");
        assert_eq!(ran.stdout, "", "{what}");
        assert!(
            ran.closing()
                .starts_with("error reason=config-invalid detail="),
            "{what}: {ran:?}"
        );
        assert_eq!(bed.nft.argv()?, Vec::<String>::new(), "{what}");
        assert_eq!(bed.rpc.requests(), Vec::<String>::new(), "{what}");
    }
    Ok(())
}

#[test]
fn check_reads_an_invalid_state_json_as_unreadable_never_as_an_error() -> TestResult {
    let mut bed = Bed::new("check-state")?;
    assert_eq!(bed.sync(answers(&two_keys(), 86400))?.code, Some(0));
    // a third key at an address the last sync never held
    let grown = snapshot(
        42,
        vec![
            entry(0x11, "203.0.113.10"),
            entry(0x22, "198.51.100.7"),
            entry(0x33, "192.0.2.33"),
        ],
    );
    let ran = bed.check(vec![snap(&grown)])?;
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "missing 192.0.2.33\nresult=behind missing_total=1 mismatch_total=0\n"
    );

    // the contract lists no state-invalid for check: a state.json it cannot read makes the
    // address plainly missing, exit 1
    bed.write("state.json", "not the form the tool writes\n")?;
    let ran = bed.check(vec![snap(&grown)])?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(
        ran.stdout,
        "missing 192.0.2.33\nresult=missing missing_total=1 mismatch_total=0\n"
    );
    assert!(!ran.logged("error"), "{ran:?}");
    Ok(())
}

#[test]
fn render_with_an_invalid_input_exits_2_with_nothing_on_stdout() -> TestResult {
    for (name, text, reason) in [
        (
            "config",
            "CONFIG_VERSION=1\nP2P_PORTS=0\n",
            "config-invalid",
        ),
        ("floor", "# not a floor\n", "floor-invalid"),
        ("state.json", "{}\n", "state-invalid"),
    ] {
        let mut bed = Bed::new("render-input")?;
        bed.write(name, text)?;
        bed.rpc.set(vec![snap(&two_keys())])?;
        let before = bed.contents()?;
        let ran = bed.run(&["render"])?;
        assert_eq!(ran.code, Some(2), "{name}: {ran:?}");
        assert_eq!(ran.stdout, "", "{name}");
        assert!(
            ran.closing()
                .starts_with(&format!("error reason={reason} detail=")),
            "{name}: {ran:?}"
        );
        assert!(!ran.logged("result="), "{name}: {ran:?}");
        assert_eq!(bed.rpc.requests(), Vec::<String>::new(), "{name}");
        assert_eq!(bed.nft.argv()?, Vec::<String>::new(), "{name}");
        assert_eq!(bed.contents()?, before, "{name}");
    }
    Ok(())
}

// ---- render --snapshot ----

#[test]
fn render_snapshot_over_the_cap_or_not_json_is_refused_with_exit_1() -> TestResult {
    let bed = Bed::new("render-snapshot")?;
    let refused = |reason: &str| {
        format!(
            "result=stale reason={reason} epoch=0 v4=0 v6=0 static=0 floor=0 keys=0 dns_skipped=0 unparsed=0"
        )
    };

    // exactly the cap, a valid snapshot padded with whitespace: read whole and rendered
    let mut padded = two_keys().to_string().into_bytes();
    padded.resize(SNAPSHOT_CAP, b' ');
    let file = bed.outside("at-cap.json", &padded)?;
    let ran = bed.run(&["render", "--snapshot", &file])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, two_keys_program()?);

    // one byte past it
    padded.push(b' ');
    let file = bed.outside("past-cap.json", &padded)?;
    let ran = bed.run(&["render", "--snapshot", &file])?;
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(ran.closing(), refused("rpc-body-too-large"), "{ran:?}");

    for (what, bytes) in [
        ("text", b"not json\n".as_slice()),
        ("empty", b"".as_slice()),
        ("truncated", br#"{"version":1,"swarms":["#.as_slice()),
    ] {
        let file = bed.outside("bad.json", bytes)?;
        let ran = bed.run(&["render", "--snapshot", &file])?;
        assert_eq!(ran.code, Some(1), "{what}: {ran:?}");
        assert_eq!(ran.stdout, "", "{what}");
        assert_eq!(
            ran.closing(),
            refused("snapshot-malformed"),
            "{what}: {ran:?}"
        );
    }

    // a file that cannot be read is an input error, exit 2
    let missing = bed.temp.join("absent.json");
    let ran = bed.run(&["render", "--snapshot", &missing.to_string_lossy()])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(
        ran.closing()
            .starts_with("error reason=snapshot-unreadable detail="),
        "{ran:?}"
    );
    assert_eq!(bed.rpc.requests(), Vec::<String>::new());
    assert_eq!(bed.names()?, ["config", "floor"]);
    Ok(())
}

// ---- a stdout that cannot be written ----

#[test]
fn a_stdout_that_cannot_be_written_is_output_failed_exit_2() -> TestResult {
    let mut bed = Bed::new("output-failed")?;
    assert_eq!(bed.sync(answers(&two_keys(), 86400))?.code, Some(0));
    let snapshot = bed.outside("snapshot.json", two_keys().to_string().as_bytes())?;
    let before = bed.contents()?;
    let commands: [&[&str]; 4] = [
        &["status"],
        &["status", "--json"],
        &["render", "--snapshot", snapshot.as_str()],
        &["check"],
    ];
    for sink in FAILING {
        for args in commands {
            bed.rpc.set(vec![snap(&two_keys())])?;
            let ran = bed.run_to(sink, args)?;
            assert_eq!(ran.code, Some(2), "{sink:?} {args:?}: {ran:?}");
            assert!(
                ran.closing()
                    .starts_with("error reason=output-failed detail="),
                "{sink:?} {args:?}: {ran:?}"
            );
            // render's summary is not logged for a program that never got out
            assert!(!ran.logged("result="), "{sink:?} {args:?}: {ran:?}");
        }
        // check's verdicts that print without asking the node fail the same way
        let table = bed.nft.table()?.ok_or("no table after the sync")?;
        bed.nft.delete_table()?;
        let ran = bed.run_to(sink, &["check"])?;
        assert_eq!(ran.code, Some(2), "{sink:?} absent: {ran:?}");
        assert!(
            ran.closing().starts_with("error reason=output-failed "),
            "{sink:?} absent: {ran:?}"
        );
        bed.nft.set_table(&table)?;

        // --version prints no log line at all
        let ran = run_binary(sink, &["--version".to_owned()])?;
        assert_eq!(ran.code, Some(2), "{sink:?} --version: {ran:?}");
        assert_eq!(ran.stderr, "", "{sink:?} --version");
    }
    assert_eq!(bed.contents()?, before);
    Ok(())
}

#[test]
fn a_check_whose_closing_line_is_stale_still_ends_with_output_failed() -> TestResult {
    let mut bed = Bed::new("output-failed-stale")?;
    assert_eq!(bed.sync(answers(&two_keys(), 86400))?.code, Some(0));
    for sink in FAILING {
        bed.rpc.set(vec![Response::Down])?;
        let ran = bed.run_to(sink, &["check"])?;
        assert_eq!(ran.code, Some(2), "{sink:?}: {ran:?}");
        let lines = ran.stderr.lines().collect::<Vec<_>>();
        let [.., stale, closing] = lines.as_slice() else {
            return Err(format!("too few lines: {ran:?}").into());
        };
        // the reason word the deployment reads is still there, just before the failure
        assert_eq!(*stale, "stale reason=rpc-unreachable", "{sink:?}");
        assert!(
            closing.starts_with("error reason=output-failed detail="),
            "{sink:?}: {ran:?}"
        );
    }
    Ok(())
}

#[test]
fn sync_ignores_a_stdout_that_cannot_be_written_and_exits_by_its_result() -> TestResult {
    for sink in FAILING {
        let mut bed = Bed::new("sync-output")?;
        let ran = bed.sync_to(sink, answers(&two_keys(), 86400))?;
        assert_eq!(ran.code, Some(0), "{sink:?}: {ran:?}");
        assert!(!ran.logged("error"), "{sink:?}: {ran:?}");
        assert_eq!(bed.status_value("RESULT")?, "applied", "{sink:?}");
        assert_eq!(bed.read("current.nft")?, Some(two_keys_program()?));

        let ran = bed.sync_to(sink, answers(&two_keys(), 86400))?;
        assert_eq!(ran.code, Some(0), "{sink:?}: {ran:?}");
        assert_eq!(bed.status_value("RESULT")?, "unchanged", "{sink:?}");

        let ran = bed.sync_to(sink, vec![Response::Down])?;
        assert_eq!(ran.code, Some(3), "{sink:?}: {ran:?}");
        assert_eq!(ran.closing(), "stale reason=rpc-unreachable action=kept");
        assert_eq!(bed.status_value("RESULT")?, "stale", "{sink:?}");

        // a refusal under --require-authoritative too: exit 1, nothing written
        let before = bed.contents()?;
        bed.rpc.set(vec![Response::Down])?;
        let ran = bed.run_to(sink, &["sync", "--require-authoritative"])?;
        assert_eq!(ran.code, Some(1), "{sink:?}: {ran:?}");
        assert_eq!(ran.closing(), "refused reason=rpc-unreachable");
        assert_eq!(bed.contents()?, before, "{sink:?}");
    }
    Ok(())
}

// ---- capture-floor ----

#[test]
fn a_capture_floor_whose_write_fails_exits_2_and_keeps_the_old_floor() -> TestResult {
    let bed = Bed::new("capture-write")?;
    let peers = bed.outside(
        "peers.yaml",
        b"k1:\n  primary:\n    network_address: /ip4/192.0.2.10/udp/49590/quic-v1\n",
    )?;
    // `floor` a directory: the rename of the written temporary file onto it fails
    fs::remove_file(bed.file("floor"))?;
    fs::create_dir(bed.file("floor"))?;
    let ran = bed.run(&["capture-floor", &peers])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert!(
        ran.stderr.starts_with("capture-floor: state-io-failed: ")
            && ran.stderr.ends_with('\n')
            && ran.stderr.lines().count() == 1,
        "{ran:?}"
    );
    assert!(bed.file("floor").is_dir());
    assert_eq!(bed.names()?, ["config", "floor", "lock"]);

    // the temporary name blocked: the floor file on disk stays as it was
    fs::remove_dir(bed.file("floor"))?;
    bed.write("floor", "# floor 1\n198.51.100.1\n")?;
    bed.block("floor")?;
    let ran = bed.run(&["capture-floor", &peers])?;
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert!(
        ran.stderr.starts_with("capture-floor: state-io-failed: ")
            && ran.stderr.contains("a directory is in the way")
            && ran.stderr.lines().count() == 1,
        "{ran:?}"
    );
    assert_eq!(
        bed.read("floor")?.as_deref(),
        Some("# floor 1\n198.51.100.1\n")
    );
    Ok(())
}
