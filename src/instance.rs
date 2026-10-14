//! Single-instance lock: `state\shard.lock` held open with no sharing, pid in `state\shard.pid`.

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::paths::Paths;

const ERROR_SHARING_VIOLATION: i32 = 32;

pub struct InstanceLock {
    _file: File,
    pid_file: PathBuf,
}

pub enum Acquire {
    Acquired(InstanceLock),
    /// Another Shard holds the lock; its pid if it could be read.
    AlreadyRunning(Option<u32>),
}

pub fn acquire(paths: &Paths) -> Result<Acquire> {
    std::fs::create_dir_all(&paths.state)
        .with_context(|| format!("creating {}", paths.state.display()))?;
    let pid_file = paths.state.join("shard.pid");
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(paths.lock_file())
    {
        Ok(file) => {
            std::fs::write(&pid_file, std::process::id().to_string())
                .context("writing state\\shard.pid")?;
            Ok(Acquire::Acquired(InstanceLock {
                _file: file,
                pid_file,
            }))
        }
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
            let pid = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse().ok());
            Ok(Acquire::AlreadyRunning(pid))
        }
        Err(e) => Err(e).with_context(|| format!("opening {}", paths.lock_file().display())),
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_file);
    }
}
