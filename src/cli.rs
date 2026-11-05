//! Command line: argument parsing, the usage text, the `--version` lines and the exit codes.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config;
use crate::fsutil::{FsError, Lock, StateDir};
use crate::log::{self, Log, Priority};
use crate::nft::{self, Nft};
use crate::sync::{self, RenderOptions, SyncOptions};

/// Usage text, printed to stderr on a usage error.
pub const USAGE: &str = "\
usage: tn-p2p-allow-sync [--state-dir DIR] [--nft PATH] COMMAND
  sync [--require-authoritative] [--wait SECS]
  check
  status [--json]
  render [--snapshot FILE] [--check]
  capture-floor FILE...
  --version
";

/// Versions of the `tn_admissionSnapshot` result this build accepts, printed space-separated on the `snapshot-versions` line.
pub const SNAPSHOT_VERSIONS: &[u64] = &[1];

/// Version of the config file this build reads, printed on the `config-version` line.
pub const CONFIG_VERSION: u64 = 1;

/// The state dir when `--state-dir` is not given.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/telcoin-p2p-allow";

/// How long `capture-floor` waits for the lock.
///
/// The command takes no `--wait`, and the scripts run it right before a `sync --wait 30`, so it waits as long as that sync would.
pub const CAPTURE_FLOOR_WAIT: Duration = Duration::from_secs(30);

/// The commit this binary was built from: `TN_P2P_ALLOW_GIT_SHA` at build time, or `None` when it was unset or empty.
const COMMIT: Option<&str> = match option_env!("TN_P2P_ALLOW_GIT_SHA") {
    Some(sha) if !sha.is_empty() => Some(sha),
    _ => None,
};

// a build that sets the variable to anything but a full commit hash fails here, rather than
// shipping a `commit` line that the release checks reject only after the build
const _: () = assert!(
    match COMMIT {
        Some(sha) => is_commit_hash(sha),
        None => true,
    },
    "TN_P2P_ALLOW_GIT_SHA must be a full commit hash: 40 lowercase hex digits"
);

/// The target triple this binary was compiled for, printed on the `target` line.
const TARGET: &str = if cfg!(all(
    target_arch = "x86_64",
    target_vendor = "unknown",
    target_os = "linux",
    target_env = "musl"
)) {
    "x86_64-unknown-linux-musl"
} else if cfg!(all(
    target_arch = "x86_64",
    target_vendor = "unknown",
    target_os = "linux",
    target_env = "gnu"
)) {
    "x86_64-unknown-linux-gnu"
} else if cfg!(all(
    target_arch = "aarch64",
    target_vendor = "unknown",
    target_os = "linux",
    target_env = "musl"
)) {
    "aarch64-unknown-linux-musl"
} else if cfg!(all(
    target_arch = "aarch64",
    target_vendor = "unknown",
    target_os = "linux",
    target_env = "gnu"
)) {
    "aarch64-unknown-linux-gnu"
} else {
    "unknown"
};

/// How a run ended.
///
/// [`Exit::code`] maps each outcome to the process exit status of the CLI contract.
/// A panic is not an outcome: it unwinds out of `main` and exits 101, which is always a bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exit 0, fresh.
    /// `sync` applied the program or found it already loaded, `check` found the table consistent, and `status`, `render` and `capture-floor` finished.
    Fresh,
    /// Exit 1, refused with nothing written, not even `status`.
    /// `sync --require-authoritative` got a snapshot that is not authoritative or a dynamic set that is empty, `check` found an address missing or the table absent, and `render` got a snapshot that is not authoritative.
    Refused,
    /// Exit 2, error.
    /// Usage, config, floor or state is invalid, the state dir is unsafe, nft is missing or answers EPERM, or the apply failed.
    Error,
    /// Exit 3, stale.
    /// `sync` kept the last good program, and `check` cannot judge because the RPC is down or the sync is behind the snapshot's epoch.
    Stale,
    /// Exit 4, busy: another run holds the lock.
    Busy,
}

impl Exit {
    /// The process exit status of this outcome: 0, 1, 2, 3 or 4.
    pub const fn code(self) -> u8 {
        match self {
            Self::Fresh => 0,
            Self::Refused => 1,
            Self::Error => 2,
            Self::Stale => 3,
            Self::Busy => 4,
        }
    }
}

/// A parsed command line: the global flags and the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// `--state-dir DIR`, or [`DEFAULT_STATE_DIR`].
    pub state_dir: PathBuf,
    /// `--nft PATH`, or `None` for the fixed default.
    pub nft: Option<PathBuf>,
    /// The command and its flags.
    pub command: Command,
}

/// A command and its flags, as the usage text lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `sync [--require-authoritative] [--wait SECS]`.
    Sync {
        /// `--require-authoritative`: refuse with exit 1, writing nothing, unless the snapshot is authoritative and the dynamic set is not empty.
        require_authoritative: bool,
        /// `--wait SECS`: how long to wait for the lock; `None` means not at all.
        wait: Option<u64>,
    },
    /// `check`.
    Check,
    /// `status [--json]`.
    Status {
        /// `--json`: one line, a JSON object.
        json: bool,
    },
    /// `render [--snapshot FILE] [--check]`.
    Render {
        /// `--snapshot FILE`: render from this file instead of the RPC.
        snapshot: Option<PathBuf>,
        /// `--check`: also run `nft -c -f -` on the program.
        check: bool,
    },
    /// `capture-floor FILE...`: one or more files to take the floor from.
    CaptureFloor(Vec<PathBuf>),
    /// `--version`.
    Version,
}

/// Parses the arguments after the program name, or `None` on a usage error.
///
/// The global flags come before the command, every flag at most once, and only the long forms of the usage text exist.
/// A flag value or a `capture-floor` file that starts with `-` or is empty is a usage error, so a forgotten value never swallows the next flag; write `./-name` for such a file.
/// `--wait` takes decimal digits only.
pub fn parse<I>(args: I) -> Option<Invocation>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let mut state_dir = None;
    let mut nft = None;
    let command = loop {
        let arg = args.next()?;
        match arg.to_str()? {
            "--state-dir" if state_dir.is_none() => state_dir = Some(path_value(args.next()?)?),
            "--nft" if nft.is_none() => nft = Some(path_value(args.next()?)?),
            "--version" => break Command::Version,
            "check" => break Command::Check,
            "sync" => break parse_sync(&mut args)?,
            "status" => break parse_status(&mut args)?,
            "render" => break parse_render(&mut args)?,
            "capture-floor" => break Command::CaptureFloor(parse_files(&mut args)?),
            _ => return None,
        }
    };
    // the commands that take no further arguments leave the iterator untouched
    if args.next().is_some() {
        return None;
    }
    Some(Invocation {
        state_dir: state_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)),
        nft,
        command,
    })
}

fn parse_sync(args: &mut impl Iterator<Item = OsString>) -> Option<Command> {
    let mut require_authoritative = false;
    let mut wait = None;
    while let Some(arg) = args.next() {
        match arg.to_str()? {
            "--require-authoritative" if !require_authoritative => require_authoritative = true,
            "--wait" if wait.is_none() => wait = Some(seconds(args.next()?)?),
            _ => return None,
        }
    }
    Some(Command::Sync {
        require_authoritative,
        wait,
    })
}

fn parse_status(args: &mut impl Iterator<Item = OsString>) -> Option<Command> {
    let mut json = false;
    for arg in args {
        match arg.to_str()? {
            "--json" if !json => json = true,
            _ => return None,
        }
    }
    Some(Command::Status { json })
}

fn parse_render(args: &mut impl Iterator<Item = OsString>) -> Option<Command> {
    let mut snapshot = None;
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.to_str()? {
            "--snapshot" if snapshot.is_none() => snapshot = Some(path_value(args.next()?)?),
            "--check" if !check => check = true,
            _ => return None,
        }
    }
    Some(Command::Render { snapshot, check })
}

fn parse_files(args: &mut impl Iterator<Item = OsString>) -> Option<Vec<PathBuf>> {
    let files = args.map(path_value).collect::<Option<Vec<_>>>()?;
    (!files.is_empty()).then_some(files)
}

/// A path argument: not empty and not starting with `-`.
fn path_value(arg: OsString) -> Option<PathBuf> {
    let bytes = arg.as_encoded_bytes();
    (!bytes.is_empty() && !bytes.starts_with(b"-")).then(|| PathBuf::from(arg))
}

/// A `--wait` value: decimal digits, no sign.
fn seconds(arg: OsString) -> Option<u64> {
    let text = arg.to_str()?;
    if text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

/// Runs the command line `args`, program name first as [`std::env::args_os`] yields it, and returns how the run ended.
///
/// Output goes to stdout and diagnostics to stderr; the log lines of `sync` and `render` carry journald priority prefixes when stderr is the journal.
/// A usage error prints [`USAGE`] to stderr and exits 2.
/// `--version`, `capture-floor`, `sync` and `render` are complete; `check` and `status` parse but exit 2 with "not implemented" for now.
pub fn run<I>(args: I) -> Exit
where
    I: IntoIterator<Item = OsString>,
{
    run_with(
        args,
        &mut io::stdout(),
        &mut io::stderr(),
        log::stderr_is_journal(),
    )
}

fn run_with<I, O, E>(args: I, out: &mut O, err: &mut E, journal: bool) -> Exit
where
    I: IntoIterator<Item = OsString>,
    O: Write,
    E: Write,
{
    let Some(invocation) = parse(args.into_iter().skip(1)) else {
        return usage(err);
    };
    match &invocation.command {
        Command::Version => write_version(out),
        Command::CaptureFloor(files) => {
            capture_floor(&invocation.state_dir, files, CAPTURE_FLOOR_WAIT, err)
        }
        Command::Sync {
            require_authoritative,
            wait,
        } => {
            let mut log = Log::new(err, journal);
            let Some(nft) = nft_runner(invocation.nft.as_deref(), &mut log) else {
                return Exit::Error;
            };
            let options = SyncOptions {
                state_dir: &invocation.state_dir,
                nft: &nft,
                require_authoritative: *require_authoritative,
                wait: wait.map(Duration::from_secs),
            };
            sync::sync(&options, out, &mut log)
        }
        Command::Render { snapshot, check } => {
            let mut log = Log::new(err, journal);
            let Some(nft) = nft_runner(invocation.nft.as_deref(), &mut log) else {
                return Exit::Error;
            };
            let options = RenderOptions {
                state_dir: &invocation.state_dir,
                nft: &nft,
                snapshot: snapshot.as_deref(),
                check: *check,
            };
            sync::render(&options, out, &mut log)
        }
        Command::Check => not_implemented("check", err),
        Command::Status { .. } => not_implemented("status", err),
    }
}

/// The nft runner for `--nft PATH`: the path must be absolute, and one other than [`nft::DEFAULT_PATH`] is warned about on every run.
///
/// A relative path would resolve against the working directory, or a bare name against the child's `PATH`, and could start another program than the one meant; it is an error, exit 2.
fn nft_runner<E: Write>(path: Option<&Path>, log: &mut Log<E>) -> Option<Nft> {
    if let Some(path) = path {
        let shown = path.to_string_lossy();
        if !path.is_absolute() {
            log.line(
                Priority::Error,
                "error",
                &[
                    ("reason", "usage"),
                    ("detail", "--nft takes an absolute path"),
                    ("path", &shown),
                ],
            );
            return None;
        }
        if path != Path::new(nft::DEFAULT_PATH) {
            log.line(Priority::Warning, "nft-path", &[("path", &shown)]);
        }
    }
    Some(Nft::new(path))
}

/// `capture-floor FILE...`: checks the state dir, reads the floor from the files, then writes `floor` under the lock, waiting up to `wait` for it.
///
/// The inputs are read before the lock is taken, so an unreadable input touches nothing, not even `lock`.
fn capture_floor<E: Write>(
    state_dir: &Path,
    files: &[PathBuf],
    wait: Duration,
    err: &mut E,
) -> Exit {
    let dir = match StateDir::open(state_dir) {
        Ok(dir) => dir,
        Err(e) => return fs_failure(&e, err),
    };
    let floor = match config::capture_floor(files) {
        Ok(floor) => floor,
        Err(e) => return failure(e.token(), &e, Exit::Error, err),
    };
    let _lock = match Lock::acquire(&dir, Some(wait)) {
        Ok(lock) => lock,
        Err(e) => return fs_failure(&e, err),
    };
    match config::write_floor(&dir, &floor) {
        Ok(()) => Exit::Fresh,
        Err(e) => fs_failure(&e, err),
    }
}

fn fs_failure<E: Write>(error: &FsError, err: &mut E) -> Exit {
    let exit = match error {
        FsError::Busy { .. } => Exit::Busy,
        FsError::StateDirInvalid { .. } | FsError::Io { .. } => Exit::Error,
    };
    failure(error.token(), error, exit, err)
}

/// Reports a failed `capture-floor` as one stderr line, `capture-floor: <token>: <detail>`.
fn failure<E: Write>(token: &str, detail: &dyn std::fmt::Display, exit: Exit, err: &mut E) -> Exit {
    // nothing is left to report a failed write to
    let _ = writeln!(err, "capture-floor: {token}: {detail}").and_then(|()| err.flush());
    exit
}

fn not_implemented<E: Write>(command: &str, err: &mut E) -> Exit {
    // nothing is left to report a failed write to
    let _ = writeln!(err, "tn-p2p-allow-sync: {command} is not implemented yet")
        .and_then(|()| err.flush());
    Exit::Error
}

fn write_version<O: Write>(out: &mut O) -> Exit {
    // a closed stdout is an error the caller sees, never a panic
    match out
        .write_all(version_text().as_bytes())
        .and_then(|()| out.flush())
    {
        Ok(()) => Exit::Fresh,
        Err(_) => Exit::Error,
    }
}

fn usage<E: Write>(err: &mut E) -> Exit {
    // nothing is left to report a failed write to
    let _ = err.write_all(USAGE.as_bytes()).and_then(|()| err.flush());
    Exit::Error
}

fn version_text() -> String {
    let snapshot_versions = SNAPSHOT_VERSIONS
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{name} {version}\ncommit {commit}\ntarget {TARGET}\nsnapshot-versions {snapshot_versions}\nconfig-version {CONFIG_VERSION}\n",
        name = env!("CARGO_PKG_NAME"),
        version = env!("CARGO_PKG_VERSION"),
        commit = COMMIT.unwrap_or("unknown"),
    )
}

/// Whether `s` is a full commit hash as `git rev-parse` prints it: 40 lowercase hex digits.
const fn is_commit_hash(s: &str) -> bool {
    let mut rest = s.as_bytes();
    if rest.len() != 40 {
        return false;
    }
    while let [first, tail @ ..] = rest {
        if !matches!(first, b'0'..=b'9' | b'a'..=b'f') {
            return false;
        }
        rest = tail;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::TempDir;

    fn run_args(args: &[&str]) -> (Exit, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let exit = run_with(args.iter().map(OsString::from), &mut out, &mut err, false);
        (
            exit,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    #[test]
    fn version_prints_the_five_lines() {
        let (exit, out, err) = run_args(&["tn-p2p-allow-sync", "--version"]);
        assert_eq!(exit, Exit::Fresh);
        assert_eq!(err, "");
        let name = format!("tn-p2p-allow-sync {}", env!("CARGO_PKG_VERSION"));
        let commit = format!("commit {}", COMMIT.unwrap_or("unknown"));
        let target = format!("target {TARGET}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines,
            [
                name.as_str(),
                commit.as_str(),
                target.as_str(),
                "snapshot-versions 1",
                "config-version 1",
            ]
        );
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn anything_else_is_a_usage_error() {
        for args in [
            &["tn-p2p-allow-sync"][..],
            &["tn-p2p-allow-sync", "--version", "extra"],
            &["tn-p2p-allow-sync", "--help"],
            &["tn-p2p-allow-sync", "-V"],
            &["tn-p2p-allow-sync", "sync", "--bogus"],
        ] {
            let (exit, out, err) = run_args(args);
            assert_eq!(exit, Exit::Error, "{args:?}");
            assert_eq!(exit.code(), 2, "{args:?}");
            assert_eq!(out, "", "{args:?}");
            assert_eq!(err, USAGE, "{args:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_argument_is_a_usage_error() {
        use std::os::unix::ffi::OsStringExt;

        let args = [
            OsString::from("tn-p2p-allow-sync"),
            OsString::from_vec(b"--version\xff".to_vec()),
        ];
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(run_with(args, &mut out, &mut err, false), Exit::Error);
        assert!(out.is_empty());
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn parsed(args: &[&str]) -> Option<Invocation> {
        parse(os(args))
    }

    fn with_defaults(command: Command) -> Invocation {
        Invocation {
            state_dir: PathBuf::from(DEFAULT_STATE_DIR),
            nft: None,
            command,
        }
    }

    #[test]
    fn every_command_parses_with_the_defaults() {
        for (args, command) in [
            (
                &["sync"][..],
                Command::Sync {
                    require_authoritative: false,
                    wait: None,
                },
            ),
            (
                &["sync", "--require-authoritative"],
                Command::Sync {
                    require_authoritative: true,
                    wait: None,
                },
            ),
            (
                &["sync", "--wait", "30", "--require-authoritative"],
                Command::Sync {
                    require_authoritative: true,
                    wait: Some(30),
                },
            ),
            (
                &["sync", "--require-authoritative", "--wait", "0"],
                Command::Sync {
                    require_authoritative: true,
                    wait: Some(0),
                },
            ),
            (
                &["sync", "--wait", "18446744073709551615"],
                Command::Sync {
                    require_authoritative: false,
                    wait: Some(u64::MAX),
                },
            ),
            (&["check"], Command::Check),
            (&["status"], Command::Status { json: false }),
            (&["status", "--json"], Command::Status { json: true }),
            (
                &["render"],
                Command::Render {
                    snapshot: None,
                    check: false,
                },
            ),
            (
                &["render", "--check", "--snapshot", "snap.json"],
                Command::Render {
                    snapshot: Some(PathBuf::from("snap.json")),
                    check: true,
                },
            ),
            (
                &["capture-floor", "/etc/telcoin/bootstrap-peers.yaml"],
                Command::CaptureFloor(vec![PathBuf::from("/etc/telcoin/bootstrap-peers.yaml")]),
            ),
            (
                &["capture-floor", "a.yaml", "./-b.yaml", "a.yaml"],
                Command::CaptureFloor(vec![
                    PathBuf::from("a.yaml"),
                    PathBuf::from("./-b.yaml"),
                    PathBuf::from("a.yaml"),
                ]),
            ),
            (&["--version"], Command::Version),
        ] {
            assert_eq!(parsed(args), Some(with_defaults(command)), "{args:?}");
        }
    }

    #[test]
    fn global_flags_come_before_the_command() {
        let expected = Invocation {
            state_dir: PathBuf::from("/srv/state"),
            nft: Some(PathBuf::from("/opt/nft")),
            command: Command::Status { json: true },
        };
        assert_eq!(
            parsed(&[
                "--state-dir",
                "/srv/state",
                "--nft",
                "/opt/nft",
                "status",
                "--json"
            ]),
            Some(expected.clone())
        );
        assert_eq!(
            parsed(&[
                "--nft",
                "/opt/nft",
                "--state-dir",
                "/srv/state",
                "status",
                "--json"
            ]),
            Some(expected)
        );
        assert_eq!(
            parsed(&["--state-dir", "rel", "--version"]),
            Some(Invocation {
                state_dir: PathBuf::from("rel"),
                nft: None,
                command: Command::Version,
            })
        );
    }

    #[test]
    fn malformed_command_lines_are_usage_errors() {
        for args in [
            &[][..],
            &["bogus"],
            &["SYNC"],
            &["sync", "--bogus"],
            &["sync", "--wait"],
            &["sync", "--wait", ""],
            &["sync", "--wait", "-1"],
            &["sync", "--wait", "+5"],
            &["sync", "--wait", "1.5"],
            &["sync", "--wait", " 5"],
            &["sync", "--wait", "30s"],
            &["sync", "--wait", "18446744073709551616"],
            &["sync", "--wait=30"],
            &["sync", "--wait", "30", "--wait", "30"],
            &["sync", "--require-authoritative", "--require-authoritative"],
            &["sync", "--state-dir", "/srv/state"],
            &["sync", "check"],
            &["--wait", "5", "sync"],
            &["--json", "status"],
            &["check", "--json"],
            &["check", "extra"],
            &["status", "--json", "--json"],
            &["status", "json"],
            &["render", "--snapshot"],
            &["render", "--snapshot", "--check"],
            &["render", "--snapshot", "a", "--snapshot", "b"],
            &["render", "--check", "--check"],
            &["capture-floor"],
            &["capture-floor", "--wait", "30", "peers.yaml"],
            &["capture-floor", "peers.yaml", "-"],
            &["capture-floor", ""],
            &["--version", "extra"],
            &["--version", "--state-dir", "/srv/state"],
            &["--state-dir"],
            &["--state-dir", "/srv/state"],
            &["--state-dir", ""],
            &["--state-dir", "--nft", "/opt/nft", "sync"],
            &["--state-dir", "a", "--state-dir", "b", "sync"],
            &["--nft", "a", "--nft", "b", "sync"],
            &["--nft", "", "sync"],
            &["--state-dir=/srv/state", "sync"],
            &["-h"],
        ] {
            assert_eq!(parsed(args), None, "{args:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn paths_may_be_any_bytes_but_flags_may_not() {
        use std::os::unix::ffi::OsStringExt;

        let odd = OsString::from_vec(b"/srv/st\xffate".to_vec());
        let args = vec![
            OsString::from("--state-dir"),
            odd.clone(),
            OsString::from("capture-floor"),
            odd.clone(),
        ];
        assert_eq!(
            parse(args),
            Some(Invocation {
                state_dir: PathBuf::from(odd.clone()),
                nft: None,
                command: Command::CaptureFloor(vec![PathBuf::from(odd)]),
            })
        );
        let wait = vec![
            OsString::from("sync"),
            OsString::from("--wait"),
            OsString::from_vec(b"3\xff".to_vec()),
        ];
        assert_eq!(parse(wait), None);
    }

    #[test]
    fn commands_not_built_yet_exit_2_without_usage() {
        for (args, command) in [
            (&["tn-p2p-allow-sync", "check"][..], "check"),
            (&["tn-p2p-allow-sync", "status", "--json"], "status"),
        ] {
            let (exit, out, err) = run_args(args);
            assert_eq!(exit, Exit::Error, "{args:?}");
            assert_eq!(out, "", "{args:?}");
            assert_eq!(
                err,
                format!("tn-p2p-allow-sync: {command} is not implemented yet\n")
            );
        }
    }

    #[test]
    fn sync_and_render_refuse_a_missing_state_dir_and_touch_nothing() {
        for command in ["sync", "render"] {
            let args = [
                "tn-p2p-allow-sync",
                "--state-dir",
                "/nonexistent/tn-p2p-allow-sync-state",
                command,
            ];
            let (exit, out, err) = run_args(&args);
            assert_eq!(exit, Exit::Error, "{command}");
            assert_eq!(out, "", "{command}");
            assert!(
                err.starts_with("error reason=state-dir-invalid detail="),
                "{command}: {err}"
            );
            assert_eq!(err.lines().count(), 1, "{command}: {err}");
        }
    }

    #[test]
    fn the_nft_path_must_be_absolute_and_a_non_default_one_is_warned_about() {
        for command in ["sync", "render"] {
            let (exit, out, err) = run_args(&["tn-p2p-allow-sync", "--nft", "nft", command]);
            assert_eq!((exit, out.as_str()), (Exit::Error, ""), "{command}");
            assert_eq!(
                err, "error reason=usage detail=\"--nft takes an absolute path\" path=nft\n",
                "{command}"
            );

            let (exit, _, err) = run_args(&[
                "tn-p2p-allow-sync",
                "--state-dir",
                "/nonexistent/tn-p2p-allow-sync-state",
                "--nft",
                "/opt/nft",
                command,
            ]);
            assert_eq!(exit, Exit::Error, "{command}");
            let lines: Vec<&str> = err.lines().collect();
            assert_eq!(lines.len(), 2, "{command}: {err}");
            assert_eq!(lines.first().copied(), Some("nft-path path=/opt/nft"));

            let (_, _, err) = run_args(&[
                "tn-p2p-allow-sync",
                "--state-dir",
                "/nonexistent/tn-p2p-allow-sync-state",
                "--nft",
                nft::DEFAULT_PATH,
                command,
            ]);
            assert!(!err.contains("nft-path"), "{command}: {err}");
        }
    }

    fn run_capture(state_dir: &Path, inputs: &[PathBuf], wait: Duration) -> (Exit, String) {
        let mut err = Vec::new();
        let exit = capture_floor(state_dir, inputs, wait, &mut err);
        (exit, String::from_utf8_lossy(&err).into_owned())
    }

    const PEERS: &str = "\
k1:
  primary:
    network_address: /ip4/192.0.2.10/udp/49590/quic-v1/p2p/12D3KooWCnizjv4ctsBTwwYJxZP3duCuoebJfbk1fUQv6Vg1dVUh
  workers:
    - network_address: /ip6/2001:db8::10/udp/49594/quic-v1
k2:
  primary:
    network_address: /dns4/validator.example/udp/49590/quic-v1
";

    #[test]
    fn capture_floor_writes_the_floor_end_to_end() -> Result<(), Box<dyn std::error::Error>> {
        let state = TempDir::new("cli-capture")?;
        let inputs = TempDir::new("cli-capture-inputs")?;
        let peers = inputs.join("bootstrap-peers.yaml");
        std::fs::write(&peers, PEERS)?;
        let args = vec![
            OsString::from("tn-p2p-allow-sync"),
            OsString::from("--state-dir"),
            state.path().as_os_str().to_owned(),
            OsString::from("capture-floor"),
            peers.into_os_string(),
        ];
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(run_with(args, &mut out, &mut err, false), Exit::Fresh);
        assert!(out.is_empty() && err.is_empty());
        assert_eq!(
            std::fs::read_to_string(state.join(config::FLOOR_FILE))?,
            "# floor 1\n192.0.2.10\n2001:db8::10\n"
        );
        let mut names = std::fs::read_dir(state.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        names.sort();
        assert_eq!(names, ["floor", "lock"]);
        Ok(())
    }

    #[test]
    fn capture_floor_refusals_touch_nothing() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let state = TempDir::new("cli-refuse")?;
        let inputs = TempDir::new("cli-refuse-inputs")?;
        let peers = inputs.join("bootstrap-peers.yaml");
        std::fs::write(&peers, PEERS)?;
        let floor = state.join(config::FLOOR_FILE);
        std::fs::write(&floor, "# floor 1\n198.51.100.1\n")?;
        let names = || -> std::io::Result<Vec<OsString>> {
            std::fs::read_dir(state.path())?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect()
        };

        // an unreadable input: not even the lock file appears
        let (exit, err) = run_capture(state.path(), &[inputs.join("absent.yaml")], Duration::ZERO);
        assert_eq!(exit, Exit::Error);
        assert!(err.starts_with("capture-floor: floor-invalid: "), "{err}");
        assert!(err.ends_with('\n') && err.lines().count() == 1, "{err}");
        assert_eq!(names()?, ["floor"]);

        // a state dir open to others
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o755))?;
        let (exit, err) = run_capture(state.path(), std::slice::from_ref(&peers), Duration::ZERO);
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700))?;
        assert_eq!(exit, Exit::Error);
        assert!(
            err.starts_with("capture-floor: state-dir-invalid: "),
            "{err}"
        );
        assert_eq!(names()?, ["floor"]);

        // the lock is held by another run
        let dir = StateDir::open(state.path())?;
        let held = Lock::acquire(&dir, None)?;
        let (exit, err) = run_capture(state.path(), std::slice::from_ref(&peers), Duration::ZERO);
        assert_eq!(exit, Exit::Busy);
        assert_eq!(exit.code(), 4);
        assert!(err.starts_with("capture-floor: busy: "), "{err}");
        drop(held);

        assert_eq!(
            std::fs::read_to_string(&floor)?,
            "# floor 1\n198.51.100.1\n"
        );
        let (exit, err) = run_capture(state.path(), &[peers], Duration::from_secs(5));
        assert_eq!((exit, err.as_str()), (Exit::Fresh, ""));
        assert_eq!(
            std::fs::read_to_string(&floor)?,
            "# floor 1\n192.0.2.10\n2001:db8::10\n"
        );
        Ok(())
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        let codes = [
            Exit::Fresh,
            Exit::Refused,
            Exit::Error,
            Exit::Stale,
            Exit::Busy,
        ]
        .map(Exit::code);
        assert_eq!(codes, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn commit_hash_shape() {
        assert!(is_commit_hash("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_commit_hash(""));
        assert!(!is_commit_hash("0123456789abcdef0123456789abcdef0123456"));
        assert!(!is_commit_hash("0123456789abcdef0123456789abcdef012345678"));
        assert!(!is_commit_hash("0123456789ABCDEF0123456789abcdef01234567"));
        assert!(!is_commit_hash("0123456789abcdeg0123456789abcdef01234567"));
        assert!(!is_commit_hash("unknown"));
    }
}
