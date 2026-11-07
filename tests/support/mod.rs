//! Helpers shared by the tests, std only: the crate has no dev-dependencies.
//!
//! The library's unit tests include this file through `#[path]` in `src/lib.rs`, and integration tests through `mod support;`.
//! Each includer uses only part of it, so dead code is allowed here.
//!
//! [`fake_rpc`] and [`FakeNft`] stand in for the node and for nft in the tests that run the built binary.

#![allow(dead_code)]
// test helpers build fixtures with std::fs and serve a loopback listener directly
#![allow(clippy::disallowed_methods)]

pub mod fake_rpc;
pub mod rng;

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A fresh directory, mode 0700, removed with everything in it when dropped.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a new empty directory under the system temporary directory, its name starting with `tag`.
    pub fn new(tag: &str) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let mut last = None;
        // the counter makes names unique within the process and the pid across processes,
        // so a collision means a leftover of a killed run and the next name is tried
        for _ in 0..8 {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "tn-p2p-allow-sync-{tag}-{}-{n}-{nanos}",
                std::process::id()
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    // the umask may have cleared bits the tests rely on
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no temporary directory name was free")))
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The path of `name` inside the directory.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // a test that made the directory unreadable must not leave it behind
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The fake nft script, `tests/support/fake-nft.sh`, as the checkout holds it.
pub const FAKE_NFT_SOURCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/fake-nft.sh");

/// The fake nft, linked from a directory of its own, with the files the script keeps next to itself.
///
/// The script documents its files; this type reads them and sets its knobs.
/// It is a symlink to the checkout's script, never a copy: a sibling test that spawns a process while a copy is open for writing leaves the child holding that write descriptor until its exec, and running the copy then fails with "Text file busy".
/// The script finds its state through `$0`, the link, so the state stays per test.
#[derive(Debug)]
pub struct FakeNft {
    dir: PathBuf,
}

impl FakeNft {
    /// Links the script from a new directory `nft` under `parent`, mode 0700.
    pub fn install(parent: &Path) -> io::Result<Self> {
        let dir = parent.join("nft");
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        // no chmod: it would follow the link to the checkout's file, which git keeps executable
        std::os::unix::fs::symlink(FAKE_NFT_SOURCE, dir.join("fake-nft.sh"))?;
        Ok(Self { dir })
    }

    /// The script, the value of `--nft`.
    pub fn path(&self) -> PathBuf {
        self.dir.join("fake-nft.sh")
    }

    fn state(&self, name: &str) -> PathBuf {
        self.dir.join("state").join(name)
    }

    /// Every call so far, one line of space-separated arguments each; none before the first call.
    pub fn argv(&self) -> io::Result<Vec<String>> {
        Ok(read_optional(&self.state("argv.log"))?
            .map(|log| log.lines().map(str::to_owned).collect())
            .unwrap_or_default())
    }

    /// What every `-f`, `-f -` and `-c -f -` call was given, in call order.
    pub fn loaded(&self) -> io::Result<Vec<String>> {
        let mut loaded = Vec::new();
        for n in 1_u32.. {
            match read_optional(&self.state(&format!("loaded.{n}.nft")))? {
                Some(text) => loaded.push(text),
                None => break,
            }
        }
        Ok(loaded)
    }

    /// The live table, one object per line, or `None` when no table is loaded.
    pub fn table(&self) -> io::Result<Option<String>> {
        read_optional(&self.state("table"))
    }

    /// The file that holds the live table, for a deletion that must happen at a chosen moment.
    pub fn table_path(&self) -> PathBuf {
        self.state("table")
    }

    /// Replaces the live table, as an operator's hand edit would.
    pub fn set_table(&self, text: &str) -> io::Result<()> {
        fs::write(self.state("table"), text)
    }

    /// Removes the live table, as `nft delete table` or a ruleset flush would.
    pub fn delete_table(&self) -> io::Result<()> {
        fs::remove_file(self.state("table"))
    }

    /// Sets the knob `name`, an empty file in the script's `state/`; the header of `fake-nft.sh` gives the exact messages.
    ///
    /// - `fail-next`: the next `-f` exits 1, then the knob is gone.
    /// - `eperm`: every call prints nft's "Operation not permitted" and exits 1.
    /// - `list-fails`: while the table exists, every `-j list table` exits 1 with a netlink error, while `-j list tables` still lists the table.
    /// - `list-garbage`: while the table exists, every `-j list table` prints the listing cut off right after `"flags": `, exits 0 and leaves stderr empty, as nft 1.0.x lists a dormant table; `list-fails` wins over it.
    /// - `check-fails`: the next `-c -f -` records all of stdin, then exits 1 with a syntax error, then the knob is gone.
    /// - `dormant`: both `-j list` calls give the table `"flags": "dormant"`, as nft 1.1.3 lists a dormant table, until a `-f` loads and removes the knob.
    ///
    /// The others stay until [`FakeNft::clear_knob`]. `tables-fail-nth` holds a count; see [`FakeNft::knob_with`].
    pub fn knob(&self, name: &str) -> io::Result<()> {
        self.knob_with(name, "")
    }

    /// Sets the knob `name` holding `contents`.
    ///
    /// - `tables-fail-nth`: a count N from 1; the Nth `-j list tables inet` from now exits 1 with a netlink error and removes the knob, and the calls before and after it list as usual.
    ///   A sync run lists the tables once after reading its inputs and once more on the stale path, so `"2"` fails only the stale path's listing; `fake-nft.sh` gives the whole order.
    pub fn knob_with(&self, name: &str, contents: &str) -> io::Result<()> {
        fs::create_dir_all(self.dir.join("state"))?;
        fs::write(self.state(name), contents)
    }

    /// Whether the knob `name` is set, so a test sees a one-shot knob used or `dormant` woken.
    pub fn has_knob(&self, name: &str) -> bool {
        self.state(name).exists()
    }

    /// Removes the knob `name`; a knob that is not set is no error.
    pub fn clear_knob(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.state(name)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// The text of `path`, or `None` when it does not exist.
pub fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
