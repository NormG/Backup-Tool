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
        if raw.trim().is_empty() {
            anyhow::bail!("pending-full.toml exists but is empty");
        }
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

/// Fallback marker when pending-full.toml cannot be written.
fn retry_marker_path() -> PathBuf {
    Config::data_dir().join("pending-full.retry")
}

fn set_retry_marker(pending: bool) -> Result<()> {
    let path = retry_marker_path();
    if pending {
        fs::create_dir_all(Config::data_dir())?;
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("creating retry marker {}", path.display()))?;
        Ok(())
    } else if path.exists() {
        fs::remove_file(&path).with_context(|| format!("removing retry marker {}", path.display()))
    } else {
        Ok(())
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

/// Pending flag for auto mode. Uses retry marker as fallback when toml is missing
/// or unreadable so automatic backups keep running without losing deferred retries.
pub fn pending_full_for_auto() -> Result<bool> {
    if retry_marker_path().exists() {
        return Ok(true);
    }
    if !PendingFullState::path().exists() {
        return Ok(false);
    }
    match pending_full_backup() {
        Ok(pending) => Ok(pending),
        Err(e) => {
            eprintln!(
                "WARNING: pending-full.toml unreadable ({e}); \
                 removing corrupt file and using normal schedule"
            );
            let _ = fs::remove_file(PendingFullState::path());
            Ok(false)
        }
    }
}

const PERSIST_ATTEMPTS: u32 = 3;

fn persist_pending(pending: bool) -> Result<()> {
    with_lock(|| {
        if pending {
            let mut state = PendingFullState::load().unwrap_or_default();
            state.pending_full_backup = true;
            state.save()?;
        } else {
            PendingFullState::default().save()?;
        }
        set_retry_marker(pending)
    })
}

/// Persist deferred-full state with brief retries and a marker-file fallback.
pub fn set_pending_full_backup_with_retry(pending: bool) -> Result<()> {
    let mut last_err = None;
    for attempt in 0..PERSIST_ATTEMPTS {
        match persist_pending(pending) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < PERSIST_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }

    if pending {
        set_retry_marker(true).with_context(|| {
            format!(
                "pending-full.toml persist failed after {PERSIST_ATTEMPTS} attempts: {}",
                last_err.expect("retry loop always records at least one error")
            )
        })
    } else {
        Err(last_err.expect("retry loop always records at least one error"))
    }
}

pub fn set_pending_full_backup(pending: bool) -> Result<()> {
    set_pending_full_backup_with_retry(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pending_state_is_false() {
        assert!(!PendingFullState::default().pending_full_backup);
    }
}
