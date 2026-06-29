//! Single-instance lock, so two copies don't fight over the same caches/DBs.

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::config;

/// A held single-instance lock; removed on drop.
pub struct InstanceLock {
    path: PathBuf,
}

impl InstanceLock {
    /// Acquire the lock, or fail if another live instance holds it.
    pub fn acquire() -> Result<Self> {
        let path = config::data_dir().join("mailsweep.lock");
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(pid) = contents.trim().parse::<i32>() {
                if pid != std::process::id() as i32 && process_alive(pid) {
                    bail!("another mailsweep instance is already running (pid {pid})");
                }
            }
        }
        std::fs::create_dir_all(config::data_dir()).ok();
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(InstanceLock { path })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(unix))]
fn process_alive(_pid: i32) -> bool {
    // Conservative: assume a recorded PID is still live on non-Linux.
    true
}
