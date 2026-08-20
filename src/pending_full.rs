//! Persisted retry state when a scheduled full backup was deferred for space.

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    path::PathBuf,
};

use crate::config::Config;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct PendingFullState {
    #[serde(default)]
    pending_full_backup: bool,
}

impl PendingFullState {
    fn path() -> PathBuf {
        Config::data_dir().join("pending-full.toml")
    }

    fn lock_path() -> PathBuf {
        Self::path().with_extension("lock")
    }

    fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| "parsing pending-full.toml")
    }

    fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        let raw = toml::to_string_pretty(self).context("serialising pending-full.toml")?;
        fs::write(&tmp, raw).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("committing {}", path.display()))
    }
}

fn with_lock<R>(f: impl FnOnce() -> Result<R>) -> Result<R> {
    let lock_path = PendingFullState::lock_path();
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lock_path)
        .with_context(|| format!("opening lock {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| "locking pending-full state")?;
    let result = f();
    let _ = lock.unlock();
    result
}

pub fn pending_full_backup() -> Result<bool> {
    with_lock(|| Ok(PendingFullState::load()?.pending_full_backup))
}

/// Pending flag for auto mode. Missing state file means no retry is queued;
/// an existing but unreadable file fails so auto mode does not guess.
pub fn pending_full_for_auto() -> Result<bool> {
    if !PendingFullState::path().exists() {
        return Ok(false);
    }
    pending_full_backup()
}

const PERSIST_ATTEMPTS: u32 = 3;

/// Persist deferred-full state with brief retries for transient I/O errors.
pub fn set_pending_full_backup_with_retry(pending: bool) -> Result<()> {
    let mut last_err = None;
    for attempt in 0..PERSIST_ATTEMPTS {
        match set_pending_full_backup(pending) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < PERSIST_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }
    Err(last_err.expect("retry loop always records at least one error"))
}

pub fn set_pending_full_backup(pending: bool) -> Result<()> {
    with_lock(|| {
        let mut state = PendingFullState::load()?;
        if state.pending_full_backup == pending {
            return Ok(());
        }
        state.pending_full_backup = pending;
        state.save()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pending_state_is_false() {
        assert!(!PendingFullState::default().pending_full_backup);
    }
}
