//! The state dir: its safety checks, reads that never follow a symlink, atomic writes, the run lock and the effective uid; and the bounded read of a file named on the command line.
//!
//! Every file the tool reads or writes at run time lives in the state dir, so no temporary file ever lands in `/tmp`.
//! The tool never creates the dir: the scripts create it as root with mode 0700, and [`StateDir::open`] refuses one that is a symlink, is not a directory, is owned by another uid or has any mode other than 0700, and a path whose last component is empty (a trailing `/`), `.` or `..`, behind which the kernel would follow a symlink.
//!
//! Files are opened with `O_NOFOLLOW` through [`OpenOptionsExt::custom_flags`], a safe API, so a symlink planted in the dir is refused rather than followed; the dir itself is opened for its fsync with `O_NOFOLLOW|O_DIRECTORY`.
//!
//! This module is the one place the crate opens, renames, removes or inspects a file by path; clippy.toml refuses those std::fs calls anywhere else.

// the owner clippy.toml names for std::fs: every open, rename and remove of the crate is here
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Name of the lock file inside the state dir; it is created empty with mode 0600 and never deleted.
pub const LOCK_FILE: &str = "lock";

/// The mode of [`LOCK_FILE`].
pub const LOCK_MODE: u32 = 0o600;

/// The only mode [`StateDir::open`] accepts for the state dir.
pub const STATE_DIR_MODE: u32 = 0o700;

/// How often [`Lock::acquire`] retries a held lock while it may still wait.
pub const LOCK_POLL: Duration = Duration::from_millis(200);

/// Where the kernel reports the process's uids.
const PROC_SELF_STATUS: &str = "/proc/self/status";

// std exposes no libc constants, and the values differ between architectures
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )
))]
const O_NOFOLLOW: i32 = 0o400_000;
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )
))]
const O_DIRECTORY: i32 = 0o200_000;
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc",
        target_arch = "powerpc64"
    )
))]
const O_NOFOLLOW: i32 = 0o100_000;
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc",
        target_arch = "powerpc64"
    )
))]
const O_DIRECTORY: i32 = 0o40_000;
#[cfg(target_vendor = "apple")]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(target_vendor = "apple")]
const O_DIRECTORY: i32 = 0x0010_0000;
// the same on every Linux target above; the compile_error below covers the others
#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4_000;
#[cfg(target_vendor = "apple")]
const O_NONBLOCK: i32 = 0x0004;
#[cfg(not(any(
    all(
        target_os = "linux",
        any(
            target_arch = "x86_64",
            target_arch = "x86",
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "powerpc",
            target_arch = "powerpc64"
        )
    ),
    target_vendor = "apple"
)))]
compile_error!(
    "the values of O_NOFOLLOW, O_DIRECTORY and O_NONBLOCK are not known for this target"
);

/// Why an operation on the state dir failed.
#[derive(Debug)]
pub enum FsError {
    /// The state dir is missing, a symlink, not a directory, owned by another uid or not mode 0700, or the effective uid is unknown.
    ///
    /// Token `state-dir-invalid`; the run exits 2 without touching anything.
    StateDirInvalid {
        /// The state dir as given.
        path: PathBuf,
        /// What the check found.
        reason: String,
    },
    /// Another run holds the lock, and the wait, if any, ran out.
    ///
    /// Token `busy`; the run exits 4 without touching anything.
    Busy {
        /// The lock file.
        path: PathBuf,
    },
    /// Reading, writing or locking a file failed, or the file is a symlink or not a regular file.
    ///
    /// Token `state-io-failed`; the run exits 2.
    Io {
        /// The file the operation was on.
        path: PathBuf,
        /// What was being done, as a verb phrase: `read`, `create`, `rename into place`.
        action: &'static str,
        /// The error the system reported.
        source: io::Error,
    },
}

impl FsError {
    /// The reason token of this error, as `status` and the log line print it.
    pub fn token(&self) -> &'static str {
        match self {
            Self::StateDirInvalid { .. } => "state-dir-invalid",
            Self::Busy { .. } => "busy",
            Self::Io { .. } => "state-io-failed",
        }
    }

    fn io(path: &Path, action: &'static str, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            action,
            source,
        }
    }

    fn state_dir(path: &Path, reason: String) -> Self {
        Self::StateDirInvalid {
            path: path.to_owned(),
            reason,
        }
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // paths print through `Debug`, so a control character cannot break the log line
        match self {
            Self::StateDirInvalid { path, reason } => write!(f, "state dir {path:?}: {reason}"),
            Self::Busy { path } => write!(f, "{path:?} is held by another run"),
            Self::Io {
                path,
                action,
                source,
            } => write!(f, "{path:?}: cannot {action}: {source}"),
        }
    }
}

impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::StateDirInvalid { .. } | Self::Busy { .. } => None,
        }
    }
}

/// The effective uid of this process: the second field of the `Uid:` line of `/proc/self/status`.
///
/// The environment is never consulted, and std offers no safe `geteuid`.
pub fn effective_uid() -> Result<u32, FsError> {
    let path = Path::new(PROC_SELF_STATUS);
    let status = fs::read_to_string(path).map_err(|e| FsError::io(path, "read", e))?;
    parse_effective_uid(&status).ok_or_else(|| {
        FsError::io(
            path,
            "read the effective uid",
            io::Error::new(io::ErrorKind::InvalidData, "no well-formed Uid: line"),
        )
    })
}

/// The effective uid in the text of `/proc/self/status`: the line `Uid:\t<real>\t<effective>\t<saved>\t<fs>`.
fn parse_effective_uid(status: &str) -> Option<u32> {
    let line = status.lines().find_map(|line| line.strip_prefix("Uid:"))?;
    let mut fields = line.split_ascii_whitespace();
    let (Some(_real), Some(effective), Some(_saved), Some(_fs), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return None;
    };
    effective.parse().ok()
}

/// Reads the regular file at `path` without following a symlink, or `None` when nothing is there.
///
/// A symlink, a directory or any other non-regular file is an error, checked before the open and again on the open file.
/// The open is `O_NONBLOCK`, so a FIFO swapped in between the check and the open cannot block the run either.
/// The read has no size cap: every caller reads a file in the euid-owned 0700 state dir.
pub fn read_nofollow(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => regular_file(&meta)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    read_opened(path)
}

/// The open and read of [`read_nofollow`], after its check: `O_RDONLY|O_NOFOLLOW|O_NONBLOCK`, then the open file must be a regular file.
fn read_opened(path: &Path) -> io::Result<Option<Vec<u8>>> {
    // O_NONBLOCK changes nothing for a regular file; for a FIFO it makes the open return at
    // once instead of waiting for a writer, and the check below then refuses it
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // the entry may have been swapped between the check and the open
    regular_file(&file.metadata()?)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// Refuses anything but a regular file, naming what it is instead.
fn regular_file(meta: &fs::Metadata) -> io::Result<()> {
    let kind = meta.file_type();
    if kind.is_file() {
        return Ok(());
    }
    let what = if kind.is_symlink() {
        "is a symlink"
    } else if kind.is_dir() {
        "is a directory"
    } else {
        "is not a regular file"
    };
    Err(io::Error::new(io::ErrorKind::InvalidInput, what))
}

/// Reads a file an operator named on the command line: symlinks followed, a regular file only, at most `max + 1` bytes.
///
/// The type is checked before the open, so a device, a FIFO or a socket is refused without being opened.
/// It is checked again on the open file, opened `O_NONBLOCK`, so a FIFO swapped in between the two is refused at once instead of blocking the run; a regular file ignores `O_NONBLOCK`.
/// The one byte past `max` lets the caller tell a file of exactly `max` bytes from a larger one; what a larger one means is the caller's to say.
pub fn read_bounded(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let regular = |meta: fs::Metadata| {
        if meta.is_file() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "is not a regular file",
            ))
        }
    };
    // the tool runs as root, and opening a device node can act on the device: a watchdog arms on open
    regular(fs::metadata(path)?)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)?;
    // the entry may have been swapped between the check and the open
    regular(file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Removes what a run that died between create and rename left at the temporary name `path`, if anything.
///
/// Unlinking a symlink removes the link, never its target.
/// An empty directory in the way is removed too; one with anything in it is an error, never emptied.
/// The caller holds the [`Lock`]: that is what makes the removal safe.
fn remove_leftover(path: &Path) -> Result<(), FsError> {
    let error = match fs::remove_file(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => e,
    };
    if error.kind() != io::ErrorKind::IsADirectory {
        return Err(FsError::io(path, "remove the leftover", error));
    }
    fs::remove_dir(path).map_err(|e| {
        let detail = format!("a directory is in the way: {e}");
        FsError::io(
            path,
            "remove the leftover",
            io::Error::new(e.kind(), detail),
        )
    })
}

/// Why [`StateDir::open`] refused a path that does not [end in a name](ends_in_a_name).
const NOT_A_NAME: &str = "does not end in a name; a trailing /, . or .. would follow a symlink";

/// Whether the last component of `path`, byte for byte, is a name: not empty, as after a trailing `/`, and not `.` or `..`.
///
/// Only then do `lstat` and `O_NOFOLLOW` see the entry itself: the kernel follows a symlink before a trailing `/` or `/.`, and `..` is a parent.
/// [`Path::components`] cannot tell, since it drops a trailing `/` and `.`; a path that ends in a name also has a [`std::path::Component::Normal`] last.
pub fn ends_in_a_name(path: &Path) -> bool {
    path.as_os_str()
        .as_bytes()
        .rsplit(|&b| b == b'/')
        .next()
        .is_some_and(|last| !matches!(last, b"" | b"." | b".."))
}

/// Gives the new file `file` exactly `mode`, writes `bytes` and syncs it; closing it is left to the drop.
fn fill(mut file: File, path: &Path, bytes: &[u8], mode: u32) -> Result<(), FsError> {
    // the mode given at creation is masked by the umask
    file.set_permissions(Permissions::from_mode(mode))
        .map_err(|e| FsError::io(path, "set the mode of", e))?;
    file.write_all(bytes)
        .map_err(|e| FsError::io(path, "write", e))?;
    file.sync_all().map_err(|e| FsError::io(path, "sync", e))
}

/// A state dir that passed the safety checks of [`StateDir::open`].
///
/// File names passed to its methods are single path components such as `status`; a name with a `/`, an empty name, `.` and `..` are refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    path: PathBuf,
}

impl StateDir {
    /// Checks the state dir at `path` and returns it.
    ///
    /// Refused with [`FsError::StateDirInvalid`]: a path that does not [end in a name](ends_in_a_name), nothing at `path`, a symlink (checked with `symlink_metadata`, never followed), anything but a directory, an owner other than the effective uid, a mode other than exactly 0700, or an effective uid that cannot be read.
    pub fn open(path: &Path) -> Result<Self, FsError> {
        if !ends_in_a_name(path) {
            return Err(FsError::state_dir(path, NOT_A_NAME.to_owned()));
        }
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(FsError::state_dir(path, "does not exist".to_owned()));
            }
            Err(e) => return Err(FsError::state_dir(path, format!("cannot inspect: {e}"))),
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            return Err(FsError::state_dir(path, "is a symlink".to_owned()));
        }
        if !kind.is_dir() {
            return Err(FsError::state_dir(path, "is not a directory".to_owned()));
        }
        let euid = effective_uid()
            .map_err(|e| FsError::state_dir(path, format!("cannot check the owner: {e}")))?;
        if meta.uid() != euid {
            return Err(FsError::state_dir(
                path,
                format!(
                    "is owned by uid {}, not the effective uid {euid}",
                    meta.uid()
                ),
            ));
        }
        let mode = meta.mode() & 0o7777;
        if mode != STATE_DIR_MODE {
            return Err(FsError::state_dir(
                path,
                format!("has mode {mode:04o}, not {STATE_DIR_MODE:04o}"),
            ));
        }
        Ok(Self {
            path: path.to_owned(),
        })
    }

    /// The state dir's path, as given to [`StateDir::open`].
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The path of the file `name` in the state dir.
    pub fn file(&self, name: &str) -> Result<PathBuf, FsError> {
        let single = !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0']);
        if single {
            Ok(self.path.join(name))
        } else {
            Err(FsError::io(
                &self.path.join(name),
                "use the name",
                io::Error::new(io::ErrorKind::InvalidInput, "not a single file name"),
            ))
        }
    }

    /// Reads the file `name`, or `None` when it does not exist.
    ///
    /// A symlink or a non-regular file is an error, never followed.
    pub fn read(&self, name: &str) -> Result<Option<Vec<u8>>, FsError> {
        let path = self.file(name)?;
        read_nofollow(&path).map_err(|e| FsError::io(&path, "read", e))
    }

    /// Replaces the file `name` with `bytes` and `mode`, atomically.
    ///
    /// The bytes go to `.<name>.new`, created with `O_CREAT|O_EXCL|O_NOFOLLOW`, set to exactly `mode` whatever the umask, and synced; the rename into place follows, then a sync of the directory.
    /// A reader sees the old content or the new, never a mix, and a crash leaves at worst the temporary file, which the next write removes.
    /// The caller holds the [`Lock`]: that is what makes removing a leftover temporary file safe.
    /// On failure the temporary file is removed and the old content stays.
    pub fn write_atomic(&self, name: &str, bytes: &[u8], mode: u32) -> Result<(), FsError> {
        let target = self.file(name)?;
        let temp = self.create(&format!(".{name}.new"), bytes, mode)?;
        if let Err(e) = fs::rename(&temp, &target) {
            // best effort: the error that matters is the rename's
            let _ = fs::remove_file(&temp);
            return Err(FsError::io(&target, "rename into place", e));
        }
        self.sync()
    }

    /// Creates the file `name` holding `bytes`, with exactly `mode` whatever the umask, syncs it and returns its path; a leftover under the name is removed first.
    ///
    /// The file is created with `O_CREAT|O_EXCL|O_NOFOLLOW`, so nothing is ever written through a symlink planted under the name.
    /// On failure the new file is removed.
    /// The caller holds the [`Lock`]: that is what makes removing a leftover safe.
    pub fn create(&self, name: &str, bytes: &[u8], mode: u32) -> Result<PathBuf, FsError> {
        let path = self.file(name)?;
        remove_leftover(&path)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
            .map_err(|e| FsError::io(&path, "create", e))?;
        if let Err(e) = fill(file, &path, bytes, mode) {
            // best effort: the error that matters is the one above
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        Ok(path)
    }

    /// Renames the file `from` to `to`, then syncs the directory so the rename survives a crash.
    ///
    /// On failure `from` stays where it is.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        let source = self.file(from)?;
        let target = self.file(to)?;
        fs::rename(&source, &target).map_err(|e| FsError::io(&target, "rename into place", e))?;
        self.sync()
    }

    /// Removes the file `name`; nothing under the name is not an error, and a symlink is unlinked, never followed.
    pub fn remove(&self, name: &str) -> Result<(), FsError> {
        let path = self.file(name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(FsError::io(&path, "remove", e)),
        }
    }

    /// Syncs the directory, so a rename into it survives a crash.
    ///
    /// The directory is opened with `O_NOFOLLOW|O_DIRECTORY`: a symlink or a file swapped in for it since [`StateDir::open`] is an error, never followed or synced.
    pub fn sync(&self) -> Result<(), FsError> {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_DIRECTORY)
            .open(&self.path)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| FsError::io(&self.path, "sync the directory", e))
    }

    /// The modification time of the file `name`, or `None` when it does not exist.
    ///
    /// A symlink or a non-regular file is an error, never followed.
    pub fn mtime(&self, name: &str) -> Result<Option<SystemTime>, FsError> {
        let path = self.file(name)?;
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(FsError::io(&path, "inspect", e)),
        };
        regular_file(&meta)
            .and_then(|()| meta.modified())
            .map(Some)
            .map_err(|e| FsError::io(&path, "read the mtime of", e))
    }
}

/// The run lock: an exclusive `flock` on `lock` in the state dir, released when dropped.
///
/// The lock belongs to the open file description, so a second open of `lock`, in this process or another, conflicts with it.
#[derive(Debug)]
pub struct Lock {
    // held only for its lock; closing the descriptor releases it
    _file: File,
}

impl Lock {
    /// Takes the lock of `dir`, creating `lock` with mode 0600 when it is missing; a `lock` of another mode, as the umask leaves a new one, is set to 0600.
    ///
    /// With `wait` `None` a held lock is [`FsError::Busy`] at once.
    /// With a duration the attempt repeats every [`LOCK_POLL`] until the lock is free or the duration has passed, then gives up as busy.
    pub fn acquire(dir: &StateDir, wait: Option<Duration>) -> Result<Self, FsError> {
        let path = dir.file(LOCK_FILE)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(LOCK_MODE)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
            .map_err(|e| FsError::io(&path, "open", e))?;
        let meta = file
            .metadata()
            .and_then(|meta| regular_file(&meta).map(|()| meta))
            .map_err(|e| FsError::io(&path, "open", e))?;
        // the mode given at creation is masked by the umask
        if meta.mode() & 0o7777 != LOCK_MODE {
            file.set_permissions(Permissions::from_mode(LOCK_MODE))
                .map_err(|e| FsError::io(&path, "set the mode of", e))?;
        }
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(e)) => return Err(FsError::io(&path, "lock", e)),
            }
            let left = wait.map_or(Duration::ZERO, |wait| wait.saturating_sub(start.elapsed()));
            if left.is_zero() {
                return Err(FsError::Busy { path });
            }
            std::thread::sleep(left.min(LOCK_POLL));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::TempDir;
    use std::error::Error;
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;

    type TestResult = Result<(), Box<dyn Error>>;

    fn mode_of(path: &Path) -> Result<u32, Box<dyn Error>> {
        Ok(fs::symlink_metadata(path)?.mode() & 0o7777)
    }

    fn state_dir(tag: &str) -> Result<(TempDir, StateDir), Box<dyn Error>> {
        let temp = TempDir::new(tag)?;
        let dir = StateDir::open(temp.path())?;
        Ok((temp, dir))
    }

    fn reason(result: Result<StateDir, FsError>) -> Result<String, Box<dyn Error>> {
        match result {
            Err(e @ FsError::StateDirInvalid { .. }) => {
                assert_eq!(e.token(), "state-dir-invalid");
                Ok(e.to_string())
            }
            other => Err(format!("expected state-dir-invalid, got {other:?}").into()),
        }
    }

    #[test]
    fn effective_uid_is_the_second_uid_field() {
        let status = "Name:\ttn-p2p-allow-sync\nUmask:\t0077\nUid:\t1000\t1001\t1002\t1003\nGid:\t5\t6\t7\t8\n";
        assert_eq!(parse_effective_uid(status), Some(1001));
        assert_eq!(parse_effective_uid("Uid:\t0\t0\t0\t0\n"), Some(0));
        for status in [
            "",
            "Gid:\t1\t2\t3\t4\n",
            "Uid:\t1000\n",
            "Uid:\t1000\t1001\t1002\n",
            "Uid:\t1000\t1001\t1002\t1003\t1004\n",
            "Uid:\t1000\tx\t1002\t1003\n",
            "Uid:\t1000\t-1\t1002\t1003\n",
            "Uid:\t1000\t4294967296\t1002\t1003\n",
            "XUid:\t1000\t1001\t1002\t1003\n",
        ] {
            assert_eq!(parse_effective_uid(status), None, "{status:?}");
        }
    }

    #[test]
    fn effective_uid_owns_what_this_process_creates() -> TestResult {
        let temp = TempDir::new("euid")?;
        assert_eq!(effective_uid()?, fs::metadata(temp.path())?.uid());
        Ok(())
    }

    #[test]
    fn a_private_dir_of_this_uid_opens() -> TestResult {
        let temp = TempDir::new("open")?;
        let dir = StateDir::open(temp.path())?;
        assert_eq!(dir.path(), temp.path());
        assert_eq!(dir.file("status")?, temp.join("status"));
        Ok(())
    }

    #[test]
    fn a_missing_dir_is_refused() -> TestResult {
        let temp = TempDir::new("missing")?;
        let message = reason(StateDir::open(&temp.join("absent")))?;
        assert!(message.ends_with("absent\": does not exist"), "{message}");
        Ok(())
    }

    #[test]
    fn a_dir_that_cannot_be_inspected_is_refused_with_the_error() -> TestResult {
        let temp = TempDir::new("uninspectable")?;
        fs::write(temp.join("file"), b"")?;
        // a file where a parent dir belongs: lstat fails with ENOTDIR, which is not a missing dir
        let message = reason(StateDir::open(&temp.join("file").join("state")))?;
        assert!(message.contains("state\": cannot inspect: "), "{message}");
        Ok(())
    }

    #[test]
    fn a_symlinked_dir_is_refused() -> TestResult {
        let temp = TempDir::new("symlink")?;
        let real = temp.join("real");
        fs::create_dir(&real)?;
        fs::set_permissions(&real, Permissions::from_mode(0o700))?;
        StateDir::open(&real)?;
        let link = temp.join("link");
        symlink(&real, &link)?;
        let message = reason(StateDir::open(&link))?;
        assert!(message.ends_with(": is a symlink"), "{message}");
        Ok(())
    }

    #[test]
    fn a_symlinked_dir_behind_a_trailing_slash_or_dot_is_refused() -> TestResult {
        let temp = TempDir::new("symlink-slash")?;
        let real = temp.join("real");
        fs::create_dir(&real)?;
        fs::set_permissions(&real, Permissions::from_mode(0o700))?;
        symlink(&real, temp.join("link"))?;
        // the kernel follows `link` before a trailing `/` or `/.`, so lstat would see `real`
        for entry in ["link", "real"] {
            for suffix in ["/", "//", "/.", "/./", "/..", "/../"] {
                let path = PathBuf::from(format!("{}{suffix}", temp.join(entry).display()));
                let message = reason(StateDir::open(&path))?;
                assert!(message.ends_with(&format!(": {NOT_A_NAME}")), "{message}");
            }
        }
        for path in ["", ".", "..", "/", "//", "a/.", "./.."] {
            let message = reason(StateDir::open(Path::new(path)))?;
            assert!(message.ends_with(&format!(": {NOT_A_NAME}")), "{message}");
        }
        assert!(ends_in_a_name(Path::new("a")));
        assert!(ends_in_a_name(Path::new("/var/lib/telcoin-p2p-allow")));
        assert!(ends_in_a_name(Path::new("./.state")));
        assert!(ends_in_a_name(Path::new("a/...")));
        StateDir::open(&real)?;
        Ok(())
    }

    #[test]
    fn a_file_is_refused() -> TestResult {
        let temp = TempDir::new("file")?;
        let file = temp.join("file");
        fs::write(&file, b"")?;
        fs::set_permissions(&file, Permissions::from_mode(0o700))?;
        let message = reason(StateDir::open(&file))?;
        assert!(message.ends_with(": is not a directory"), "{message}");
        Ok(())
    }

    #[test]
    fn any_mode_but_0700_is_refused() -> TestResult {
        let temp = TempDir::new("mode")?;
        for mode in [
            0o755, 0o750, 0o711, 0o701, 0o770, 0o500, 0o600, 0o1700, 0o2700,
        ] {
            fs::set_permissions(temp.path(), Permissions::from_mode(mode))?;
            let message = reason(StateDir::open(temp.path()));
            fs::set_permissions(temp.path(), Permissions::from_mode(0o700))?;
            let message = message?;
            assert!(
                message.ends_with(&format!(": has mode {mode:04o}, not 0700")),
                "{message}"
            );
        }
        StateDir::open(temp.path())?;
        Ok(())
    }

    #[test]
    fn a_dir_of_another_uid_is_refused() -> TestResult {
        // `/proc` belongs to root and is not 0700, so it fails whichever check comes first
        let message = reason(StateDir::open(Path::new("/proc")))?;
        if effective_uid()? != 0 {
            assert!(
                message.contains(": is owned by uid 0, not the effective uid "),
                "{message}"
            );
        }
        Ok(())
    }

    #[test]
    fn file_names_are_single_components() -> TestResult {
        let (_temp, dir) = state_dir("names")?;
        for name in ["", ".", "..", "a/b", "/abs", "../status", "nul\0"] {
            let result = dir.file(name);
            assert!(
                matches!(result, Err(FsError::Io { .. })),
                "{name:?}: {result:?}"
            );
            assert!(dir.read(name).is_err(), "{name:?}");
            assert!(dir.write_atomic(name, b"x", 0o600).is_err(), "{name:?}");
            assert!(dir.mtime(name).is_err(), "{name:?}");
        }
        Ok(())
    }

    #[test]
    fn write_atomic_writes_the_mode_and_leaves_no_temporary_file() -> TestResult {
        let (temp, dir) = state_dir("write")?;
        dir.write_atomic("status", b"RESULT=applied\n", 0o600)?;
        assert_eq!(fs::read(temp.join("status"))?, b"RESULT=applied\n");
        assert_eq!(mode_of(&temp.join("status"))?, 0o600);
        dir.write_atomic("status", b"RESULT=stale\n", 0o640)?;
        assert_eq!(dir.read("status")?, Some(b"RESULT=stale\n".to_vec()));
        assert_eq!(mode_of(&temp.join("status"))?, 0o640);
        let names = fs::read_dir(temp.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(names, ["status"]);
        Ok(())
    }

    #[test]
    fn write_atomic_replaces_a_leftover_temporary_file() -> TestResult {
        let (temp, dir) = state_dir("leftover")?;
        fs::write(temp.join(".state.json.new"), b"half a write")?;
        dir.write_atomic("state.json", b"{}\n", 0o600)?;
        assert_eq!(fs::read(temp.join("state.json"))?, b"{}\n");
        assert!(!temp.join(".state.json.new").exists());
        Ok(())
    }

    #[test]
    fn write_atomic_removes_an_empty_directory_at_the_temporary_name() -> TestResult {
        let (temp, dir) = state_dir("leftover-dir")?;
        fs::create_dir(temp.join(".status.new"))?;
        dir.write_atomic("status", b"RESULT=applied\n", 0o600)?;
        assert_eq!(fs::read(temp.join("status"))?, b"RESULT=applied\n");
        assert!(fs::symlink_metadata(temp.join(".status.new")).is_err());
        Ok(())
    }

    #[test]
    fn a_full_directory_at_the_temporary_name_is_kept_and_named() -> TestResult {
        let (temp, dir) = state_dir("leftover-full")?;
        let leftover = temp.join(".status.new");
        fs::create_dir(&leftover)?;
        fs::write(leftover.join("kept"), b"kept")?;
        let error = match dir.write_atomic("status", b"RESULT=applied\n", 0o600) {
            Err(e @ FsError::Io { .. }) => e,
            other => return Err(format!("expected state-io-failed, got {other:?}").into()),
        };
        assert_eq!(error.token(), "state-io-failed");
        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "{leftover:?}: cannot remove the leftover: a directory is in the way: "
            )),
            "{message}"
        );
        assert_eq!(fs::read(leftover.join("kept"))?, b"kept");
        assert!(!temp.join("status").exists());
        Ok(())
    }

    #[test]
    fn write_atomic_never_writes_through_a_symlink() -> TestResult {
        let (temp, dir) = state_dir("nofollow")?;
        let outside = TempDir::new("outside")?;
        let victim = outside.join("victim");
        fs::write(&victim, b"untouched")?;
        symlink(&victim, temp.join(".floor.new"))?;
        symlink(&victim, temp.join("floor"))?;
        dir.write_atomic("floor", b"# floor 1\n", 0o600)?;
        assert_eq!(fs::read(&victim)?, b"untouched");
        assert!(fs::symlink_metadata(temp.join("floor"))?.is_file());
        assert_eq!(fs::read(temp.join("floor"))?, b"# floor 1\n");
        assert!(fs::symlink_metadata(temp.join(".floor.new")).is_err());
        Ok(())
    }

    #[test]
    fn a_failed_write_keeps_the_old_content() -> TestResult {
        let (temp, dir) = state_dir("failed")?;
        // a directory in the way makes the rename fail after the temporary file was written
        fs::create_dir(temp.join("current.nft"))?;
        let result = dir.write_atomic("current.nft", b"table", 0o600);
        assert!(
            matches!(
                result,
                Err(FsError::Io {
                    action: "rename into place",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(result.map_err(|e| e.token()), Err("state-io-failed"));
        assert!(temp.join("current.nft").is_dir());
        assert!(!temp.join(".current.nft.new").exists());
        Ok(())
    }

    #[test]
    fn read_returns_none_for_a_missing_file() -> TestResult {
        let (_temp, dir) = state_dir("read-missing")?;
        assert_eq!(dir.read("state.json")?, None);
        assert_eq!(dir.mtime("state.json")?, None);
        Ok(())
    }

    #[test]
    fn read_and_mtime_refuse_symlinks_and_directories() -> TestResult {
        let (temp, dir) = state_dir("read-refuse")?;
        let outside = TempDir::new("read-outside")?;
        fs::write(outside.join("secret"), b"secret")?;
        symlink(outside.join("secret"), temp.join("config"))?;
        symlink(outside.join("absent"), temp.join("dangling"))?;
        fs::create_dir(temp.join("subdir"))?;
        for name in ["config", "dangling", "subdir"] {
            let read = dir.read(name);
            assert!(matches!(read, Err(FsError::Io { .. })), "{name}: {read:?}");
            let mtime = dir.mtime(name);
            assert!(
                matches!(mtime, Err(FsError::Io { .. })),
                "{name}: {mtime:?}"
            );
        }
        let message = dir
            .read("config")
            .map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.ends_with("config\": cannot read: is a symlink"),
            "{message}"
        );
        Ok(())
    }

    #[test]
    fn read_and_mtime_report_any_error_but_a_missing_file() -> TestResult {
        let temp = TempDir::new("read-error")?;
        let path = temp.join("state");
        fs::create_dir(&path)?;
        fs::set_permissions(&path, Permissions::from_mode(STATE_DIR_MODE))?;
        let dir = StateDir::open(&path)?;
        // the dir swapped for a file after the checks of open: every lstat in it fails with ENOTDIR
        fs::remove_dir(&path)?;
        fs::write(&path, b"")?;
        assert_eq!(
            dir.read("status").map_err(|e| e.token()),
            Err("state-io-failed")
        );
        assert_eq!(
            dir.mtime("status").map_err(|e| e.token()),
            Err("state-io-failed")
        );
        Ok(())
    }

    #[test]
    fn only_an_io_error_has_a_source() -> TestResult {
        let (temp, dir) = state_dir("source")?;
        symlink(temp.join("absent"), temp.join("config"))?;
        let Err(error) = dir.read("config") else {
            return Err("a symlink was read".into());
        };
        assert_eq!(
            error.source().map(ToString::to_string),
            Some("is a symlink".to_owned())
        );
        let Err(error) = StateDir::open(&temp.join("absent")) else {
            return Err("a missing dir opened".into());
        };
        assert!(error.source().is_none(), "{error:?}");
        let error = FsError::Busy {
            path: temp.join(LOCK_FILE),
        };
        assert!(error.source().is_none(), "{error:?}");
        Ok(())
    }

    /// Makes a FIFO at `path` with mkfifo(1); `false` when the host has no mkfifo.
    fn mkfifo(path: &Path) -> Result<bool, Box<dyn Error>> {
        // std cannot make a FIFO, so this test starts mkfifo; the tool itself never does
        #[allow(clippy::disallowed_methods)]
        let status = match std::process::Command::new("mkfifo").arg(path).status() {
            Ok(status) => status,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if status.success() {
            Ok(true)
        } else {
            Err(format!("mkfifo {path:?}: {status}").into())
        }
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() -> TestResult {
        let (temp, dir) = state_dir("fifo")?;
        let fifo = temp.join("status");
        if !mkfifo(&fifo)? {
            println!("a_fifo_is_refused_without_blocking skipped: no mkfifo");
            return Ok(());
        }
        // refused by the check before the open
        let message = dir
            .read("status")
            .map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.ends_with("status\": cannot read: is not a regular file"),
            "{message}"
        );
        // a FIFO swapped in after that check: the open returns at once, and the open file is refused
        let (done, finished) = mpsc::channel();
        let opener = fifo.clone();
        let reader = std::thread::spawn(move || {
            // the receiver outlives this thread; a send error cannot happen
            let _ = done.send(read_opened(&opener).map(drop).map_err(|e| e.to_string()));
        });
        let result = finished.recv_timeout(Duration::from_secs(5));
        if result.is_err() {
            // a blocked open waits for a writer; a non-blocking one releases it, so the thread ends
            let _ = OpenOptions::new()
                .write(true)
                .custom_flags(O_NONBLOCK)
                .open(&fifo);
        }
        reader.join().map_err(|_| "the reader thread panicked")?;
        assert_eq!(result?, Err("is not a regular file".to_owned()));
        Ok(())
    }

    #[test]
    fn the_open_after_the_check_reads_a_vanished_file_as_none_and_refuses_a_symlink() -> TestResult
    {
        let temp = TempDir::new("read-opened")?;
        // the file removed between the check and the open
        assert_eq!(read_opened(&temp.join("gone"))?, None);
        // a symlink swapped in between them: the open refuses it rather than reading its target
        fs::write(temp.join("secret"), b"secret")?;
        symlink(temp.join("secret"), temp.join("link"))?;
        let read = read_opened(&temp.join("link"));
        assert!(read.is_err(), "{read:?}");
        Ok(())
    }

    #[test]
    fn read_bounded_refuses_a_special_file_before_opening_it() -> TestResult {
        let temp = TempDir::new("bounded-special")?;
        let socket = temp.join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket)?;
        // a socket cannot be opened at all, so its detail shows the check came before the open
        for path in [Path::new("/dev/null"), temp.path(), &socket] {
            let read = read_bounded(path, 16).map_err(|e| e.to_string());
            assert_eq!(read, Err("is not a regular file".to_owned()), "{path:?}");
        }
        Ok(())
    }

    #[test]
    fn read_bounded_follows_a_symlink_and_reads_one_byte_past_max() -> TestResult {
        let temp = TempDir::new("bounded-file")?;
        let file = temp.join("input");
        fs::write(&file, b"0123456789")?;
        let link = temp.join("link");
        symlink(&file, &link)?;
        assert_eq!(read_bounded(&file, 16)?, b"0123456789");
        assert_eq!(read_bounded(&link, 4)?, b"01234");
        Ok(())
    }

    #[test]
    fn mtime_of_a_written_file() -> TestResult {
        let (temp, dir) = state_dir("mtime")?;
        dir.write_atomic("current.nft", b"table\n", 0o600)?;
        assert_eq!(
            dir.mtime("current.nft")?,
            Some(fs::metadata(temp.join("current.nft"))?.modified()?)
        );
        Ok(())
    }

    #[test]
    fn the_lock_file_is_created_0600_and_kept() -> TestResult {
        let (temp, dir) = state_dir("lock-create")?;
        let lock = Lock::acquire(&dir, None)?;
        assert_eq!(mode_of(&temp.join(LOCK_FILE))?, LOCK_MODE);
        drop(lock);
        assert!(temp.join(LOCK_FILE).is_file());
        assert_eq!(fs::read(temp.join(LOCK_FILE))?, b"");
        Lock::acquire(&dir, Some(Duration::from_secs(5)))?;
        Ok(())
    }

    #[test]
    fn a_lock_file_of_another_mode_is_set_to_0600() -> TestResult {
        let (temp, dir) = state_dir("lock-mode")?;
        let path = temp.join(LOCK_FILE);
        fs::write(&path, b"")?;
        // what a umask of 0 or 002 leaves on a new file, a setuid bit and an owner-only execute bit
        for mode in [0o666, 0o664, 0o644, 0o4600, 0o700] {
            fs::set_permissions(&path, Permissions::from_mode(mode))?;
            let lock = Lock::acquire(&dir, Some(Duration::from_secs(5)))?;
            assert_eq!(mode_of(&path)?, LOCK_MODE, "{mode:04o}");
            drop(lock);
        }
        Ok(())
    }

    #[test]
    fn the_dir_sync_never_follows_a_symlink_or_opens_a_file() -> TestResult {
        let temp = TempDir::new("dir-sync")?;
        let path = temp.join("state");
        let moved = temp.join("moved");
        fs::create_dir(&path)?;
        fs::set_permissions(&path, Permissions::from_mode(STATE_DIR_MODE))?;
        let dir = StateDir::open(&path)?;
        dir.sync()?;
        // the dir swapped for a symlink to it, then for a file, after the checks of open
        fs::rename(&path, &moved)?;
        symlink(&moved, &path)?;
        let result = dir.sync();
        assert!(
            matches!(
                result,
                Err(FsError::Io {
                    action: "sync the directory",
                    ..
                })
            ),
            "{result:?}"
        );
        fs::remove_file(&path)?;
        fs::write(&path, b"")?;
        let result = dir.sync();
        assert!(
            matches!(
                result,
                Err(FsError::Io {
                    action: "sync the directory",
                    ..
                })
            ),
            "{result:?}"
        );
        Ok(())
    }

    #[test]
    fn a_held_lock_is_busy() -> TestResult {
        let (_temp, dir) = state_dir("lock-busy")?;
        let held = Lock::acquire(&dir, None)?;
        let busy = Lock::acquire(&dir, None);
        assert!(matches!(busy, Err(FsError::Busy { .. })), "{busy:?}");
        assert_eq!(busy.map_err(|e| e.token()).map(|_| ()), Err("busy"));

        let start = Instant::now();
        let busy = Lock::acquire(&dir, Some(Duration::from_millis(450)));
        assert!(matches!(busy, Err(FsError::Busy { .. })), "{busy:?}");
        assert!(start.elapsed() >= Duration::from_millis(450));

        drop(held);
        Lock::acquire(&dir, Some(Duration::from_secs(5)))?;
        Ok(())
    }

    #[test]
    fn a_waiting_lock_gets_it_after_the_release() -> TestResult {
        let (_temp, dir) = state_dir("lock-wait")?;
        let (locked, is_locked) = mpsc::channel();
        let holder_dir = dir.clone();
        let holder = std::thread::spawn(move || -> Result<(), FsError> {
            let lock = Lock::acquire(&holder_dir, None)?;
            // the receiver outlives this thread; a send error cannot happen
            let _ = locked.send(());
            std::thread::sleep(Duration::from_millis(500));
            drop(lock);
            Ok(())
        });
        is_locked.recv()?;
        assert!(matches!(
            Lock::acquire(&dir, None),
            Err(FsError::Busy { .. })
        ));
        let start = Instant::now();
        Lock::acquire(&dir, Some(Duration::from_secs(10)))?;
        assert!(start.elapsed() >= Duration::from_millis(200));
        holder.join().map_err(|_| "the holder thread panicked")??;
        Ok(())
    }

    #[test]
    fn a_symlinked_lock_file_is_refused() -> TestResult {
        let (temp, dir) = state_dir("lock-symlink")?;
        let outside = TempDir::new("lock-outside")?;
        symlink(outside.join("lock"), temp.join(LOCK_FILE))?;
        let result = Lock::acquire(&dir, None);
        assert!(matches!(result, Err(FsError::Io { .. })), "{result:?}");
        assert!(!outside.join("lock").exists());
        Ok(())
    }
}
