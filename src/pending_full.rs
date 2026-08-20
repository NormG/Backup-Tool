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

/// Durable marker written alongside pending-full.toml.
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

fn quarantine_corrupt_toml() -> Result<()> {
    let path = PendingFullState::path();
    if !path.exists() {
        return Ok(());
    }
    let bad = path.with_extension("toml.corrupt");
    if bad.exists() {
        fs::remove_file(&bad)?;
    }
    fs::rename(&path, &bad).with_context(|| format!("quarantining {}", path.display()))
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

fn persist_pending(pending: bool) -> Result<()> {
    with_lock(|| {
        if pending {
            PendingFullState {
                pending_full_backup: true,
            }
            .save()?;
            set_retry_marker(true)
        } else {
            set_retry_marker(false)?;
            PendingFullState {
                pending_full_backup: false,
            }
            .save()
        }
    })
}

const PERSIST_ATTEMPTS: u32 = 3;

pub fn pending_full_backup() -> Result<bool> {
    with_lock(|| Ok(PendingFullState::load()?.pending_full_backup))
}

/// Whether auto mode should retry a deferred full (toml or marker).
pub fn pending_full_for_auto() -> Result<bool> {
    if PendingFullState::path().exists() {
        match pending_full_backup() {
            Ok(false) => {
                if retry_marker_path().exists() {
                    persist_pending(true)?;
                    return Ok(true);
                }
                return Ok(false);
            }
            Ok(true) => return Ok(true),
            Err(e) => {
                quarantine_corrupt_toml()?;
                if retry_marker_path().exists() {
                    return Ok(true);
                }
                return Err(e).with_context(|| {
                    "pending-full.toml is unreadable and no retry marker exists; \
                     repair or remove the quarantined file"
                });
            }
        }
    }
    Ok(retry_marker_path().exists())
}

/// Persist deferred-full state with brief retries and marker fallback.
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

#[allow(dead_code)]
pub fn set_pending_full_backup(pending: bool) -> Result<()> {
    set_pending_full_backup_with_retry(pending)
}

/// Clear all deferred-full signals after a successful full backup.
pub fn clear_pending_after_success() -> Result<()> {
    let mut last_err = None;
    for attempt in 0..10 {
        match persist_pending(false) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < 10 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
    Err(last_err.expect("retry loop always records at least one error"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn with_temp_data<R>(f: impl FnOnce(&PathBuf) -> R) -> R {
        let _guard = Config::lock_test_data_dir();
        let dir = std::env::temp_dir().join(format!(
            "backup-tool-pending-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("BACKUP_TOOL_DATA_DIR", &dir);
        let result = f(&dir);
        std::env::remove_var("BACKUP_TOOL_DATA_DIR");
        let _ = fs::remove_dir_all(dir);
        result
    }

    #[test]
    fn default_pending_state_is_false() {
        assert!(!PendingFullState::default().pending_full_backup);
    }

    #[test]
    fn persist_and_read_round_trip() {
        with_temp_data(|_| {
            set_pending_full_backup_with_retry(true).unwrap();
            assert!(pending_full_for_auto().unwrap());
            clear_pending_after_success().unwrap();
            assert!(!pending_full_for_auto().unwrap());
        });
    }

    #[test]
    fn marker_survives_corrupt_toml() {
        with_temp_data(|_| {
            fs::write(PendingFullState::path(), "not valid toml {{{").unwrap();
            set_retry_marker(true).unwrap();
            assert!(pending_full_for_auto().unwrap());
        });
    }

    #[test]
    fn stale_marker_cleared_when_toml_is_false() {
        with_temp_data(|_| {
            clear_pending_after_success().unwrap();
            assert!(!pending_full_for_auto().unwrap());
        });
    }

    #[test]
    fn marker_fallback_reconciles_stale_false_toml() {
        with_temp_data(|_| {
            PendingFullState {
                pending_full_backup: false,
            }
            .save()
            .unwrap();
            set_retry_marker(true).unwrap();
            assert!(pending_full_for_auto().unwrap());
            assert!(pending_full_backup().unwrap());
        });
    }
}
