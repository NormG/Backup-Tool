//! Exclusive lock held for the entire backup run to prevent overlapping GUI
//! and scheduled backups from creating duplicate snapshots.

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::PathBuf,
    time::{Duration, Instant},
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

    fn open_lock_file() -> Result<File> {
        fs::create_dir_all(Config::data_dir())?;
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(Self::path())
            .with_context(|| format!("opening run lock {}", Self::path().display()))
    }

    /// Returns `None` when another process already holds the run lock.
    pub fn try_acquire() -> Result<Option<Self>> {
        let file = Self::open_lock_file()?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e).with_context(|| "locking backup run"),
        }
    }

    /// Wait up to `timeout` for the run lock (used by scheduled auto backups).
    pub fn acquire_wait(timeout: Duration) -> Result<Option<Self>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(lock) = Self::try_acquire()? {
                return Ok(Some(lock));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_secs(5));
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
        let _guard = Config::lock_test_data_dir();
        let dir =
            std::env::temp_dir().join(format!("backup-tool-runlock-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("BACKUP_TOOL_DATA_DIR", &dir);

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

        std::env::remove_var("BACKUP_TOOL_DATA_DIR");
        let _ = fs::remove_dir_all(dir);
    }
}
