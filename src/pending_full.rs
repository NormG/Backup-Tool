//! Persisted retry state when a scheduled full backup was deferred for space.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

    fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| "parsing pending-full.toml")
    }

    fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self).context("serialising pending-full.toml")?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))
    }
}

pub fn pending_full_backup() -> Result<bool> {
    Ok(PendingFullState::load()?.pending_full_backup)
}

/// Like [`pending_full_backup`], but returns `true` on I/O or parse errors so
/// auto mode retries a full instead of silently dropping a deferred retry.
pub fn pending_full_backup_or_assume_pending() -> bool {
    match pending_full_backup() {
        Ok(pending) => pending,
        Err(e) => {
            eprintln!(
                "WARNING: could not read pending-full.toml ({e}); \
                 assuming deferred full is still pending"
            );
            true
        }
    }
}

pub fn set_pending_full_backup(pending: bool) -> Result<()> {
    let mut state = PendingFullState::load()?;
    if state.pending_full_backup == pending {
        return Ok(());
    }
    state.pending_full_backup = pending;
    state.save()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pending_state_is_false() {
        assert!(!PendingFullState::default().pending_full_backup);
    }
}
