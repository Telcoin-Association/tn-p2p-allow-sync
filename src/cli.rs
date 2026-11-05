//! Command line: argument parsing, the usage text, the `--version` lines and the exit codes.

use std::ffi::OsString;
use std::io::{self, Write};

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

/// Runs the command line `args`, program name first as [`std::env::args_os`] yields it, and returns how the run ended.
///
/// Output goes to stdout and diagnostics to stderr.
pub fn run<I>(args: I) -> Exit
where
    I: IntoIterator<Item = OsString>,
{
    run_with(args, &mut io::stdout(), &mut io::stderr())
}

fn run_with<I, O, E>(args: I, out: &mut O, err: &mut E) -> Exit
where
    I: IntoIterator<Item = OsString>,
    O: Write,
    E: Write,
{
    let mut args = args.into_iter().skip(1);
    match (args.next(), args.next()) {
        (Some(arg), None) if arg == "--version" => write_version(out),
        _ => usage(err),
    }
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

    fn run_args(args: &[&str]) -> (Exit, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let exit = run_with(args.iter().map(OsString::from), &mut out, &mut err);
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
            &["tn-p2p-allow-sync", "sync"],
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
        assert_eq!(run_with(args, &mut out, &mut err), Exit::Error);
        assert!(out.is_empty());
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
