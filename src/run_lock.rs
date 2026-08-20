//! Exclusive lock held for the entire backup run to prevent overlapping GUI
//! and scheduled backups from creating duplicate snapshots.

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::PathBuf,
};

use crate::config::Config;

/// Held for the duration of [`backup::run`](crate::backup::run); released on drop.
pub struct RunLock {
    _file: File,
}

impl RunLock {
    fn path() -> PathBuf {
        Config::data_dir().join("backup.run.lock")
    }

    /// Returns `None` when another process already holds the run lock.
    pub fn try_acquire() -> Result<Option<Self>> {
        fs::create_dir_all(Config::data_dir())?;
        let path = Self::path();
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("opening run lock {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e).with_context(|| "locking backup run"),
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_first_held() {
        let first = RunLock::try_acquire()
            .unwrap()
            .expect("first acquire should succeed");
        assert!(
            RunLock::try_acquire().unwrap().is_none(),
            "second acquire should be blocked"
        );
        drop(first);
        assert!(
            RunLock::try_acquire().unwrap().is_some(),
            "lock should be available after drop"
        );
    }
}
