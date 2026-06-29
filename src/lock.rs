//! Single-instance lock, so two copies don't fight over the same caches/DBs.
//!
//! Uses an advisory file lock (cross-platform: Linux/macOS/Windows). The lock
//! is released automatically when the process exits, even on a crash.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use fs2::FileExt;

use crate::config;

/// A held single-instance lock; released when dropped (or on process exit).
pub struct InstanceLock {
    _file: File,
    _path: PathBuf,
}

impl InstanceLock {
    /// Acquire the lock, or fail if another live instance holds it.
    pub fn acquire() -> Result<Self> {
        let dir = config::data_dir();
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("mailsweep.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening lock file {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(InstanceLock {
                _file: file,
                _path: path,
            }),
            Err(_) => bail!("another mailsweep instance is already running"),
        }
    }
}
