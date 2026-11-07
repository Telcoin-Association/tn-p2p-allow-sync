//! Integration tests of what the built binary reads and how it reports it: special files where the state dir's files belong, the inputs of `capture-floor`, the closing log line of a run with many warnings, and `--state-dir` values that end in `/`, `.` or `..`.
//!
//! Each test gets its own directory holding the state dir, mode 0700, a link to `tests/support/fake-nft.sh` and a [`FakeRpc`].
//! Every run of the binary is bounded by [`RUN_LIMIT`], so an input that would block a run fails its test instead of hanging the suite.

mod support;

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use support::fake_rpc::{self, FakeRpc, Response};
use support::{FakeNft, TempDir, read_optional};
use tn_p2p_allow_sync::cli::USAGE;
use tn_p2p_allow_sync::config::MAX_CAPTURE_INPUT;

type TestResult = Result<(), Box<dyn Error>>;
type Res<T> = Result<T, Box<dyn Error>>;

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_tn-p2p-allow-sync");

/// The epoch info every good run asks for after the snapshot.
const EPOCH_INFO: &str = r#"{"epochId":42,"epochDuration":86400}"#;

/// The longest one run may take: scanning a 16 MiB input takes seconds in a debug build, and a run blocked on a FIFO never ends.
const RUN_LIMIT: Duration = Duration::from_secs(60);

/// How the detail of a refused special file, a directory or a FIFO reached through a symlink ends, as the log quotes it.
const NOT_REGULAR: &str = ": cannot read: is not a regular file\"";

/// A static list that warns twice on every run: a catch-all, and the two networks it covers.
const COVERING: &str = "0.0.0.0/0,10.0.0.0/8,10.3.0.0/16";

/// The first six warnings of a run with [`COVERING`], no floor and a [`faulty`] snapshot, in the order they come.
const FIRST_SIX: [&str; 6] = [
    "nft-path path=",
    "floor-missing detail=",
    "static-catch-all detail=",
    "static-covered dropped=2",
    "absorbed kind=unknown-slots count=1",
    "absorbed kind=uppercase-keys count=1",
];

/// What one run of the binary did.
#[derive(Debug)]
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn lines(&self) -> Vec<&str> {
        self.stderr.lines().collect()
    }

    /// The last line on stderr, empty when there is none.
    fn last(&self) -> &str {
        self.stderr.lines().next_back().unwrap_or_default()
    }

    /// Checks the exit code and the whole of stdout.
    fn ended(&self, code: i32, stdout: &str) {
        let ended = (self.code, self.stdout.as_str());
        assert_eq!(ended, (Some(code), stdout), "{self:?}");
    }

    /// Checks that the closing line starts with `start`.
    fn closes_with(&self, start: &str) {
        assert!(self.last().starts_with(start), "{self:?}");
    }

    /// Checks that the closing line starts with `start` and names a file that is not a regular file.
    fn refused(&self, start: &str) {
        self.closes_with(start);
        assert!(self.last().ends_with(NOT_REGULAR), "{self:?}");
    }
}

/// Runs the binary with `args` and `JOURNAL_STREAM` removed, and kills it once it has run for [`RUN_LIMIT`].
///
/// The output is collected after the exit, so it must fit the pipe buffers; every run here prints a few KiB at most.
fn run(args: &[OsString]) -> Res<Ran> {
    // the binary under test is the one program these tests run, besides mkfifo
    #[allow(clippy::disallowed_methods)]
    let mut command = Command::new(BIN);
    let mut child = command
        .args(args)
        // stderr is a pipe, never the journal
        .env_remove("JOURNAL_STREAM")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now()
        .checked_add(RUN_LIMIT)
        .ok_or("the deadline is out of range")?;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("the run blocked: {args:?}").into());
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output()?;
    Ok(Ran {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

/// A directory entry as a run may leave it: its mode with the file type, inode, modification time and, for a regular file, its bytes.
type Stamp = (u32, u64, (i64, i64), Option<Vec<u8>>);

/// Every entry of `dir` by name, never following a symlink or opening anything but a regular file, since opening a FIFO blocks.
fn entries(dir: &Path) -> Res<BTreeMap<String, Stamp>> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        let bytes = meta.is_file().then(|| fs::read(&path)).transpose()?;
        let modified = (meta.mtime(), meta.mtime_nsec());
        let name = path.file_name().ok_or("no name")?.to_string_lossy();
        entries.insert(
            name.into_owned(),
            (meta.mode(), meta.ino(), modified, bytes),
        );
    }
    Ok(entries)
}

/// A special file the tests put where a regular file belongs.
#[derive(Debug, Clone, Copy)]
enum Special {
    Fifo,
    Socket,
}

/// Puts a special file of `kind` at `path`; false when the host has no `mkfifo` to make a FIFO with.
fn make_special(kind: Special, path: &Path) -> Res<bool> {
    match kind {
        Special::Fifo => mkfifo(path),
        Special::Socket => {
            // the socket file stays after the listener closes, and opening it then fails with ENXIO
            drop(UnixListener::bind(path)?);
            Ok(true)
        }
    }
}

/// Makes a FIFO at `path`; false when `mkfifo` is not on `PATH`.
fn mkfifo(path: &Path) -> Res<bool> {
    // std makes no FIFO without libc and the tests take no dependency, so the coreutils tool makes it
    #[allow(clippy::disallowed_methods)]
    let status = match Command::new("mkfifo").arg(path).status() {
        Ok(status) => status,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !status.success() {
        return Err(format!("mkfifo {path:?}: {status}").into());
    }
    Ok(true)
}

/// Writes a file of exactly `len` bytes at `path`: a hole, then `tail` as its last bytes.
fn sparse_file(path: &Path, len: u64, tail: &[u8]) -> TestResult {
    let start = len
        .checked_sub(u64::try_from(tail.len())?)
        .ok_or("the tail is longer than the file")?;
    let mut file = fs::File::create(path)?;
    file.set_len(len)?;
    file.seek(SeekFrom::Start(start))?;
    file.write_all(tail)?;
    assert_eq!(file.metadata()?.len(), len);
    Ok(())
}

/// Checks that `ran` hit the log cap: [`FIRST_SIX`] written, the seventh line counting `dropped` lines, and returns the eighth, the closing line.
fn capped(ran: &Ran, dropped: usize) -> String {
    let lines = ran.lines();
    assert_eq!(lines.len(), 8, "{ran:?}");
    for (line, start) in lines.iter().zip(FIRST_SIX) {
        assert!(line.starts_with(start), "{start}: {ran:?}");
    }
    let count = format!("log-capped dropped={dropped}");
    assert_eq!(lines.get(6), Some(&count.as_str()), "{ran:?}");
    ran.last().to_owned()
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
    fn config(&self, allow_from: &str) -> TestResult {
        let path = self.file("config");
        let port = self.rpc.port();
        let text = format!(
            "CONFIG_VERSION=1\nRPC_PORT={port}\nP2P_PORTS=49590,49594\nP2P_ALLOW_FROM={allow_from}\n"
        );
        fs::write(&path, text)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// Runs the binary with `--state-dir state_dir`, `--nft` set to the fake and `args` after them.
    fn run_at(&self, state_dir: &OsStr, args: &[&str]) -> Res<Ran> {
        let mut all = vec![OsString::from("--state-dir"), state_dir.to_owned()];
        all.extend([OsString::from("--nft"), self.nft.path().into_os_string()]);
        all.extend(args.iter().map(OsString::from));
        run(&all)
    }

    fn run(&self, args: &[&str]) -> Res<Ran> {
        self.run_at(self.state.as_os_str(), args)
    }

    /// Queues `responses` on the node, then runs `sync` with `args`.
    fn sync(&mut self, responses: Vec<Response>, args: &[&str]) -> Res<Ran> {
        self.rpc.set(responses)?;
        self.run(&[&["sync"][..], args].concat())
    }

    /// A sync of `snap` that must apply.
    fn applied(&mut self, snap: &Value) -> TestResult {
        let ran = self.sync(answers(snap), &[])?;
        assert!(ran.stdout.starts_with("result=applied "), "{ran:?}");
        Ok(())
    }

    /// Queues `snap` on the node, then runs `args`.
    fn asking(&mut self, snap: &Value, args: &[&str]) -> Res<Ran> {
        self.rpc.set(vec![fake_rpc::result(&snap.to_string())])?;
        self.run(args)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.state.join(name)
    }

    fn entries(&self) -> Res<BTreeMap<String, Stamp>> {
        entries(&self.state)
    }

    /// The nft calls made since the first `since`.
    fn calls_since(&self, since: usize) -> Res<Vec<String>> {
        Ok(self.nft.argv()?.into_iter().skip(since).collect())
    }

    /// One value of the `status` file, which must be a regular file or missing; empty when the key or the file is missing.
    fn status_value(&self, key: &str) -> Res<String> {
        let text = read_optional(&self.file("status"))?.unwrap_or_default();
        let value = text
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='));
        Ok(value.unwrap_or_default().to_owned())
    }

    /// Checks that nothing reached nft or the node.
    fn untouched(&self) -> TestResult {
        assert_eq!(self.nft.argv()?, Vec::<String>::new());
        assert_eq!(self.rpc.requests(), Vec::<String>::new());
        Ok(())
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

/// A dns4 multiaddr standing in for an IPv4 literal, which the tool never resolves.
fn dns4(ip: &str) -> String {
    format!("/dns4/{ip}.example/udp/49590/quic-v1")
}

/// An entry for key `n` with these multiaddrs, resolved from its record.
fn entry(n: u8, multiaddrs: &[String]) -> Value {
    json!({
        "bls_pubkey": key(n), "slots": ["current", "eligible"], "peer_id": null, "network_pubkey": null,
        "multiaddrs": multiaddrs, "record_timestamp": 1_793_750_400_u64, "source": "record", "admitted": true,
    })
}

/// A snapshot whose one swarm, an available primary, tracks epoch `tracked`, with its pool published for `published`.
fn snapshot(tracked: u64, published: u64, entries: Vec<Value>) -> Value {
    let admission = json!({
        "configured": "Closed", "effective": "Closed", "epoch": published, "fallback": null,
        "resolved_current": 4, "required_current": 3, "connected_current": 3, "contradicted": 0,
    });
    let primary = json!({
        "network": "primary", "worker_id": null, "available": true, "tracked_epoch": tracked,
        "admission": admission, "entries": entries,
    });
    json!({"version": 1, "epoch": tracked, "swarms": [primary]})
}

/// Two keys at `epoch`: 0x11 at 203.0.113.10 and 0x22 at 198.51.100.7.
fn two_keys_at(epoch: u64) -> Value {
    let entries = vec![
        entry(0x11, &[ip4("203.0.113.10")]),
        entry(0x22, &[ip4("198.51.100.7")]),
    ];
    snapshot(epoch, epoch, entries)
}

fn two_keys() -> Value {
    two_keys_at(42)
}

/// The node's answers to a good run: the snapshot, then the epoch info.
fn answers(snap: &Value) -> Vec<Response> {
    vec![
        fake_rpc::result(&snap.to_string()),
        fake_rpc::result(EPOCH_INFO),
    ]
}

/// The node's answers to a run that warns twelve times: a [`faulty`] snapshot, then an epoch info it does not know.
fn faulty_answers(address: fn(&str) -> String) -> Res<Vec<Response>> {
    let snap = faulty(address)?.to_string();
    Ok(vec![fake_rpc::result(&snap), fake_rpc::error(-32601)])
}

/// A snapshot at epoch 43 holding one fault of each kind the parser absorbs, its addresses written by `address`.
///
/// An unknown slot, a dns4 and an unparsable multiaddr, a list of 33 addresses, a key listed twice, an uppercase key, and a pool published for epoch 42 only.
fn faulty(address: fn(&str) -> String) -> Res<Value> {
    let long: Vec<String> = (1..=33_u8)
        .map(|n| address(&format!("192.0.2.{n}")))
        .collect();
    let odd_addresses = [
        address("198.51.100.1"),
        "/dns4/validator.example/udp/49590/quic-v1".to_owned(),
        "not-a-multiaddr".to_owned(),
    ];
    let mut odd = entry(0x11, &odd_addresses);
    let slots = json!(["current", "observer"]);
    odd.as_object_mut()
        .ok_or("not an object")?
        .insert("slots".to_owned(), slots);
    let upper = json!({
        "bls_pubkey": format!("0x{:02X}{}", 0xab, "0".repeat(190)), "slots": ["eligible"],
        "multiaddrs": [address("198.51.100.3")], "source": "record", "admitted": true,
    });
    let entries = vec![
        odd,
        entry(0x22, &long),
        entry(0x22, &[address("198.51.100.2")]),
        upper,
    ];
    Ok(snapshot(43, 42, entries))
}

/// A FIFO and a socket in turn at `name`, after a good sync: `sync` refuses with `token`, exit 2, keeps the table and writes only `status`; `check`, `render` and `status` read the same dir without blocking and write nothing.
fn special_at(name: &str, token: &str) -> TestResult {
    for kind in [Special::Fifo, Special::Socket] {
        let mut bed = Bed::new(&format!("special-{name}"))?;
        bed.applied(&two_keys())?;
        let path = bed.file(name);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        if !make_special(kind, &path)? {
            println!("skipped {kind:?} at {name}: mkfifo is not on PATH");
            continue;
        }
        let table = bed.nft.table()?;
        let last_good = bed.status_value("LAST_GOOD")?;
        let mut before = bed.entries()?;
        let calls = bed.nft.argv()?.len();
        let asked = bed.rpc.requests().len();

        let ran = bed.sync(answers(&two_keys()), &[])?;
        let line = format!(
            "result=error reason={token} epoch=42 v4=2 v6=0 static=0 floor=0 keys=2 dns_skipped=0 unparsed=0\n"
        );
        ran.ended(2, &line);
        // the type is checked before the open, so a FIFO and a socket read alike
        ran.refused(&format!("error reason={token} action=kept detail="));
        // refused before the node is asked; the stale path only looks for the table and keeps it
        assert_eq!(bed.rpc.requests().len(), asked);
        assert_eq!(bed.calls_since(calls)?, ["-j list tables inet"]);
        assert_eq!(bed.nft.table()?, table);
        // the special file, current.nft, state.json and lock stay as they were; status is replaced
        let mut after = bed.entries()?;
        let old = before.remove("status").map(|stamp| stamp.1);
        let new = after.remove("status").map(|stamp| stamp.1);
        assert!(new.is_some() && new != old, "{kind:?}: status was kept");
        assert_eq!(after, before, "{kind:?}");
        assert_eq!(bed.status_value("RESULT")?, "error");
        assert_eq!(bed.status_value("STALE_REASON")?, token);
        assert_eq!(bed.status_value("EPOCH")?, "42");
        assert_eq!(bed.status_value("LAST_GOOD")?, last_good);

        let before = bed.entries()?;
        // check reads config, status and state.json, never the floor, and an unreadable state.json is no verdict of its own
        let ran = bed.asking(&two_keys(), &["check"])?;
        if name == "config" {
            ran.ended(2, "");
            ran.refused("error reason=config-invalid detail=");
        } else {
            ran.ended(0, "result=ok missing_total=0 mismatch_total=0\n");
            assert_eq!(ran.lines().len(), 1, "{ran:?}");
        }
        // render reads all three
        let ran = bed.asking(&two_keys(), &["render"])?;
        ran.ended(2, "");
        ran.refused(&format!("error reason={token} detail="));
        // status reads only the status file
        let ran = bed.run(&["status"])?;
        assert_eq!(ran.code, Some(0), "{ran:?}");
        let reason = format!("STALE_REASON={token}");
        for line in ["RESULT=error", reason.as_str(), "EPOCH=42", "LOADED=true"] {
            assert!(ran.stdout.lines().any(|l| l == line), "{line}: {ran:?}");
        }
        assert_eq!(bed.entries()?, before, "{kind:?}");
    }
    Ok(())
}

#[test]
fn a_fifo_or_a_socket_at_config_is_config_invalid_without_blocking() -> TestResult {
    special_at("config", "config-invalid")
}

#[test]
fn a_fifo_or_a_socket_at_floor_is_floor_invalid_without_blocking() -> TestResult {
    special_at("floor", "floor-invalid")
}

/// The contract's reason table: a special file where a file should be is `state-io-failed`, at `config` and `floor` excepted; `state-invalid` is for bytes that are not in the tool's form.
#[test]
fn a_fifo_or_a_socket_at_state_json_is_state_io_failed_without_blocking() -> TestResult {
    special_at("state.json", "state-io-failed")
}

#[test]
fn capture_floor_refuses_a_special_input_or_a_directory_without_blocking() -> TestResult {
    let bed = Bed::new("capture-special")?;
    let good = bed.temp.join("good.yaml");
    fs::write(&good, "- /ip4/192.0.2.50/udp/49590/quic-v1\n")?;
    let good = good.to_string_lossy().into_owned();
    let socket = bed.temp.join("socket.yaml");
    make_special(Special::Socket, &socket)?;
    let directory = bed.temp.join("directory.yaml");
    fs::create_dir(&directory)?;
    let not_regular = "cannot read: is not a regular file";
    // a socket cannot be opened, so the open's own error is the detail
    let mut refused = vec![
        (socket, "cannot read: No such device or address"),
        (directory, not_regular),
    ];
    let fifo = bed.temp.join("fifo.yaml");
    if mkfifo(&fifo)? {
        // a symlink is followed, and what it reaches is checked on the open file
        let link = bed.temp.join("link.yaml");
        symlink(&fifo, &link)?;
        refused.extend([(fifo, not_regular), (link, not_regular)]);
    } else {
        println!("skipped the FIFO: mkfifo is not on PATH");
    }
    let before = bed.entries()?;
    for (input, detail) in refused {
        // one refused input refuses the whole run, the good one before it included
        let ran = bed.run(&["capture-floor", &good, &input.to_string_lossy()])?;
        ran.ended(2, "");
        let start = format!("capture-floor: floor-invalid: {input:?}: {detail}");
        assert!(ran.stderr.starts_with(&start), "{ran:?}");
        assert_eq!(ran.lines().len(), 1, "{ran:?}");
        // the inputs are read before the lock is taken, so not even lock is created
        assert_eq!(bed.entries()?, before);
    }
    Ok(())
}

#[test]
fn capture_floor_takes_exactly_16_mib_through_a_symlink_and_refuses_one_byte_more() -> TestResult {
    let bed = Bed::new("capture-size")?;
    let tail = b"\n- /ip4/192.0.2.77/udp/49590/quic-v1\n";
    let larger = bed.temp.join("larger.yaml");
    let len = MAX_CAPTURE_INPUT.checked_add(1).ok_or("overflow")?;
    sparse_file(&larger, len, tail)?;
    let before = bed.entries()?;
    let ran = bed.run(&["capture-floor", &larger.to_string_lossy()])?;
    ran.ended(2, "");
    let detail = "cannot read: is larger than 16777216 bytes";
    let refused = format!("capture-floor: floor-invalid: {larger:?}: {detail}\n");
    assert_eq!(ran.stderr, refused);
    assert_eq!(bed.entries()?, before);

    // the inputs live in /etc/telcoin, so a symlinked one is followed
    let exact = bed.temp.join("exact.yaml");
    sparse_file(&exact, MAX_CAPTURE_INPUT, tail)?;
    let link = bed.temp.join("bootstrap-peers.yaml");
    symlink(&exact, &link)?;
    let ran = bed.run(&["capture-floor", &link.to_string_lossy()])?;
    ran.ended(0, "");
    assert_eq!(ran.stderr, "");
    let floor = bed.file("floor");
    let text = read_optional(&floor)?;
    assert_eq!(text.as_deref(), Some("# floor 1\n192.0.2.77\n"));
    let meta = fs::symlink_metadata(&floor)?;
    assert!(meta.is_file() && meta.mode() & 0o7777 == 0o600, "{meta:?}");
    let names: Vec<String> = bed.entries()?.into_keys().collect();
    assert_eq!(names, ["config", "floor", "lock"]);
    Ok(())
}

#[test]
fn check_closes_with_its_verdict_after_every_warning_it_gives() -> TestResult {
    let mut bed = Bed::new("check-closing")?;
    bed.applied(&two_keys())?;
    // a socket at status cannot be read: check takes it for no status at all, with a warning
    let status = bed.file("status");
    fs::remove_file(&status)?;
    make_special(Special::Socket, &status)?;
    let warned = |ran: &Ran| {
        let lines = ran.lines();
        assert_eq!(lines.len(), 3, "{ran:?}");
        let first = lines
            .first()
            .is_some_and(|l| l.starts_with("nft-path path="));
        let second = lines.get(1).is_some_and(|l| {
            l.starts_with("status-unreadable detail=")
                && l.ends_with(&format!("status\\\"{NOT_REGULAR}"))
        });
        assert!(first && second, "{ran:?}");
    };
    let before = bed.entries()?;

    // no applied epoch: the snapshot is ahead of it, and applied_epoch is empty
    let ran = bed.asking(&two_keys(), &["check"])?;
    ran.ended(3, "result=behind missing_total=0 mismatch_total=0\n");
    warned(&ran);
    assert_eq!(ran.last(), "behind snapshot_epoch=42 applied_epoch=");

    // the node answers 503 to an empty queue
    bed.rpc.set(Vec::new())?;
    let ran = bed.run(&["check"])?;
    ran.ended(3, "result=stale missing_total=0 mismatch_total=0\n");
    warned(&ran);
    assert_eq!(ran.last(), "stale reason=rpc-http-503");

    bed.nft.knob("eperm")?;
    let ran = bed.run(&["check"])?;
    ran.ended(2, "");
    warned(&ran);
    ran.closes_with("error reason=nft-unavailable detail=");
    bed.nft.clear_knob("eperm")?;
    assert_eq!(bed.entries()?, before);

    // with the status readable again: a snapshot below the applied epoch
    fs::remove_file(&status)?;
    // the same addresses: the table is found loaded and only the epoch moves
    let ran = bed.sync(answers(&two_keys_at(43)), &[])?;
    let unchanged = ran.stdout.starts_with("result=unchanged reason= epoch=43 ");
    assert!(unchanged, "{ran:?}");
    let ran = bed.asking(&two_keys(), &["check"])?;
    ran.ended(3, "result=stale missing_total=0 mismatch_total=0\n");
    assert_eq!(ran.lines().len(), 2, "{ran:?}");
    let regressed = "stale reason=epoch-regressed snapshot_epoch=42 applied_epoch=43";
    assert_eq!(ran.last(), regressed);
    Ok(())
}

#[test]
fn a_failed_load_after_twelve_warnings_writes_six_counts_six_and_ends_with_nft_words() -> TestResult
{
    let mut bed = Bed::new("load-closing")?;
    bed.config(COVERING)?;
    bed.applied(&two_keys())?;
    let table = bed.nft.table()?;
    let mut before = bed.entries()?;
    bed.nft.knob("fail-next")?;
    // twelve warnings: the nft path, the floor, the static list twice, seven kinds of
    // absorbed fault and the epoch info; the last six of them are dropped
    let ran = bed.sync(faulty_answers(ip4)?, &[])?;
    ran.ended(
        2,
        "result=error reason=nft-apply-failed epoch=42 v4=2 v6=0 static=1 floor=0 keys=2 dns_skipped=0 unparsed=0\n",
    );
    let last = capped(&ran, 6);
    let nft_words = "error reason=nft-apply-failed action=kept detail=\"nft exited with status 1: ";
    assert!(last.starts_with(nft_words), "{ran:?}");
    assert!(!bed.nft.has_knob("fail-next"));
    assert_eq!(bed.nft.table()?, table);
    let mut after = bed.entries()?;
    before.remove("status");
    after.remove("status");
    assert_eq!(after, before);
    assert_eq!(bed.status_value("STALE_REASON")?, "nft-apply-failed");
    Ok(())
}

#[test]
fn a_refusal_closes_after_every_warning_and_writes_nothing_but_the_lock() -> TestResult {
    let mut bed = Bed::new("refused-closing")?;
    bed.config(COVERING)?;
    // names only, so memory holds nothing and the snapshot is refused as an empty set
    let ran = bed.sync(faulty_answers(dns4)?, &["--require-authoritative"])?;
    ran.ended(
        1,
        "result=stale reason=empty-set epoch= v4=0 v6=0 static=0 floor=0 keys=0 dns_skipped=0 unparsed=0\n",
    );
    assert_eq!(capped(&ran, 6), "refused reason=empty-set");
    assert_eq!(bed.nft.argv()?, ["-j list tables inet"]);
    assert_eq!(bed.nft.table()?, None);
    let names: Vec<String> = bed.entries()?.into_keys().collect();
    assert_eq!(names, ["config", "lock"]);
    Ok(())
}

/// docs/contract.md §render still says the summary "can lose it to log-capped"; the closing-line rule of v0.1.1 keeps it, last.
#[test]
fn render_closes_with_its_summary_after_every_warning() -> TestResult {
    let bed = Bed::new("render-closing")?;
    bed.config(COVERING)?;
    let file = bed.temp.join("snapshot.json");
    let file_arg = file.to_string_lossy().into_owned();
    // eleven warnings: render asks for no epoch info, so five are dropped
    let response = json!({"jsonrpc": "2.0", "id": 1, "result": faulty(ip4)?});
    fs::write(&file, response.to_string())?;
    let before = bed.entries()?;
    let ran = bed.run(&["render", "--snapshot", &file_arg])?;
    assert_eq!(ran.code, Some(0), "{ran:?}");
    let catch_all = "add element inet tn_p2p_allow static_v4 { 0.0.0.0/0 }\n";
    assert!(ran.stdout.contains(catch_all), "{ran:?}");
    // the list of 33 is read up to 32, and the dns4 and the unparsable multiaddr give nothing
    assert_eq!(
        capped(&ran, 5),
        "result=rendered reason= epoch=43 v4=35 v6=0 static=1 floor=0 keys=3 dns_skipped=1 unparsed=1"
    );

    // the bare result object this time, with names only: the stale summary closes the run
    fs::write(&file, faulty(dns4)?.to_string())?;
    let ran = bed.run(&["render", "--snapshot", &file_arg])?;
    ran.ended(1, "");
    assert_eq!(
        capped(&ran, 5),
        "result=stale reason=empty-set epoch=0 v4=0 v6=0 static=1 floor=0 keys=0 dns_skipped=36 unparsed=1"
    );
    assert_eq!(bed.entries()?, before);
    bed.untouched()
}

/// `dir` followed by each of `suffixes` in turn.
fn forms(dir: &Path, suffixes: &[&str]) -> Vec<OsString> {
    let with = |suffix: &&str| {
        let mut form = dir.as_os_str().to_owned();
        form.push(suffix);
        form
    };
    suffixes.iter().map(with).collect()
}

#[test]
fn a_trailing_slash_on_the_state_dir_changes_nothing() -> TestResult {
    let mut bed = Bed::new("slash")?;
    bed.applied(&two_keys())?;
    let snap = fake_rpc::result(&two_keys().to_string());
    for (command, responses) in [
        ("sync", answers(&two_keys())),
        ("check", vec![snap.clone()]),
        ("render", vec![snap]),
        ("status", Vec::new()),
    ] {
        let mut seen = Vec::new();
        for form in forms(&bed.state, &["", "/", "//"]) {
            bed.rpc.set(responses.clone())?;
            let calls = bed.nft.argv()?.len();
            let ran = bed.run_at(&form, &[command])?;
            assert_eq!(ran.code, Some(0), "{command} {form:?}: {ran:?}");
            // AGE_SECS counts the seconds since the last good sync, which the runs may straddle
            let stdout: Vec<String> = ran
                .stdout
                .lines()
                .filter(|line| !line.starts_with("AGE_SECS="))
                .map(str::to_owned)
                .collect();
            seen.push((stdout, ran.stderr, bed.calls_since(calls)?));
        }
        let first = seen.first().ok_or("no run")?;
        let same = seen.iter().all(|other| other == first);
        assert!(same, "{command}: {seen:?}");
        if command == "sync" {
            let line = first.0.first().ok_or("no sync line")?;
            assert!(line.starts_with("result=unchanged "), "{line}");
        }
    }
    Ok(())
}

#[test]
fn a_state_dir_ending_in_dot_or_dot_dot_is_a_usage_error_and_touches_nothing() -> TestResult {
    let bed = Bed::new("dot")?;
    let peers = bed.temp.join("peers.yaml");
    fs::write(&peers, "- /ip4/192.0.2.50/udp/49590/quic-v1\n")?;
    let peers = peers.to_string_lossy().into_owned();
    let before = (bed.entries()?, entries(bed.temp.path())?);
    for form in forms(&bed.state, &["/.", "/..", "/./", "/..//"]) {
        for args in [
            &["sync"][..],
            &["check"],
            &["status"],
            &["render"],
            &["capture-floor", &peers],
        ] {
            let ran = bed.run_at(&form, args)?;
            ran.ended(2, "");
            assert_eq!(ran.stderr, USAGE, "{form:?} {args:?}");
        }
    }
    assert_eq!((bed.entries()?, entries(bed.temp.path())?), before);
    bed.untouched()
}

#[test]
fn a_symlinked_state_dir_is_refused_with_or_without_a_trailing_slash() -> TestResult {
    let bed = Bed::new("link")?;
    let link = bed.temp.join("link");
    symlink(&bed.state, &link)?;
    let peers = bed.temp.join("peers.yaml");
    fs::write(&peers, "- /ip4/192.0.2.50/udp/49590/quic-v1\n")?;
    let before = bed.entries()?;
    // the trailing slashes are dropped, so the checks see the link itself, never the dir it names
    let refused = format!("state dir {link:?}: is a symlink");
    let logged = format!("error reason=state-dir-invalid detail={refused:?}");
    for form in forms(&link, &["", "/", "//"]) {
        for command in ["sync", "check", "status", "render"] {
            let ran = bed.run_at(&form, &[command])?;
            ran.ended(2, "");
            let lines = ran.lines();
            let warned = lines
                .first()
                .is_some_and(|l| l.starts_with("nft-path path="));
            assert!(warned && lines.len() == 2, "{command} {form:?}: {ran:?}");
            assert_eq!(ran.last(), logged, "{command} {form:?}");
        }
        let ran = bed.run_at(&form, &["capture-floor", &peers.to_string_lossy()])?;
        ran.ended(2, "");
        let expected = format!("capture-floor: state-dir-invalid: {refused}\n");
        assert_eq!(ran.stderr, expected);
    }
    assert_eq!(bed.entries()?, before);
    bed.untouched()
}
