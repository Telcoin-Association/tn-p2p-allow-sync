//! Helpers shared by the tests, std only: the crate has no dev-dependencies.
//!
//! The library's unit tests include this file through `#[path]` in `src/lib.rs`, and integration tests through `mod support;`.
//! Each includer uses only part of it, so dead code is allowed here.

#![allow(dead_code)]

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
